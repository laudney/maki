//! Real task plugin and background sessions with a callback-controlled provider.
//! Model selection inherits the test provider; the session APIs stay real.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use maki_agent::template::Vars;
use maki_agent::tools::test_support::stub_seed_owned_by;
use maki_agent::tools::{RequestTools, ToolAudience, ToolContext, ToolExecResult, ToolRegistry};
use maki_agent::{
    Agent, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, AgentSeed, CancelToken,
    CancelTrigger, Envelope, EventSender, History, InputSource, ModelSlot, RunContext,
    SessionDefaults, SessionMailbox, TaskKind, ToolOutput,
};
use maki_lua::agent_autocmd::HostEvent;
use maki_lua::{PluginHost, SessionEndReason, UiAction};
use maki_providers::provider::{BoxFuture, Provider};
use maki_providers::tokens::ContextGauge;
use maki_providers::{
    AgentError, ContentBlock, Message, Model, ModelInfo, ProviderEvent, RequestOptions, Role,
    StopReason, StreamResponse, TokenUsage,
};
use maki_storage::id::{MakiId, SessionRef};
use serde_json::{Value, json};
use test_case::test_case;

const RESULT: &str = "background answer";
const LIST_TOOL: &str = "background_sessions";
const NOTICE_TOOL: &str = "next_notice";
const FIRST: &str = "first background task";
const SECOND: &str = "second background task";
const STOPPED_BY_USER_ERR: &str = "stopped by user";
const STARTED: &str = "Background task started";
const FINISHED: &str = "finished";
const FAILED: &str = "failed";
const FIRST_FINISHED_LINE: &str = "Background agent finished: first background task";
const FIRST_CANCELLED_LINE: &str = "Background agent cancelled: first background task";
const SECOND_CANCELLED_LINE: &str = "Background agent cancelled: second background task";
const TASK_FINISHED_LINE: &str = "Background agent finished: test blocking task";
const TASK_CANCELLED_LINE: &str = "Background agent cancelled: test blocking task";
const PROMPT: &str = "Find the answer";
const PLUGIN: &str = "background_task_test";
const INIT: &str = include_str!("../../plugins/task/init.lua");
const RUNNER: &str = include_str!("../../plugins/task/runner.lua");
const BACKGROUND: &str = include_str!("../../plugins/task/background.lua");
const BATCH: &str = include_str!("../../plugins/batch/init.lua");
const TIMEOUT: Duration = Duration::from_secs(5);
const ROOT_PROMPT: &str = "Run the selected delegation tool";
const ROOT_DONE: &str = "root completed";
const ROOT_CALL: &str = "root-delegation";
const ROOT_RUN: u64 = 51;
const TASK_NAME: &str = "test blocking task";
const MOVED_TASK_RECEIPT: &str = "The user moved this task to the background. Its result arrives as a notification; do not wait for it. Continue with other work, or end your turn.";
const WAIT_READY: &str = "delegation wait registered";
const PAUSE_OPEN: &str = "pause_task_open";
const OBSERVE_WAITS: &str = "observe_task_waits";
const EXPIRE_WAIT: &str = "expire_task_wait";
const HOLD_WAKE: &str = "hold_task_wake";
const TIMEOUT_ERR: &str = "timeout";
const CANCELLED_ERR: &str = "cancelled";
const UNKNOWN_TYPE_ERR: &str = "unknown subagent type: bogus";
const SCHEMA_ROOT_ERR: &str = "output_schema must have type object";
const OBSERVE_SLOTS: &str = "observe_task_slots";
const SLOT_REQUESTED: &str = "task worker asked for a slot";
const SLOT_GRANTED: &str = "task worker got a slot";
const PARTIAL_TEXT: &str = "the child got halfway";
const INTERRUPTED_PREFIX: &str = "sub-agent interrupted (cancelled). Partial output:";

struct ParkedProvider {
    started: flume::Sender<()>,
    release: flume::Receiver<()>,
    stopped: flume::Sender<()>,
    partial: Option<&'static str>,
}

/// Signals `stopped` when a provider call ends, finished or dropped.
struct ProviderCall<'a>(&'a flume::Sender<()>);

impl Drop for ProviderCall<'_> {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

impl Provider for ParkedProvider {
    fn stream_message<'a>(
        &'a self,
        _: &'a Model,
        _: &'a [Message],
        _: &'a str,
        _: &'a Value,
        events: &'a flume::Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let _call = ProviderCall(&self.stopped);
            if let Some(text) = self.partial {
                events
                    .send(ProviderEvent::TextDelta { text: text.into() })
                    .unwrap();
                while !events.is_empty() {
                    smol::future::yield_now().await;
                }
            }
            self.started.send(()).unwrap();
            self.release.recv_async().await.unwrap();
            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: RESULT.into(),
                    }],
                    ..Default::default()
                },
                usage: TokenUsage::default(),
                stop_reason: Some(StopReason::EndTurn),
                input_transformations: Vec::new(),
            })
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

