use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use flume::{Receiver, Sender};
use maki_agent::session::Resumed;
use maki_agent::tools::ToolRegistry;
use maki_agent::{
    AgentEvent, CancelToken, Envelope, MOVED_CALL_NOTIFICATION, ModelSlot, ToolDoneEvent,
};
use maki_lua::{PluginHost, SessionEndReason, UiAction};
use maki_providers::provider::{BoxFuture, Provider};
use maki_providers::{
    AgentError, ContentBlock, Message, Model, ModelInfo, ProviderEvent, RequestOptions, Role,
    StopReason, StreamResponse, TokenUsage,
};
use maki_storage::id::SessionRef;
use serde_json::{Map, Value, json};
use test_case::test_case;

use super::super::shared_queue::{QueueItem, QueuedInput};
use super::{AgentHandles, spawn_with_lua};
use crate::app::{App, Msg, moved_note};
use crate::components::{Action, DisplayRole};

const TIMEOUT: Duration = Duration::from_secs(5);
const PLUGIN: &str = "task_e2e";
const TASK: &str = "task";
const BATCH: &str = "batch";
const ROOT_PROMPT: &str = "delegate work";
const ROOT_DONE: &str = "delegation accepted";
const CALL_ID: &str = "delegation-call";
const CHILD_ONE: &str = "first delegated prompt";
const CHILD_TWO: &str = "second delegated prompt";
const NAME_ONE: &str = "first delegated task";
const NAME_TWO: &str = "second delegated task";
const RESULT: &str = "delegated result";
const RESULT_LINES: usize = 40;
const WAIT_READY: &str = "delegation wait ready";
const CANCEL_ACK: &str = "delegation cancellation handled: ";
const CONTROL: &str = "prepare_delegation";
const BACKGROUND: &str = "background";
const DONE_STATUS: &str = "done";
const WORKING_STATUS: &str = "working";
const STARTED: &str = "Background task started";
const MOVED_TASK_RECEIPT: &str = "The user moved this task to the background. Its result arrives as a notification; do not wait for it. Continue with other work, or end your turn.";
const EVENT_MISSING: &str = "the delegation event did not reach the UI";
const GATE_CLOSED: &str = "the delegated provider lost its barrier";
const WAKE_MISSING: &str = "the delegated result did not wake its parent";
const OPEN_MISSING: &str = "the child preparation did not reach its barrier";

static REGISTRY_TESTS: Mutex<()> = Mutex::new(());

struct DelegationProvider {
    next: Mutex<Option<(String, &'static str, Value)>>,
    gates: [Receiver<()>; 2],
    started: Sender<String>,
}

impl Provider for DelegationProvider {
    fn stream_message<'a>(
        &'a self,
        _: &'a Model,
        messages: &'a [Message],
        _: &'a str,
        _: &'a Value,
        events: &'a Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        let child = [CHILD_ONE, CHILD_TWO].iter().position(|prompt| {
            messages.iter().any(|message| {
                message.role == Role::User && message.first_text_content() == Some(*prompt)
            })
        });
        Box::pin(async move {
            let (block, stop) = if let Some(index) = child {
                events
                    .send(ProviderEvent::TextDelta {
                        text: RESULT.into(),
                    })
                    .unwrap();
                self.started
                    .send([CHILD_ONE, CHILD_TWO][index].into())
                    .unwrap();
                self.gates[index].recv_async().await.expect(GATE_CLOSED);
                (
                    ContentBlock::Text {
                        text: full_result(),
                    },
                    StopReason::EndTurn,
                )
            } else if let Some((id, tool, input)) = self.next.lock().unwrap().take() {
                (ContentBlock::tool_use(id, tool, input), StopReason::ToolUse)
            } else {
                (
                    ContentBlock::Text {
                        text: ROOT_DONE.into(),
                    },
                    StopReason::EndTurn,
                )
            };
            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![block],
                    ..Default::default()
                },
                usage: TokenUsage::default(),
                stop_reason: Some(stop),
                input_transformations: Vec::new(),
            })
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

fn full_result() -> String {
    format!("{RESULT}\n").repeat(RESULT_LINES)
}

struct DelegationUi {
    app: App,
    handles: AgentHandles,
    host: PluginHost,
    provider: Arc<DelegationProvider>,
    started: Receiver<String>,
    release: [Sender<()>; 2],
    receipts: Vec<ToolDoneEvent>,
    lines: Vec<String>,
    // Release the registry only after the agent handles and Lua host drop.
    _registration_window: MutexGuard<'static, ()>,
}