fn host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    load(&host);
    (reg, host)
}

fn load(host: &PluginHost) {
    let source = format!(
        r#"
local builtin_require = require
local modules, loaded = {{}}, {{}}
local require
require = function(name)
  if modules[name] then
    if not loaded[name] then loaded[name] = modules[name]() end
    return loaded[name]
  end
  return builtin_require(name)
end
modules.runner = function()
{RUNNER}
end
modules.background = function()
{BACKGROUND}
end
local pause_open = false
maki.agent.resolve_model = function()
  if pause_open then
    pause_open = false
    maki.ui.input()
  end
  return {{}}
end
maki.api.register_tool({{
  name = "{PAUSE_OPEN}", description = "Park the next session preparation",
  schema = {{ type = "object", properties = {{}} }},
  handler = function() pause_open = true; return "ready" end,
}})
local observe_waits = false
maki.api.register_tool({{
  name = "{OBSERVE_WAITS}", description = "Report registered task waits",
  schema = {{ type = "object", properties = {{}} }},
  handler = function() observe_waits = true; return "ready" end,
}})
local native_await = maki.async.await
local expire_wait, expiring_ctx = false, nil
maki.api.register_tool({{
  name = "{EXPIRE_WAIT}", description = "Expire the next blocking task at a barrier",
  schema = {{ type = "object", properties = {{}} }},
  handler = function() expire_wait = true; return "ready" end,
}})
local native_register = maki.api.register_tool
maki.api.register_tool = function(spec)
  if spec.name == "task" then
    local handler = spec.handler
    spec.handler = function(input, ctx)
      if expire_wait then
        expire_wait = false
        expiring_ctx = ctx
      end
      return handler(input, ctx)
    end
  end
  return native_register(spec)
end
local hold_wake = false
maki.api.register_tool({{
  name = "{HOLD_WAKE}", description = "Hold the next task wake until the test answers",
  schema = {{ type = "object", properties = {{}} }},
  handler = function() hold_wake = true; return "ready" end,
}})
maki.async.await = function(n, register, ...)
  if expiring_ctx then
    local ctx = expiring_ctx
    expiring_ctx = nil
    maki.ui.input()
    ctx:set_deadline(0)
  end
  return native_await(n, function(wake, ...)
    local deliver = wake
    if hold_wake then
      hold_wake = false
      deliver = function(...)
        maki.ui.input()
        wake(...)
      end
    end
    local results = table.pack(register(deliver, ...))
    if observe_waits then maki.ui.flash("{WAIT_READY}") end
    return table.unpack(results, 1, results.n)
  end, ...)
end
{INIT}
local runner, observe_slots = require("runner"), false
local slots = runner.semaphore
runner.semaphore = {{
  acquire = function()
    if observe_slots then maki.ui.flash("{SLOT_REQUESTED}") end
    local permit = slots:acquire()
    if observe_slots then maki.ui.flash("{SLOT_GRANTED}") end
    return permit
  end,
}}
maki.api.register_tool({{
  name = "{OBSERVE_SLOTS}", description = "Report each worker that asks for a slot",
  schema = {{ type = "object", properties = {{}} }},
  handler = function() observe_slots = true; return "ready" end,
}})
local native_notify = maki.session.notify
local sent, seen, waiter = 0, 0, nil
maki.session.notify = function(text, opts)
  local ok, err = native_notify(text, opts)
  sent = sent + 1
  if waiter then
    local done = waiter
    waiter = nil
    done()
  end
  return ok, err
end
maki.api.register_tool({{
  name = "{NOTICE_TOOL}",
  description = "wait until the plugin has sent one more notice than last time",
  schema = {{ type = "object", properties = {{}} }},
  audiences = {{ "main" }},
  handler = function()
    if sent == seen then
      maki.async.await(1, function(done) waiter = done end)
    end
    seen = seen + 1
    return "ready"
  end,
}})
maki.api.register_tool({{
  name = "{LIST_TOOL}",
  description = "list test background sessions",
  schema = {{ type = "object", properties = {{}} }},
  audiences = {{ "main" }},
  handler = function(_, ctx)
    return maki.json.encode(maki.agent.list({{ session = ctx:session_id() }}))
  end,
}})
"#
    );
    host.load_source_with_opts(
        PLUGIN,
        &source,
        serde_json::Map::from_iter([
            ("allow_background".into(), json!(true)),
            ("max_concurrent".into(), json!(1)),
        ]),
    )
    .unwrap();
}

fn seed(
    mailbox: &SessionMailbox,
    session: MakiId,
) -> (Arc<AgentSeed>, flume::Receiver<()>, flume::Sender<()>) {
    owned_seed(mailbox, session, CancelToken::none())
}

fn owned_seed(
    mailbox: &SessionMailbox,
    session: MakiId,
    generation: CancelToken,
) -> (Arc<AgentSeed>, flume::Receiver<()>, flume::Sender<()>) {
    let (seed, started, release, _) = parked_seed(mailbox, session, generation);
    (seed, started, release)
}

/// [`owned_seed`] that also reports each provider call that ends.
fn parked_seed(
    mailbox: &SessionMailbox,
    session: MakiId,
    generation: CancelToken,
) -> (
    Arc<AgentSeed>,
    flume::Receiver<()>,
    flume::Sender<()>,
    flume::Receiver<()>,
) {
    streaming_seed(mailbox, session, generation, None)
}

/// [`parked_seed`] whose provider streams `partial` before it parks.
fn streaming_seed(
    mailbox: &SessionMailbox,
    session: MakiId,
    generation: CancelToken,
    partial: Option<&'static str>,
) -> (
    Arc<AgentSeed>,
    flume::Receiver<()>,
    flume::Sender<()>,
    flume::Receiver<()>,
) {
    let seed = mailbox.publish_seed(stub_seed_owned_by(session, generation));
    let (started, started_rx) = flume::bounded(1);
    let (release, release_rx) = flume::bounded(1);
    let (stopped, stopped_rx) = flume::bounded(1);
    seed.model_slot.store(Arc::new(ModelSlot {
        model: seed.model_slot.load().model.clone(),
        provider: Arc::new(ParkedProvider {
            started,
            release: release_rx,
            stopped,
            partial,
        }),
    }));
    (seed, started_rx, release, stopped_rx)
}

fn exec(reg: &ToolRegistry, name: &str, input: Value, ctx: &ToolContext) -> ToolExecResult {
    let inv = reg.get(name).unwrap().tool.parse(&input).unwrap();
    smol::block_on(inv.execute(ctx))
}

fn text(result: ToolExecResult) -> (String, bool) {
    let output = match result.output {
        Ok(output) => output,
        Err(error) => return (error, true),
    };
    let output = match output {
        ToolOutput::Plain(s) | ToolOutput::Markdown(s) => s.text,
        other => panic!("unexpected output {other:?}"),
    };
    (output, false)
}

fn start(reg: &ToolRegistry, ctx: &ToolContext, name: &str) {
    let (output, failed) = text(exec(
        reg,
        "task",
        json!({ "description": name, "prompt": PROMPT, "background": true }),
        ctx,
    ));
    assert!(!failed, "{output}");
    assert!(output.starts_with(STARTED), "{output}");
}

fn sessions(reg: &ToolRegistry, ctx: &ToolContext) -> Vec<Value> {
    let (output, failed) = text(exec(reg, LIST_TOOL, json!({}), ctx));
    assert!(!failed, "{output}");
    match serde_json::from_str(&output).unwrap() {
        Value::Array(tasks) => tasks,
        Value::Object(tasks) if tasks.is_empty() => Vec::new(),
        other => panic!("unexpected session list: {other}"),
    }
}

fn session_id<'a>(tasks: &'a [Value], name: &str) -> &'a str {
    tasks
        .iter()
        .find(|task| task["name"] == name)
        .and_then(|task| task["id"].as_str())
        .unwrap()
}

/// The next notice the plugin sends, its transcript line, and whether it
/// woke the chat.
fn next_notice(
    reg: &ToolRegistry,
    ctx: &ToolContext,
    mailbox: &SessionMailbox,
) -> (String, Option<String>, bool) {
    let (output, failed) = text(exec(reg, NOTICE_TOOL, json!({}), ctx));
    assert!(!failed, "{output}");
    let woken = mailbox.claim_wake();
    let woke = !woken.is_empty();
    let mut notices = if woke { woken } else { mailbox.drain() };
    assert_eq!(notices.len(), 1);
    let notice = notices.remove(0).message;
    let body = notice.first_text_content().unwrap().to_owned();
    (body, notice.display_text, woke)
}

/// What `maki.task.stop` does: cancel natively, then tell the owner.
fn stop(host: &PluginHost, session: MakiId, id: &str) {
    host.event_handle().cancel_plugin_session(id);
    fire_task_stopped(host, session, id);
}

fn fire_task_stopped(host: &PluginHost, session: MakiId, id: &str) {
    host.event_handle().fire(
        Some(&session),
        HostEvent::TaskStopped {
            id,
            plugin: Some(PLUGIN),
        },
    );
}

#[test]
fn background_result_outlives_the_calling_turn_and_wakes_the_chat() {
    let (reg, _host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release) = seed(&mailbox, session);
    let (foreground, token) = CancelToken::new();
    let ctx = seed.tool_context(token);
    start(&reg, &ctx, FIRST);
    started.recv().unwrap();
    foreground.cancel();
    release.send(()).unwrap();
    let (text, line, woke) = next_notice(&reg, &ctx, &mailbox);
    assert!(woke);
    assert!(text.contains(FIRST), "{text}");
    assert!(text.contains(FINISHED), "{text}");
    assert!(text.contains(RESULT), "{text}");
    assert_eq!(line.as_deref(), Some(FIRST_FINISHED_LINE));
    assert!(sessions(&reg, &ctx).is_empty());
}