impl DelegationUi {
    fn new(capacity: usize) -> Self {
        let registration_window = REGISTRY_TESTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let registry = Arc::clone(ToolRegistry::global_arc());
        let host = PluginHost::new(registry).unwrap();
        host.load_source_with_opts(
            PLUGIN,
            &format!(
                r#"
local pause, pause_finish = false, false
maki.agent.resolve_model = function()
  if pause then pause = false; maki.ui.input() end
  return {{}}
end
maki.api.register_tool({{
  name = "{CONTROL}", description = "Park the next delegation open",
  schema = {{ type = "object", properties = {{ after_finish = {{ type = "boolean" }} }} }},
  handler = function(input)
    if input.after_finish then pause_finish = true else pause = true end
    return "ready"
  end,
}})
local native_register = maki.api.register_tool
maki.api.register_tool = function(spec)
  if spec.name == "task" then
    local handler = spec.handler
    spec.handler = function(input, ctx)
      local reply = handler(input, ctx)
      if pause_finish then pause_finish = false; maki.ui.input() end
      return reply
    end
  end
  return native_register(spec)
end
local native_await = maki.async.await
local native_on_cancel = maki.async.on_cancel
maki.async.on_cancel = function(callback)
  return native_on_cancel(function(reason)
    callback(reason)
    maki.ui.flash("{CANCEL_ACK}" .. reason)
  end)
end
maki.async.await = function(n, register, ...)
  return native_await(n, function(...)
    local results = table.pack(register(...))
    maki.ui.flash("{WAIT_READY}")
    return table.unpack(results, 1, results.n)
  end, ...)
end
{}
"#,
                include_str!("../../../../plugins/task/init.lua")
            ),
            Map::from_iter([
                ("max_concurrent".into(), json!(capacity)),
                ("allow_background".into(), json!(true)),
            ]),
        )
        .unwrap();
        host.load_source(BATCH, include_str!("../../../../plugins/batch/init.lua"))
            .unwrap();
        let (first, first_gate) = flume::unbounded();
        let (second, second_gate) = flume::unbounded();
        let (started_tx, started) = flume::unbounded();
        let provider = Arc::new(DelegationProvider {
            next: Mutex::new(None),
            gates: [first_gate, second_gate],
            started: started_tx,
        });
        let mut app = crate::app::tests::test_app();
        let model = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: crate::components::test_model(),
            provider: provider.clone(),
        }));
        let (handles, permissions) = spawn_with_lua(
            &model,
            Resumed::empty(app.state.session.id.into()),
            host.event_handle(),
        );
        permissions.set_session_yolo(Some(true));
        handles.apply_to_app(&mut app);
        app.lua_event_handle = host.event_handle();
        Self {
            app,
            handles,
            host,
            provider,
            started,
            release: [first, second],
            receipts: Vec::new(),
            lines: Vec::new(),
            _registration_window: registration_window,
        }
    }

    fn dispatch(&mut self, actions: Vec<Action>) -> Vec<usize> {
        let mut selected = Vec::new();
        for action in actions {
            match action {
                Action::SendMessage(input) => {
                    self.handles.queue.push(QueueItem::Message(QueuedInput {
                        text: input.message.clone(),
                        input: *input,
                        run_id: self.app.run_id,
                        displayed: true,
                    }))
                }
                Action::BackgroundTools { run_id } => {
                    selected.push(self.handles.background_tools(run_id))
                }
                Action::CancelAgent { run_id } => self.handles.cancel_run(run_id),
                Action::CancelSubagent { tool_use_id } => self.handles.cancel_subagent(tool_use_id),
                _ => {}
            }
        }
        selected
    }

    fn submit(&mut self, tool: &'static str, input: Value) {
        *self.provider.next.lock().unwrap() =
            Some((format!("{CALL_ID}-{}", self.app.run_id + 1), tool, input));
        self.app.update(Msg::Paste(ROOT_PROMPT.into()));
        let actions = self
            .app
            .update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        self.dispatch(actions);
    }

    fn move_key(&mut self) -> Vec<usize> {
        let actions = self.app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        )));
        self.dispatch(actions)
    }

    fn feed(&mut self, envelope: Envelope) {
        if let AgentEvent::ObservationLine { text, .. } = &envelope.event {
            self.lines.push(text.clone());
        }
        if let AgentEvent::ToolBackgrounded(done) = &envelope.event {
            self.receipts.push((**done).clone());
        }
        let actions = self.app.update(Msg::Agent(Box::new(envelope)));
        self.dispatch(actions);
    }

    fn until(&mut self, stop: impl Fn(&Envelope) -> bool) {
        loop {
            let event = self
                .handles
                .agent_rx
                .recv_timeout(TIMEOUT)
                .expect(EVENT_MISSING);
            let done = stop(&event);
            self.feed(event);
            if done {
                return;
            }
        }
    }

    fn drained(&mut self) {
        let run_id = self.app.run_id;
        self.until(|event| {
            event.run_id == run_id && matches!(event.event, AgentEvent::QueueDrained)
        });
    }

    fn registered(&self) {
        loop {
            if matches!(self.host.ui_action_rx().recv_timeout(TIMEOUT).expect(EVENT_MISSING), UiAction::Flash(text) if text == WAIT_READY)
            {
                return;
            }
        }
    }

    fn cancellation_handled(&self, reason: &str) {
        let expected = format!("{CANCEL_ACK}{reason}");
        loop {
            if matches!(self.host.ui_action_rx().recv_timeout(TIMEOUT).expect(EVENT_MISSING), UiAction::Flash(text) if text == expected)
            {
                return;
            }
        }
    }

    fn exec(&self, tool: &str, input: Value) -> String {
        let ctx = self
            .handles
            .mailbox
            .seed()
            .unwrap()
            .tool_context(CancelToken::none());
        let registered = ToolRegistry::global().get(tool).unwrap();
        let invocation = registered.tool.parse(&input).unwrap();
        smol::block_on(invocation.execute(&ctx))
            .output
            .unwrap()
            .as_text()
            .to_owned()
    }

    fn wake(&mut self) {
        let listener = self.handles.mailbox.wake_listener();
        let mut notices = self.handles.claim_mailbox_wake();
        if notices.is_empty() {
            smol::block_on(futures_lite::future::or(listener, async {
                smol::Timer::after(TIMEOUT).await;
                panic!("{WAKE_MISSING}");
            }));
            notices = self.handles.claim_mailbox_wake();
        }
        assert!(!notices.is_empty(), "{WAKE_MISSING}");
        let actions = self.app.start_mailbox_run(notices);
        self.dispatch(actions);
        self.drained();
    }

    fn assert_lines(&mut self, expected: &[String]) {
        assert_eq!(self.lines, expected);
        let chat = self.app.main_chat();
        let shown: Vec<_> = (0..chat.message_count())
            .filter_map(|index| chat.message_at(index))
            .filter(|message| message.role == DisplayRole::Notice)
            .map(|message| message.text.clone())
            .collect();
        assert_eq!(shown, expected);
        let history = self.handles.history.load();
        let stored: Vec<_> = history
            .messages
            .iter()
            .filter(|message| message.is_observation())
            .filter_map(|message| message.display_text.clone())
            .collect();
        assert_eq!(stored, expected);
    }

    fn root_answers(&self) -> usize {
        self.handles
            .history
            .load()
            .messages
            .iter()
            .filter(|message| {
                message.role == Role::Assistant && message.first_text_content() == Some(ROOT_DONE)
            })
            .count()
    }

    fn start_child(&self, name: &str, prompt: &str) {
        let mut input = task_input(name, prompt);
        input["background"] = json!(true);
        let receipt = self.exec(TASK, input);
        assert!(receipt.starts_with(STARTED), "{receipt}");
    }

    /// The `/tasks` id of the moved task named `name`.
    fn task_id(&self, name: &str) -> String {
        self.app
            .chats
            .iter()
            .find(|chat| chat.name == name)
            .and_then(|chat| chat.task_id())
            .unwrap()
            .to_string()
    }

    fn assert_moved_receipt(&self) {
        assert_eq!(self.receipts.len(), 1);
        assert_eq!(self.receipts[0].output.as_text(), MOVED_TASK_RECEIPT);
    }

    fn notice_body(&self) -> String {
        let history = self.handles.history.load();
        history
            .messages
            .iter()
            .find(|message| message.is_observation() && message.display_text.is_some())
            .and_then(|message| message.first_text_content())
            .unwrap()
            .to_owned()
    }

    fn assert_prompt_once(&self, id: &str, prompt: &str) {
        let chat = self
            .app
            .chats
            .iter()
            .find(|chat| chat.task_id().is_some_and(|task| task.as_ref() == id))
            .unwrap();
        assert!(chat.detached);
        assert_eq!(
            (0..chat.message_count())
                .filter_map(|index| chat.message_at(index))
                .filter(|message| message.role == DisplayRole::User && message.text == prompt)
                .count(),
            1
        );
    }
}