#[test]
fn user_stop_of_a_running_task_queues_a_failure_and_frees_its_slot() {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release) = seed(&mailbox, session);
    let ctx = seed.tool_context(CancelToken::none());
    start(&reg, &ctx, FIRST);
    started.recv().unwrap();
    stop(&host, session, session_id(&sessions(&reg, &ctx), FIRST));
    let (text, line, woke) = next_notice(&reg, &ctx, &mailbox);
    assert!(!woke, "user stop woke the chat");
    assert!(text.contains(STOPPED_BY_USER_ERR), "{text}");
    assert!(text.contains(FAILED), "{text}");
    assert_eq!(line.as_deref(), Some(FIRST_CANCELLED_LINE));
    start(&reg, &ctx, SECOND);
    started.recv().unwrap();
    release.send(()).unwrap();
    assert!(next_notice(&reg, &ctx, &mailbox).0.contains(SECOND));
}

#[test]
fn user_stop_of_a_queued_task_skips_the_provider() {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release) = seed(&mailbox, session);
    let ctx = seed.tool_context(CancelToken::none());
    start(&reg, &ctx, FIRST);
    started.recv().unwrap();
    start(&reg, &ctx, SECOND);
    stop(&host, session, session_id(&sessions(&reg, &ctx), SECOND));
    let (text, line, woke) = next_notice(&reg, &ctx, &mailbox);
    assert!(!woke, "user stop woke the chat");
    assert!(text.contains(SECOND), "{text}");
    assert!(text.contains(STOPPED_BY_USER_ERR), "{text}");
    assert_eq!(line.as_deref(), Some(SECOND_CANCELLED_LINE));
    release.send(()).unwrap();
    assert!(next_notice(&reg, &ctx, &mailbox).0.contains(FIRST));
    assert!(started.try_recv().is_err());
    assert!(sessions(&reg, &ctx).is_empty());
}

/// Any plugin can fire `TaskStopped`, so the event alone must not stop work.
#[test]
fn a_spoofed_stop_event_leaves_a_queued_task_alone() {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release) = seed(&mailbox, session);
    let ctx = seed.tool_context(CancelToken::none());
    start(&reg, &ctx, FIRST);
    started.recv().unwrap();
    start(&reg, &ctx, SECOND);
    fire_task_stopped(&host, session, session_id(&sessions(&reg, &ctx), SECOND));
    release.send(()).unwrap();
    assert!(next_notice(&reg, &ctx, &mailbox).0.contains(FIRST));
    started.recv().unwrap();
    release.send(()).unwrap();
    let (text, _, woke) = next_notice(&reg, &ctx, &mailbox);
    assert!(woke);
    assert!(text.contains(SECOND), "{text}");
    assert!(text.contains(RESULT), "{text}");
}

#[test_case(false ; "session_end")]
#[test_case(true ; "reload")]
fn teardown_stops_background_work_without_notifying_a_gone_chat(reload: bool) {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (generation, owner) = CancelToken::new();
    let (seed, started, _release) = owned_seed(&mailbox, session, owner);
    let ctx = seed.tool_context(CancelToken::none());
    start(&reg, &ctx, FIRST);
    started.recv().unwrap();
    start(&reg, &ctx, SECOND);
    if reload {
        host.unload(PLUGIN).unwrap();
        load(&host);
    } else {
        // A frontend ends the chat generation before its sessions.
        generation.cancel();
        host.event_handle()
            .end_sessions_blocking([session], SessionEndReason::Shutdown);
    }
    assert!(sessions(&reg, &ctx).is_empty());
    assert!(mailbox.drain().is_empty());
    assert!(started.try_recv().is_err());
}

/// Headless, SDK and ACP runs have no live chat to deliver to.
#[test]
fn background_without_a_live_chat_runs_blocking() {
    let (reg, _host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release) = seed(&mailbox, session);
    let ctx = seed.tool_context(CancelToken::none());
    drop(seed);
    drop(mailbox);
    let (done_tx, done_rx) = flume::bounded(1);
    thread::spawn(move || {
        let result = exec(
            &reg,
            "task",
            json!({ "description": FIRST, "prompt": PROMPT, "background": true }),
            &ctx,
        );
        done_tx.send(text(result)).unwrap();
    });
    started.recv().unwrap();
    assert!(done_rx.try_recv().is_err());
    release.send(()).unwrap();
    assert_eq!(done_rx.recv().unwrap(), (RESULT.into(), false));
}