impl Drop for DelegationUi {
    fn drop(&mut self) {
        self.handles.cancel_all();
        self.host.event_handle().end_sessions_blocking(
            [self.handles.mailbox.session_id()],
            SessionEndReason::Shutdown,
        );
    }
}

fn task_input(name: &str, prompt: &str) -> Value {
    json!({ "description": name, "prompt": prompt })
}

#[test_case(false; "ready_task")]
#[test_case(true; "moving_during_preparation")]
fn ctrl_b_task_receipt_reaches_the_notice_line_and_task_row(prepare: bool) {
    let mut ui = DelegationUi::new(1);
    if prepare {
        ui.exec(CONTROL, json!({}));
    }
    ui.submit(TASK, task_input(NAME_ONE, CHILD_ONE));
    if prepare {
        let UiAction::Input { reply_tx, .. } = ui
            .host
            .ui_action_rx()
            .recv_timeout(TIMEOUT)
            .expect(OPEN_MISSING)
        else {
            panic!("{OPEN_MISSING}");
        };
        assert_eq!(ui.move_key(), [1]);
        assert_eq!(ui.move_key(), [0]);
        reply_tx.send(Ok(json!({}))).unwrap();
    } else {
        ui.registered();
        assert_eq!(ui.move_key(), [1]);
    }
    assert_eq!(ui.started.recv_timeout(TIMEOUT).unwrap(), CHILD_ONE);
    ui.drained();
    assert_eq!(ui.root_answers(), 1);
    ui.cancellation_handled(BACKGROUND);
    ui.assert_moved_receipt();
    let rows = serde_json::to_value(ui.app.tasks()).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(rows[1]["id"], ui.task_id(NAME_ONE));
    assert_eq!(rows[1]["status"], WORKING_STATUS);
    assert!(ui.app.chats[1].detached);
    let chat = ui.app.main_chat();
    assert_eq!(
        (0..chat.message_count())
            .filter_map(|index| chat.message_at(index))
            .filter(|message| message.role == DisplayRole::Note && message.text == moved_note(1))
            .count(),
        1
    );
    ui.release[0].send(()).unwrap();
    ui.wake();
    assert_eq!(ui.root_answers(), 2);
    ui.assert_lines(&[format!("Background agent finished: {NAME_ONE}")]);
    assert_eq!(
        ui.notice_body(),
        format!("Background task {NAME_ONE} finished:\n\n{}", full_result())
    );
}