/// Answers the root prompt with one call of `tool` and hands every other
/// request, the subagent's included, to `child`.
struct DelegationProvider {
    child: Arc<dyn Provider>,
    tool: &'static str,
    input: Value,
    asked: AtomicBool,
}

impl Provider for DelegationProvider {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        events: &'a flume::Sender<ProviderEvent>,
        options: RequestOptions,
        session: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        if !messages.iter().any(|message| {
            message.role == Role::User && message.first_text_content() == Some(ROOT_PROMPT)
        }) {
            return self
                .child
                .stream_message(model, messages, system, tools, events, options, session);
        }
        let (block, stop_reason) = if self.asked.swap(true, Ordering::SeqCst) {
            (
                ContentBlock::Text {
                    text: ROOT_DONE.into(),
                },
                StopReason::EndTurn,
            )
        } else {
            (
                ContentBlock::tool_use(ROOT_CALL, self.tool, self.input.clone()),
                StopReason::ToolUse,
            )
        };
        Box::pin(async move {
            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![block],
                    ..Default::default()
                },
                usage: TokenUsage::default(),
                stop_reason: Some(stop_reason),
                input_transformations: Vec::new(),
            })
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// A main-agent run whose one tool call is managed, so Ctrl+B can move it.
struct ManagedCall {
    finished: flume::Receiver<History>,
    events: flume::Receiver<Envelope>,
    cancel: CancelTrigger,
}

fn managed_call(
    reg: &Arc<ToolRegistry>,
    mailbox: &SessionMailbox,
    ctx: ToolContext,
    tool: &'static str,
    input: Value,
) -> ManagedCall {
    assert!(!text(exec(reg, OBSERVE_WAITS, json!({}), &ctx)).1);
    let (cancel, token) = CancelToken::new();
    let (tx, events) = flume::unbounded();
    let event_tx =
        EventSender::new(tx, ROOT_RUN).with_owner_cancel(ctx.event_tx.owner_cancel().unwrap());
    let params = AgentParams {
        provider: Arc::new(DelegationProvider {
            child: ctx.provider,
            tool,
            input,
            asked: AtomicBool::new(false),
        }),
        model: (*ctx.model).clone(),
        config: ctx.config,
        tool_output_lines: ctx.tool_output_lines,
        permissions: ctx.permissions,
        session_id: ctx.session_id,
        background_for: None,
        task_id: None,
        running_tool: None,
        child_presentation: None,
        mailbox: Some(mailbox.clone()),
        timeouts: ctx.timeouts,
        file_access: ctx.file_access,
        prompt_slots: ctx.prompt_slots,
        subagent_cancels: ctx.subagent_cancels,
        ledger: ctx.ledger,
        registry: Arc::clone(reg),
        audience: ToolAudience::MAIN,
        model_policy: ctx.model_policy,
    };
    let registry = Arc::clone(reg);
    let (done, finished) = flume::unbounded();
    thread::spawn(move || {
        let mut history = History::new(Vec::new());
        let mut gauge = ContextGauge::default();
        smol::block_on(
            Agent::new(
                params,
                AgentRunParams {
                    history: &mut history,
                    gauge: &mut gauge,
                    event_tx,
                    context: Arc::new(move |model, workflow| {
                        RunContext::fixed(
                            String::new(),
                            RequestTools::build(
                                &registry,
                                &Vars::new(),
                                model,
                                &Default::default(),
                                &[],
                                workflow,
                                false,
                            ),
                        )
                    }),
                },
            )
            .with_cancel(token)
            .run(AgentInput::from_defaults(
                ROOT_PROMPT.into(),
                AgentMode::Build,
                Vec::new(),
                SessionDefaults::default(),
                InputSource::Tui,
            )),
        )
        .unwrap();
        done.send(history).unwrap();
    });
    ManagedCall {
        finished,
        events,
        cancel,
    }
}

fn call_output(call: &ManagedCall) -> (String, bool) {
    let history = call.finished.recv_timeout(TIMEOUT).unwrap();
    history
        .as_slice()
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } if tool_use_id == ROOT_CALL => Some((content.clone(), *is_error)),
            _ => None,
        })
        .unwrap()
}