#[test_case(false; "ready_task")]
#[test_case(true; "moving_during_preparation")]
fn esc_after_ctrl_b_leaves_the_moved_task_running(prepare: bool) {
    let mut ui = DelegationUi::new(1);
    if prepare {
        ui.exec(CONTROL, json!({}));
    }
    ui.submit(TASK, task_input(NAME_ONE, CHILD_ONE));
    let reply = if prepare {
        let UiAction::Input { reply_tx, .. } = ui
            .host
            .ui_action_rx()
            .recv_timeout(TIMEOUT)
            .expect(OPEN_MISSING)
        else {
            panic!("{OPEN_MISSING}");
        };
        Some(reply_tx)
    } else {
        ui.registered();
        assert_eq!(ui.started.recv_timeout(TIMEOUT).unwrap(), CHILD_ONE);
        None
    };
    assert_eq!(ui.move_key(), [1]);
    let run_id = ui.app.run_id;
    ui.app.last_esc = Some(Instant::now());
    let actions = ui
        .app
        .update(Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert!(
        matches!(actions.as_slice(), [Action::CancelAgent { run_id: cancelled }] if *cancelled == run_id)
    );
    ui.dispatch(actions);
    // The barrier sits in model resolution, before the call registers its
    // cancel hook, so the hook only answers the move once preparation resumes.
    if let Some(reply) = reply {
        reply.send(Ok(json!({}))).unwrap();
        assert_eq!(ui.started.recv_timeout(TIMEOUT).unwrap(), CHILD_ONE);
    }
    ui.until(|event| event.run_id == run_id && matches!(event.event, AgentEvent::QueueDrained));
    ui.cancellation_handled(BACKGROUND);
    ui.assert_moved_receipt();
    ui.release[0].send(()).unwrap();
    ui.wake();
    ui.assert_lines(&[format!("Background agent finished: {NAME_ONE}")]);
    let body = ui.notice_body();
    assert!(body.contains(&full_result()), "{body}");
}

#[test_case(false; "finished_child_row_before_the_key")]
#[test_case(true; "queued_child_events_after_the_key")]
fn a_result_in_hand_completes_the_call_despite_the_key(delay_events: bool) {
    let mut ui = DelegationUi::new(1);
    ui.exec(CONTROL, json!({ "after_finish": true }));
    ui.submit(TASK, task_input(NAME_ONE, CHILD_ONE));
    ui.registered();
    assert_eq!(ui.started.recv_timeout(TIMEOUT).unwrap(), CHILD_ONE);
    ui.release[0].send(()).unwrap();
    let UiAction::Input { reply_tx, .. } = ui
        .host
        .ui_action_rx()
        .recv_timeout(TIMEOUT)
        .expect(OPEN_MISSING)
    else {
        panic!("{OPEN_MISSING}");
    };
    if !delay_events {
        ui.until(|event| matches!(event.event, AgentEvent::SubagentHistory { .. }));
    }
    assert_eq!(ui.move_key(), [1]);
    ui.cancellation_handled(BACKGROUND);
    reply_tx.send(Ok(json!({}))).unwrap();
    ui.drained();
    assert!(ui.receipts.is_empty());
    ui.assert_lines(&[]);
    assert!(ui.handles.claim_mailbox_wake().is_empty());
    let rows = serde_json::to_value(ui.app.tasks()).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(rows[1]["status"], DONE_STATUS);
    assert!(!ui.app.chats[1].detached);
    let history = ui.handles.history.load();
    let results: Vec<_> = history
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results, [full_result()]);
    let chat = ui.app.main_chat();
    assert!(
        !(0..chat.message_count())
            .filter_map(|index| chat.message_at(index))
            .any(|message| message.role == DisplayRole::Note)
    );
}

#[test_case(CHILD_TWO, NAME_TWO; "queued_worker_and_late_child_events")]
fn a_moved_queued_child_reaches_the_ui_after_its_parent_finishes(prompt: &str, name: &str) {
    let mut ui = DelegationUi::new(1);
    ui.start_child(NAME_ONE, CHILD_ONE);
    assert_eq!(ui.started.recv_timeout(TIMEOUT).unwrap(), CHILD_ONE);
    ui.submit(TASK, task_input(name, prompt));
    ui.registered();
    assert!(ui.started.is_empty());
    assert_eq!(ui.move_key(), [1]);
    ui.drained();
    ui.assert_moved_receipt();
    let id = ui.task_id(name);
    ui.cancellation_handled(BACKGROUND);
    ui.assert_prompt_once(&id, prompt);
    ui.release[0].send(()).unwrap();
    assert_eq!(ui.started.recv_timeout(TIMEOUT).unwrap(), prompt);
    ui.until(|event| {
        event.subagent.as_ref().is_some_and(|info| {
            info.parent_tool_use_id == id && info.prompt.as_deref() == Some(prompt)
        })
    });
    ui.assert_prompt_once(&id, prompt);
    ui.wake();
    ui.assert_lines(&[format!("Background agent finished: {NAME_ONE}")]);
    ui.release[1].send(()).unwrap();
    ui.wake();
    ui.assert_lines(&[
        format!("Background agent finished: {NAME_ONE}"),
        format!("Background agent finished: {name}"),
    ]);
    ui.assert_prompt_once(&id, prompt);
}

#[test_case(TASK; "nested_task_keeps_generic_batch_promotion")]
fn ctrl_b_nested_task_uses_the_batch_notice_and_one_result_line(tool: &str) {
    let mut ui = DelegationUi::new(1);
    ui.submit(
        BATCH,
        json!({ "tool_calls": [{ "tool": tool, "parameters": task_input(NAME_ONE, CHILD_ONE) }] }),
    );
    ui.registered();
    assert_eq!(ui.started.recv_timeout(TIMEOUT).unwrap(), CHILD_ONE);
    assert_eq!(ui.move_key(), [1]);
    ui.drained();
    assert_eq!(ui.receipts.len(), 1);
    let receipt: Value = serde_json::from_str(&ui.receipts[0].output.as_text()).unwrap();
    assert_eq!(receipt["tool"], BATCH);
    assert!(receipt["task_id"].is_string());
    assert_eq!(receipt["notification"], MOVED_CALL_NOTIFICATION);
    assert_eq!(ui.root_answers(), 1);
    ui.release[0].send(()).unwrap();
    ui.wake();
    ui.assert_lines(&[format!("Background tool finished: {BATCH}")]);
    assert!(ui.handles.history.load().messages.iter().any(|message| {
        message.is_observation()
            && message
                .first_text_content()
                .is_some_and(|body| body.contains(RESULT))
    }));
    assert!(ui.handles.claim_mailbox_wake().is_empty());
}