/// The child a moved call left running, as `/tasks` names it.
fn detached_child(events: &[Envelope]) -> String {
    let children: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            AgentEvent::SubagentDetached { child } => Some(child.parent_tool_use_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(children.len(), 1);
    children[0].clone()
}

fn wait_registered(host: &PluginHost) {
    loop {
        if matches!(host.ui_action_rx().recv_timeout(TIMEOUT).unwrap(), UiAction::Flash(text) if text == WAIT_READY)
        {
            return;
        }
    }
}

fn task_input() -> Value {
    json!({ "description": TASK_NAME, "prompt": PROMPT })
}

#[test_case(false; "child_running")]
#[test_case(true; "child_still_preparing")]
fn a_moved_task_keeps_its_one_child_and_pushes_its_result(prepare: bool) {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release, stopped) = parked_seed(&mailbox, session, CancelToken::none());
    let ctx = seed.tool_context(CancelToken::none());
    if prepare {
        assert!(!text(exec(&reg, PAUSE_OPEN, json!({}), &ctx)).1);
    }
    let call = managed_call(&reg, &mailbox, ctx.clone(), "task", task_input());
    if prepare {
        let UiAction::Input { reply_tx, .. } = host.ui_action_rx().recv_timeout(TIMEOUT).unwrap()
        else {
            panic!("session preparation did not reach its barrier");
        };
        assert_eq!(mailbox.background_tools(ROOT_RUN), 1);
        assert_eq!(mailbox.background_tools(ROOT_RUN), 0);
        reply_tx.send(Ok(json!({}))).unwrap();
        started.recv_timeout(TIMEOUT).unwrap();
    } else {
        started.recv_timeout(TIMEOUT).unwrap();
        wait_registered(&host);
        assert_eq!(mailbox.background_tools(ROOT_RUN), 1);
    }
    assert_eq!(call_output(&call), (MOVED_TASK_RECEIPT.into(), false));
    let events: Vec<_> = call.events.drain().collect();
    let child = detached_child(&events);
    assert!(events.iter().any(|event| matches!(
        &event.event, AgentEvent::SubagentDetached { child: info }
        if info.parent_tool_use_id == child && info.prompt.as_deref() == Some(PROMPT)
    )));
    assert!(!events.iter().any(|event| {
        event
            .subagent
            .as_ref()
            .is_some_and(|info| info.kind == TaskKind::Tool)
    }));
    assert!(
        !events.iter().any(
            |event| matches!(&event.event, AgentEvent::ToolDone(done) if done.id == ROOT_CALL)
        )
    );
    call.cancel.cancel();
    assert!(stopped.is_empty());
    release.send(()).unwrap();
    let (text, line, woke) = next_notice(&reg, &ctx, &mailbox);
    assert!(woke);
    assert_eq!(
        text,
        format!("Background task {TASK_NAME} {FINISHED}:\n\n{RESULT}")
    );
    assert_eq!(line.as_deref(), Some(TASK_FINISHED_LINE));
    stopped.recv_timeout(TIMEOUT).unwrap();
    assert!(started.is_empty());
    assert!(mailbox.drain().is_empty());
}

/// The child's result is in hand but the waiting call has not returned it
/// when the move lands, so the call completes with it and nothing moves.
#[test]
fn a_result_in_hand_before_a_move_completes_the_call_once() {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release, stopped) = parked_seed(&mailbox, session, CancelToken::none());
    let ctx = seed.tool_context(CancelToken::none());
    assert!(!text(exec(&reg, HOLD_WAKE, json!({}), &ctx)).1);
    let call = managed_call(&reg, &mailbox, ctx, "task", task_input());
    started.recv_timeout(TIMEOUT).unwrap();
    wait_registered(&host);
    release.send(()).unwrap();
    stopped.recv_timeout(TIMEOUT).unwrap();
    let UiAction::Input { reply_tx, .. } = host.ui_action_rx().recv_timeout(TIMEOUT).unwrap()
    else {
        panic!("the result did not reach the held wake");
    };
    assert_eq!(mailbox.background_tools(ROOT_RUN), 1);
    reply_tx.send(Ok(json!({}))).unwrap();
    assert_eq!(call_output(&call), (RESULT.into(), false));
    let events: Vec<_> = call.events.drain().collect();
    assert!(!events.iter().any(|event| matches!(
        &event.event,
        AgentEvent::ToolBackgrounded(_) | AgentEvent::SubagentDetached { .. }
    )));
    assert!(events.iter().any(|event| matches!(
        &event.event,
        AgentEvent::BackgroundMoveSettled { count: 0, .. }
    )));
    assert!(mailbox.drain().is_empty());
}

enum MovedEnd {
    UserStop,
    Generation,
    Reload,
}

#[test_case(MovedEnd::UserStop; "user_stop")]
#[test_case(MovedEnd::Generation; "generation_teardown")]
#[test_case(MovedEnd::Reload; "plugin_reload")]
fn a_moved_task_ends_like_any_background_task(end: MovedEnd) {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (generation, owner) = CancelToken::new();
    let (seed, started, _release, stopped) = parked_seed(&mailbox, session, owner);
    let ctx = seed.tool_context(CancelToken::none());
    let call = managed_call(&reg, &mailbox, ctx.clone(), "task", task_input());
    started.recv_timeout(TIMEOUT).unwrap();
    wait_registered(&host);
    assert_eq!(mailbox.background_tools(ROOT_RUN), 1);
    assert_eq!(call_output(&call), (MOVED_TASK_RECEIPT.into(), false));
    let child = detached_child(&call.events.drain().collect::<Vec<_>>());
    match end {
        MovedEnd::UserStop => ctx.subagent_cancels.cancel(child),
        MovedEnd::Generation => {
            generation.cancel();
            host.event_handle()
                .end_sessions_blocking([session], SessionEndReason::Shutdown);
        }
        MovedEnd::Reload => {
            host.unload(PLUGIN).unwrap();
            load(&host);
        }
    }
    stopped.recv_timeout(TIMEOUT).unwrap();
    if matches!(end, MovedEnd::UserStop) {
        let (text, line, woke) = next_notice(&reg, &ctx, &mailbox);
        assert!(!woke, "a user stop woke the chat");
        assert!(text.contains(FAILED), "{text}");
        assert_eq!(line.as_deref(), Some(TASK_CANCELLED_LINE));
    }
    assert!(mailbox.drain().is_empty());
}

/// Runs `task` on its own thread, so the test can drive the provider.
fn spawn_task(reg: &Arc<ToolRegistry>, ctx: ToolContext) -> flume::Receiver<(String, bool)> {
    let registry = Arc::clone(reg);
    let (done, finished) = flume::unbounded();
    thread::spawn(move || {
        done.send(text(exec(&registry, "task", task_input(), &ctx)))
            .unwrap()
    });
    finished
}

#[test_case(false; "no_session_direct_path")]
#[test_case(true; "session_worker_path")]
fn cancelling_a_blocking_task_stops_its_child_and_frees_its_slot(session_backed: bool) {
    let (reg, _host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release, stopped) = parked_seed(&mailbox, session, CancelToken::none());
    let mut ctx = seed.tool_context(CancelToken::none());
    if !session_backed {
        ctx.session_id = None;
    }
    let (cancel, token) = CancelToken::new();
    let mut task_ctx = ctx.clone();
    task_ctx.cancel = token;
    let finished = spawn_task(&reg, task_ctx);
    started.recv_timeout(TIMEOUT).unwrap();
    cancel.cancel();
    assert!(finished.recv_timeout(TIMEOUT).unwrap().1);
    stopped.recv_timeout(TIMEOUT).unwrap();
    assert!(mailbox.claim_wake().is_empty());
    let finished = spawn_task(&reg, ctx);
    started.recv_timeout(TIMEOUT).unwrap();
    release.send(()).unwrap();
    stopped.recv_timeout(TIMEOUT).unwrap();
    assert_eq!(
        finished.recv_timeout(TIMEOUT).unwrap(),
        (RESULT.into(), false)
    );
}

fn slot_flashes(host: &PluginHost, text: &str) -> usize {
    host.ui_action_rx()
        .try_iter()
        .filter(|action| matches!(action, UiAction::Flash(flash) if flash == text))
        .count()
}

/// A blocking task still queued for a slot has no prompt to wait on, so Esc
/// ends its call at once rather than when a slot frees, and its worker with
/// it: the next task to queue gets the slot it was waiting for.
#[test]
fn esc_on_a_queued_blocking_task_ends_the_call_and_its_worker_at_once() {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release) = seed(&mailbox, session);
    let ctx = seed.tool_context(CancelToken::none());
    assert!(!text(exec(&reg, OBSERVE_SLOTS, json!({}), &ctx)).1);
    start(&reg, &ctx, FIRST);
    started.recv_timeout(TIMEOUT).unwrap();
    let (cancel, token) = CancelToken::new();
    let mut task_ctx = ctx.clone();
    task_ctx.cancel = token;
    let finished = spawn_task(&reg, task_ctx);
    let mut requests = 0;
    while requests < 2 {
        if matches!(host.ui_action_rx().recv_timeout(TIMEOUT).unwrap(), UiAction::Flash(text) if text == SLOT_REQUESTED)
        {
            requests += 1;
        }
    }
    cancel.cancel();
    let (output, failed) = finished.recv_timeout(TIMEOUT).unwrap();
    assert_eq!((output.as_str(), failed), (CANCELLED_ERR, true));
    release.send(()).unwrap();
    assert!(next_notice(&reg, &ctx, &mailbox).0.contains(FIRST));
    start(&reg, &ctx, SECOND);
    started.recv_timeout(TIMEOUT).unwrap();
    assert_eq!(slot_flashes(&host, SLOT_GRANTED), 1);
}

/// Esc on a blocking task hands the call what its child streamed, as when
/// the call ran the child itself.
#[test]
fn esc_on_a_blocking_task_returns_what_its_child_streamed() {
    let (reg, _host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, _release, stopped) =
        streaming_seed(&mailbox, session, CancelToken::none(), Some(PARTIAL_TEXT));
    let ctx = seed.tool_context(CancelToken::none());
    let (cancel, token) = CancelToken::new();
    let mut task_ctx = ctx;
    task_ctx.cancel = token;
    let finished = spawn_task(&reg, task_ctx);
    started.recv_timeout(TIMEOUT).unwrap();
    cancel.cancel();
    let (output, failed) = finished.recv_timeout(TIMEOUT).unwrap();
    assert!(failed);
    assert!(output.starts_with(INTERRUPTED_PREFIX), "{output}");
    assert!(output.contains(PARTIAL_TEXT), "{output}");
    stopped.recv_timeout(TIMEOUT).unwrap();
    assert!(mailbox.claim_wake().is_empty());
}

#[test_case(false; "attached_task_finishes_normally")]
#[test_case(true; "nested_task_uses_generic_batch_promotion")]
fn tasks_keep_blocking_until_completion_or_generic_batch_promotion(nested: bool) {
    let (reg, host) = host();
    host.load_source("batch", BATCH).unwrap();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release, _stopped) = parked_seed(&mailbox, session, CancelToken::none());
    let ctx = seed.tool_context(CancelToken::none());
    let (tool, input) = if nested {
        (
            "batch",
            json!({ "tool_calls": [{ "tool": "task", "parameters": task_input() }] }),
        )
    } else {
        ("task", task_input())
    };
    let call = managed_call(&reg, &mailbox, ctx, tool, input);
    started.recv_timeout(TIMEOUT).unwrap();
    wait_registered(&host);
    assert!(call.finished.is_empty());
    if nested {
        assert_eq!(mailbox.background_tools(ROOT_RUN), 1);
        let (receipt, failed) = call_output(&call);
        assert!(!failed);
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(receipt["tool"], "batch");
        let listener = mailbox.wake_listener();
        release.send(()).unwrap();
        smol::block_on(futures_lite::future::or(listener, async {
            smol::Timer::after(TIMEOUT).await;
            panic!("generic batch completion did not notify");
        }));
        let notices = mailbox.claim_wake();
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0]
                .message
                .first_text_content()
                .unwrap()
                .contains(RESULT)
        );
    } else {
        release.send(()).unwrap();
        assert_eq!(call_output(&call), (RESULT.into(), false));
        assert!(mailbox.drain().is_empty());
        assert!(
            !call
                .events
                .drain()
                .any(|event| matches!(event.event, AgentEvent::SubagentDetached { .. }))
        );
    }
}

#[test_case(false, CANCELLED_ERR; "esc_while_waiting")]
#[test_case(true, TIMEOUT_ERR; "deadline_while_waiting")]
fn an_ordinary_stop_cancels_a_running_task_without_waking_the_parent(timeout: bool, reason: &str) {
    let (reg, host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, release, stopped) = parked_seed(&mailbox, session, CancelToken::none());
    let ctx = seed.tool_context(CancelToken::none());
    if timeout {
        assert!(!text(exec(&reg, EXPIRE_WAIT, json!({}), &ctx)).1);
    }
    let (cancel, token) = CancelToken::new();
    let mut task_ctx = ctx.clone();
    task_ctx.cancel = token;
    let finished = spawn_task(&reg, task_ctx);
    if timeout {
        let UiAction::Input { reply_tx, .. } = host.ui_action_rx().recv_timeout(TIMEOUT).unwrap()
        else {
            panic!("the task wait did not reach its deadline barrier");
        };
        started.recv_timeout(TIMEOUT).unwrap();
        reply_tx.send(Ok(json!({}))).unwrap();
    } else {
        started.recv_timeout(TIMEOUT).unwrap();
        cancel.cancel();
    }
    let (output, failed) = finished.recv_timeout(TIMEOUT).unwrap();
    assert!(failed && output.contains(reason), "{output}");
    stopped.recv_timeout(TIMEOUT).unwrap();
    assert!(mailbox.claim_wake().is_empty());
    let finished = spawn_task(&reg, ctx);
    started.recv_timeout(TIMEOUT).unwrap();
    release.send(()).unwrap();
    assert_eq!(
        finished.recv_timeout(TIMEOUT).unwrap(),
        (RESULT.into(), false)
    );
}

#[test_case(json!({ "subagent_type": "bogus" }), UNKNOWN_TYPE_ERR; "invalid_type")]
#[test_case(json!({ "output_schema": { "type": "array" } }), SCHEMA_ROOT_ERR; "invalid_schema")]
fn a_failed_blocking_open_returns_its_error_without_a_receipt(extra: Value, expected: &str) {
    let (reg, _host) = host();
    let session = MakiId::generate();
    let mailbox = SessionMailbox::register(session);
    let (seed, started, _release) = seed(&mailbox, session);
    let ctx = seed.tool_context(CancelToken::none());
    let mut input = task_input();
    input
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let call = managed_call(&reg, &mailbox, ctx, "task", input);
    assert_eq!(call_output(&call), (expected.into(), true));
    assert_eq!(mailbox.background_tools(ROOT_RUN), 0);
    assert!(started.is_empty());
    assert!(mailbox.drain().is_empty());
}
