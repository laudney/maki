use arc_swap::ArcSwap;
use maki_providers::provider::BoxFuture;
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use maki_config::{ModelPolicy, ProjectConfig, SessionDefaults};
use maki_providers::model::Model;
use maki_providers::provider::{self, Provider};
use maki_providers::retry::RetryPolicy;
use maki_providers::{ContentBlock, Message, RequestOptions, Role, Timeouts, TokenUsage};
use maki_storage::StateDir;
use maki_storage::id::{MakiId, SessionRef};
use maki_storage::sessions::{QueueReceipt, QueueStatus, SessionClaim};
use serde_json::Value;
use std::mem;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::time::{sleep_until, timeout};
use tokio_util::task::AbortOnDropHandle;
use tracing::{error, warn};

use crate::agent::{self, AgentHooks, RunContext, RunContextBuilder};
use crate::cancel::CancelToken;
use crate::permissions::{PermissionManager, PluginRuleStore};
use crate::prompt::ResolvedPrompts;
use crate::session::{Resumed, SessionControl, SessionTrack, SharedSettings, StoredSession};
use crate::template;
use crate::tools::{
    BackgroundFor, FileAccess, LocalTools, RequestTools, ToolAudience, ToolRegistry,
};
use crate::types::{EventSender, error_event};
use crate::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, AgentParams, AgentSeed,
    DoneReason, Envelope, EventStreamGuard, ExtractedCommand, InputSource, InterruptSource,
    McpHandle, McpSession, ModelSlot, PermissionsConfig, PromptRole, RunLedger, RunTotals,
    SessionEvents, SessionMailbox, ToolDeferral, ToolOutputLines, event_stream,
};
use maki_types::cancel::RunCancels;

const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

const UNREPORTED_HEAD: &str = "background task result(s) never reached the model before";
const UNREPORTED_TAIL: &str = "Tasks still running are stopped, and resuming this session \
     tells the model their results are lost.";
const LAPSED_CAUSE: &str = "--background-timeout ran out";
const STOPPED_CAUSE: &str = "maki was interrupted";
const FAILED_CAUSE: &str = "the last run failed";

/// What a session does with tool calls that move off their turn.
#[derive(Clone, Debug)]
pub enum BackgroundMode {
    /// No seed is published, so nothing can move and every call blocks.
    Blocking,
    /// Results ride the next prompt. Closing the session stops the work.
    Detached,
    /// Like [`Self::Detached`], and once the input ends the session stays up
    /// to this long, running a turn for each result that wakes it.
    KeepAlive(Duration),
}

impl BackgroundMode {
    /// `None` is a zero `--background-timeout`: background work is off.
    pub fn from_wait(wait: Option<Duration>) -> Self {
        wait.map_or(Self::Blocking, Self::KeepAlive)
    }

    fn seeded(&self) -> bool {
        !matches!(self, Self::Blocking)
    }
}

/// Registered before the first run, so the receipts a stored session left
/// unsettled come back as lost notices that run reads.
fn open_mailbox(resumed: &Resumed) -> SessionMailbox {
    let mailbox = SessionMailbox::register(resumed.id.id());
    if let Some(session) = &resumed.session {
        mailbox.restore_lost(&session.meta.pending_background, session.messages());
    }
    mailbox
}

/// The registry holds the seed weakly, so the caller keeps it for the
/// session's life. Without one no call can move.
fn publish_seed(
    mailbox: &SessionMailbox,
    background: &BackgroundMode,
    seed: impl FnOnce() -> AgentSeed,
) -> Option<Arc<AgentSeed>> {
    background.seeded().then(|| mailbox.publish_seed(seed()))
}

/// Background work reads the chat's thinking and mode from the seed, and a
/// run's input is the chat's latest word on both.
fn adopt_chat_settings(seed: Option<&AgentSeed>, input: &AgentInput) {
    if let Some(seed) = seed {
        let opts = RequestOptions {
            thinking: input.thinking,
            fast: input.fast,
        };
        seed.set_chat_settings(opts, &input.mode);
    }
}

fn warn_unreported(unreported: usize, cause: &str) {
    warn!(unreported, cause, "background results unreported");
    if unreported > 0 {
        eprintln!("warning: {unreported} {UNREPORTED_HEAD} {cause}. {UNREPORTED_TAIL}");
    }
}

/// Holds a session open once its input ends while background work is out,
/// so each result can still wake the model. One deadline covers the whole
/// wait, so a chain of wakes cannot keep the process up for good.
struct KeepAlive {
    mailbox: SessionMailbox,
    /// `None` when the session ends with its input.
    wait: Option<Duration>,
    deadline: Option<Instant>,
    next: AfterInput,
}

/// What the session does once its input ends.
enum AfterInput {
    End,
    /// Wakes turns under the last run's settings.
    Wake(AgentInput),
    /// Ends, warning about what never reached the model.
    Fail,
}

impl KeepAlive {
    fn new(mailbox: SessionMailbox, background: BackgroundMode) -> Self {
        let wait = match background {
            BackgroundMode::KeepAlive(wait) => Some(wait),
            BackgroundMode::Blocking | BackgroundMode::Detached => None,
        };
        Self {
            mailbox,
            wait,
            deadline: None,
            next: AfterInput::End,
        }
    }

    fn follow(&mut self, input: &AgentInput) {
        if self.wait.is_some() {
            self.next = AfterInput::Wake(woken_input(input));
        }
    }

    /// Nobody is left to retry a run that failed once the input ends, and a
    /// woken turn would most likely fail the same way. A later prompt arms
    /// the keep-alive again.
    fn fail(&mut self) {
        if matches!(self.next, AfterInput::Wake(_)) {
            self.next = AfterInput::Fail;
        }
    }

    /// The host switched after the last run, so a woken turn runs in the
    /// mode it switched to.
    fn switch_mode(&mut self, mode: AgentMode) {
        if let AfterInput::Wake(next) = &mut self.next {
            next.mode = mode;
        }
    }

    /// The notices of the next wake, or `None` once nothing is out and
    /// nothing woke, or the deadline passed.
    async fn wake(&mut self) -> Option<Vec<Message>> {
        let wait = self.wait?;
        let deadline = *self.deadline.get_or_insert_with(|| Instant::now() + wait);
        let mailbox = &self.mailbox;
        // This also counts results that came in but would wake a turn
        // past the deadline: they die with the process all the same.
        let lapsed = || {
            warn_unreported(mailbox.unsettled().len(), LAPSED_CAUSE);
            None
        };
        // Lapsed first: a wake queued during the last turn is ready at once.
        // A deadline already behind us is checked against the clock, since
        // the timer only reports it after a trip through the driver.
        if Instant::now() >= deadline {
            return lapsed();
        }
        tokio::select! {
            biased;
            () = sleep_until(deadline.into()) => lapsed(),
            wake = mailbox.next_wake() => wake,
        }
    }

    /// The next woken turn, or `None` once the session may end. What still
    /// runs at a lapse dies with the process, and the session meta keeps its
    /// receipts.
    async fn next_input(&mut self) -> Option<AgentInput> {
        let mut input = match mem::replace(&mut self.next, AfterInput::End) {
            AfterInput::End => return None,
            AfterInput::Fail => {
                warn_unreported(self.mailbox.unsettled().len(), FAILED_CAUSE);
                return None;
            }
            AfterInput::Wake(input) => input,
        };
        input.preamble = self.wake().await?;
        Some(input)
    }
}

/// Nobody prompted this turn: the notices it is woken with are all it says.
fn woken_input(like: &AgentInput) -> AgentInput {
    AgentInput {
        message: String::new(),
        mode: like.mode.clone(),
        images: Vec::new(),
        preamble: Vec::new(),
        earlier: Vec::new(),
        thinking: like.thinking,
        fast: like.fast,
        workflow: like.workflow,
        prompt: None,
        source: like.source,
    }
}

/// Resolve a provider, or report the failure on the event stream and let the
/// caller give up. Every site that reaches a provider comes through here, so a
/// failed connect cannot be silent on one path and loud on another. What giving
/// up means is the caller's call: abandon the session, or skip this turn.
async fn connect(
    model: &mut Model,
    timeouts: Timeouts,
    event_tx: &EventSender,
) -> Option<Arc<dyn Provider>> {
    match provider::from_model_async(model, timeouts).await {
        Ok(p) => Some(Arc::from(p)),
        Err(e) => {
            error!(error = %e, "provider error");
            let _ = event_tx.send(error_event(&e, RunTotals::default()));
            None
        }
    }
}

/// What a session task ends with, before its MCP is torn down: the track
/// is out of the task whatever that teardown does, so it never drops on an
/// executor thread after the caller stopped looking.
#[derive(Default)]
pub struct SessionEnd {
    /// `None` when no provider connected. Drop it once the session's
    /// background work is stopped: that writes what the work spent after the
    /// last turn.
    pub track: Option<SessionTrack>,
    mcp: Option<McpHandle>,
}

pub type SessionTask = AbortOnDropHandle<SessionEnd>;

#[derive(Clone)]
struct ContextSpec {
    vars: template::Vars,
    instructions: String,
    slots: Arc<ResolvedPrompts>,
    config: AgentConfig,
    excluded_tools: Vec<&'static str>,
    mcp: bool,
    background: bool,
    system_override: Option<String>,
    append_system: Option<String>,
}

impl ContextSpec {
    /// Takes the handle rather than a `bool`. Two flags side by side are one
    /// silent swap away from a session that describes tools it cannot call.
    fn new(
        vars: template::Vars,
        instructions: &agent::Instructions,
        slots: Arc<ResolvedPrompts>,
        config: AgentConfig,
        excluded_tools: Vec<&'static str>,
        mcp: Option<&McpHandle>,
    ) -> Self {
        Self {
            vars,
            instructions: instructions.text.clone(),
            slots,
            config,
            excluded_tools,
            mcp: mcp.is_some(),
            background: true,
            system_override: None,
            append_system: None,
        }
    }

    fn render(&self, model: &Model, workflow: bool) -> RunContext {
        let tools = RequestTools::build_with_background(
            ToolRegistry::global(),
            &self.vars,
            model,
            &self.config,
            &self.excluded_tools,
            workflow,
            self.mcp,
            self.background,
        );
        let mut context = match &self.system_override {
            Some(system) => {
                RunContext::fixed(system.clone(), tools).with_prompt_policy(&self.slots)
            }
            None => RunContext::render(&self.vars, &self.instructions, &self.slots, model, tools),
        };
        if let Some(append) = &self.append_system {
            for text in [&mut context.system, &mut context.authored] {
                text.push('\n');
                text.push_str(append);
            }
        }
        context
    }

    fn builder(self) -> RunContextBuilder {
        Arc::new(move |model, workflow| self.render(model, workflow))
    }
}

/// Names advertised to SDK clients: base tools plus what the first request
/// would put in the model's context from MCP (always-load definitions and
/// `tool_search`). Probed as [`ToolDeferral::Client`] because the native
/// array lists every deferred definition too.
fn advertised_tool_names(tools: &Value, mcp: Option<&McpSession>) -> Vec<String> {
    let mut probe = tools.clone();
    if let Some(mcp) = mcp {
        mcp.extend_tools(&mut probe, ToolDeferral::Client);
    }
    extract_tool_names(&probe)
}

pub trait SessionPlugins: Send + Sync {
    fn prompts(&self) -> BoxFuture<'static, ResolvedPrompts>;
    fn observe(&self, session: MakiId, envelope: &Envelope);
    fn fire(&self, session: MakiId, event: &str, data: Value) -> Result<(), String>;
    fn stop_task(&self, id: &str);
    fn move_tool(&self, _id: String) -> BoxFuture<'static, maki_types::MoveOutcome> {
        Box::pin(std::future::ready(maki_types::MoveOutcome::NoHook))
    }
}

pub struct InteractiveParams {
    pub background: BackgroundMode,
    pub stop: CancelToken,
    pub unattended: bool,
    pub auto_wake: bool,
    pub reauth: bool,
    pub plugins: Option<Arc<dyn SessionPlugins>>,
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub prompt_slots: Arc<ResolvedPrompts>,
    pub excluded_tools: Vec<&'static str>,
    pub mcp_handle: Option<McpHandle>,
    pub initial_wd: PathBuf,
    pub resumed: Resumed,
    /// The exclusive right to write the resumed session.
    pub claim: SessionClaim,
    /// The same state directory the session was loaded from.
    pub storage: StateDir,
    pub yolo: bool,
    pub system_prompt_override: Option<String>,
    pub append_system_prompt: Option<String>,
    /// The `always_*` knobs. `workflow` picks the tool catalog here; the rest
    /// are what a host without toggles puts on every [`AgentInput`] it sends.
    pub defaults: SessionDefaults,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    pub project_config: ProjectConfig,
    /// Host-side overrides that shadow a registered tool's execution while
    /// keeping its advertised schema (e.g. ACP answers `question` via elicitation).
    pub local_tools: LocalTools,
    /// The host's settings, which the session saves in place of each
    /// prompt's own. `None` keeps saving what each prompt ran under.
    pub settings: Option<SharedSettings>,
}

pub use maki_types::input::{Compaction, SessionInput};

pub struct InteractiveHandle {
    pub tool_names: Vec<String>,
    pub session: SessionControl,
    pub input_tx: flume::Sender<SessionInput>,
    pub answer_tx: flume::Sender<String>,
    pub cancels: Arc<RunCancels>,
    pub rewind_tx: flume::Sender<RewindRequest>,
    pub model_tx: flume::Sender<Model>,
    pub mode_tx: flume::Sender<AgentMode>,
    /// Wakes the driver for pending rewinds and shared settings changes.
    pub settings_saved: flume::Sender<()>,
    /// The credentials or provider configuration changed: the next input
    /// rebuilds the model and its provider from what they are now.
    /// Coalescing, as `settings_saved` is.
    pub rebuild_tx: flume::Sender<()>,
    pub session_id: SessionRef,
    pub permissions: Arc<PermissionManager>,
    pub task: SessionTask,
    pub spend: Arc<RunLedger>,
}

pub struct RewindRequest {
    pub index: usize,
    pub prompt: String,
    pub reply: flume::Sender<Result<StoredSession, String>>,
}

impl InteractiveHandle {
    pub fn switch_mode(&self, mode: AgentMode) {
        if let Some(seed) = AgentSeed::live(self.session_id.id()) {
            seed.set_chat_settings(seed.chat_opts(), &mode);
        }
        let _ = self.mode_tx.send(mode);
        let _ = self.settings_saved.try_send(());
    }

    pub async fn rewind(&self, index: usize, prompt: String) -> Result<StoredSession, String> {
        let (reply, result) = flume::bounded(1);
        self.cancels
            .post_cancel(
                &self.rewind_tx,
                RewindRequest {
                    index,
                    prompt,
                    reply,
                },
            )
            .map_err(|_| "session ended".to_owned())?;
        let _ = self.settings_saved.try_send(());
        result
            .recv_async()
            .await
            .map_err(|_| "session ended".to_owned())?
    }
}

fn rewind_session(
    request: RewindRequest,
    track: &mut SessionTrack,
    input: &flume::Receiver<SessionInput>,
    events: &EventSender,
    selected: bool,
    window: u32,
    plugins: Option<&dyn SessionPlugins>,
) -> bool {
    let before = track.control().snapshot();
    let result = track
        .rewind(request.index, &request.prompt, events.run_id() + 1)
        .map(|receipts| {
            queue_updates(events, receipts);
            let snapshot = track.control().snapshot();
            if let Some(plugins) = plugins {
                let surviving: HashSet<_> = snapshot
                    .messages()
                    .iter()
                    .flat_map(Message::tool_uses)
                    .map(|(id, _, _)| id)
                    .collect();
                for (id, _, _) in before.messages().iter().flat_map(Message::tool_uses) {
                    if !surviving.contains(id) {
                        plugins.stop_task(id);
                    }
                }
            }
            events.try_send(AgentEvent::ContextReset {
                context_size: snapshot.meta.context_size,
                context_window: window,
            });
            for _ in 0..input.try_iter().count() + usize::from(selected) {
                cancelled(events, snapshot.meta.context_size, window);
            }
            snapshot
        })
        .map_err(|error| error.to_string());
    let committed = result.is_ok();
    let _ = request.reply.send(result);
    committed
}

fn cancelled(events: &EventSender, context_size: u32, context_window: u32) {
    let _ = events.send(AgentEvent::Done {
        usage: TokenUsage::default(),
        cost: None,
        list_cost: None,
        context_size,
        context_window,
        num_turns: 0,
        reason: DoneReason::Cancelled,
    });
}

enum Idle {
    Mailbox,
    Queue,
    Input(Result<SessionInput, flume::RecvError>),
    Save,
}

pub async fn resolve_prompt(
    input: &mut AgentInput,
    mcp: Option<&McpSession>,
) -> Result<(), AgentError> {
    if let Some(ref prompt_ref) = input.prompt {
        let Some(mcp) = mcp else {
            return Err(AgentError::Tool {
                tool: "mcp_prompt".into(),
                message: "MCP not available".into(),
            });
        };
        let messages = mcp
            .get_prompt(&prompt_ref.qualified_name, &prompt_ref.arguments)
            .await
            .map_err(|e| AgentError::Tool {
                tool: "mcp_prompt".into(),
                message: e.to_string(),
            })?;
        for pm in messages {
            let text = pm.text;
            let msg = match pm.role {
                PromptRole::Assistant => Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text { text }],
                    ..Default::default()
                },
                PromptRole::User => Message::user(text),
            };
            input.preamble.push(msg);
        }
    }

    Ok(())
}

struct HostInterrupts {
    session: SessionControl,
    events: EventSender,
}

fn queue_updates(events: &EventSender, receipts: Vec<QueueReceipt>) {
    for receipt in receipts {
        let _ = events.send(AgentEvent::QueueUpdated(receipt));
    }
}

impl InterruptSource for HostInterrupts {
    fn poll<'a>(
        &'a self,
        cancel: &'a CancelToken,
        mode: &'a AgentMode,
    ) -> provider::BoxFuture<'a, Option<ExtractedCommand>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return None;
            }
            match self.session.take_queued(Some(mode), Some(cancel)) {
                Ok(Some((input, receipts))) => {
                    queue_updates(&self.events, receipts);
                    Some(match input {
                        SessionInput::Prompt(mut input) => {
                            let earlier = std::mem::take(&mut input.earlier);
                            let mut inputs: Vec<_> = earlier
                                .into_iter()
                                .map(|earlier| AgentInput {
                                    message: earlier.message,
                                    images: earlier.images,
                                    preamble: earlier.preamble,
                                    ..AgentInput::from_defaults(
                                        String::new(),
                                        input.mode.clone(),
                                        Vec::new(),
                                        SessionDefaults {
                                            thinking: Some(input.thinking.into()),
                                            fast: input.fast,
                                            workflow: input.workflow,
                                        },
                                        input.source,
                                    )
                                })
                                .collect();
                            inputs.push(input);
                            ExtractedCommand::Interrupt(inputs)
                        }
                        SessionInput::Compact(compact) => {
                            ExtractedCommand::Compact(compact.instructions)
                        }
                    })
                }
                Ok(None) => None,
                Err(error) => {
                    let _ = self.events.send(AgentEvent::Error {
                        message: error.to_string(),
                        auth: false,
                        totals: RunTotals::default(),
                    });
                    None
                }
            }
        })
    }
}

pub fn spawn_interactive(mut params: InteractiveParams) -> (InteractiveHandle, SessionEvents) {
    let vars = template::vars_in(&params.initial_wd.to_string_lossy());
    let mut instructions =
        agent::load_instructions(&vars.apply("{cwd}"), &params.config.instructions);
    let mut spec = ContextSpec {
        system_override: params.system_prompt_override.clone(),
        append_system: params.append_system_prompt.clone(),
        ..ContextSpec::new(
            vars,
            &instructions,
            Arc::clone(&params.prompt_slots),
            params.config.clone(),
            params.excluded_tools,
            params.mcp_handle.as_ref(),
        )
    };
    spec.background = params.background.seeded();
    let initial = spec.render(&params.model, params.defaults.workflow);

    let mcp = params
        .mcp_handle
        .clone()
        .map(|h| McpSession::new(h, &params.resumed.history));
    let tool_names = advertised_tool_names(initial.tools.definitions(), mcp.as_ref());

    let (guard, events) = event_stream();
    let base_tx = guard.sender(0);
    let (input_tx, input_rx) = flume::unbounded::<SessionInput>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let cancels = RunCancels::new();
    let run_cancels = Arc::clone(&cancels);
    let (rewind_tx, rewind_rx) = flume::unbounded();
    let (model_tx, model_rx) = flume::unbounded::<Model>();
    let (mode_tx, mode_rx) = flume::unbounded::<AgentMode>();
    let (settings_saved, settings_saved_rx) = flume::bounded::<()>(1);
    let (rebuild_tx, rebuild_rx) = flume::bounded::<()>(1);

    let session_ref = params.resumed.id.clone();
    let mailbox = open_mailbox(&params.resumed);

    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let mut permissions_config = params.permissions_config;
    permissions_config.yolo |= params.yolo;
    let permissions = Arc::new(PermissionManager::new(
        permissions_config,
        params.initial_wd,
        params.project_config,
        Arc::clone(&params.plugin_rules),
    ));

    let answer_rx = Arc::new(Mutex::new(answer_rx));

    // Built before the task: a host may accept a settings change and close
    // the session before the task first runs, and the track is what saves it
    // as it drops. Opening touches no file.
    let spend = Arc::<RunLedger>::default();
    let mut track = SessionTrack::with_background(
        params.resumed,
        params.claim,
        params.storage,
        mailbox.clone(),
        Arc::clone(&spend),
        &working_dir,
    )
    .with_permissions(Arc::clone(&permissions));
    if let Some(settings) = &params.settings {
        track = track.with_settings(Arc::clone(settings));
    }
    let session_control = track.control();
    let queue_control = session_control.clone();
    if !params.unattended {
        mailbox.publish_events(base_tx.clone());
    }
    let initial_provider = Arc::from(provider::from_model_fallback(
        &mut params.model,
        params.timeouts,
    ));
    let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
        model: params.model.clone(),
        provider: initial_provider,
    }));
    let seed = publish_seed(&mailbox, &params.background, || {
        AgentSeed::new(
            session_ref.clone(),
            Arc::clone(&model_slot),
            params.config.clone(),
            ToolOutputLines::default(),
            Arc::clone(&permissions),
            Arc::from(permissions.working_dir()),
            mcp.clone(),
            params.timeouts,
            Arc::clone(&params.model_policy),
        )
    });
    let file_access = seed
        .as_ref()
        .map_or_else(FileAccess::fresh, |seed| Arc::clone(&seed.file_access));
    if let Some(seed) = &seed {
        seed.instruction_files
            .store(Arc::clone(&instructions.files));
        seed.set_chat_settings(
            RequestOptions {
                thinking: params.defaults.thinking.map(Into::into).unwrap_or_default(),
                fast: params.defaults.fast,
            },
            &AgentMode::Build,
        );
    }
    let session_ref_clone = session_ref.clone();
    let task_permissions = Arc::clone(&permissions);
    let task = AbortOnDropHandle::new(tokio::spawn(run_session(
        guard,
        params.mcp_handle.clone(),
        async move {
            let mut keep_alive = KeepAlive::new(mailbox.clone(), params.background.clone());
            let mut input_closed = false;
            let mut model = params.model;
            // Connected for the first input, and again after a rebuild: a
            // session whose provider cannot connect yet, for want of a login,
            // stays up and answers each input with the error.
            let mut provider: Option<Arc<dyn Provider>> = None;
            let mut run_id: u64 = 0;

            match queue_control.recover_queue(params.defaults) {
                Ok(receipts) => queue_updates(&base_tx, receipts),
                Err(error) => {
                    let _ = base_tx.send(AgentEvent::Error {
                        message: error.to_string(),
                        auth: false,
                        totals: RunTotals::default(),
                    });
                    return Some(track);
                }
            }
            loop {
                if let Some(mode) = mode_rx.try_iter().last() {
                    keep_alive.switch_mode(mode);
                }
                if params.stop.is_cancelled() {
                    warn_unreported(mailbox.unsettled().len(), STOPPED_CAUSE);
                    break;
                }
                if let Ok(request) = rewind_rx.try_recv() {
                    rewind_session(
                        request,
                        &mut track,
                        &input_rx,
                        &base_tx.with_run_id(run_id),
                        false,
                        model.context_window,
                        params.plugins.as_deref(),
                    );
                    run_id += 1;
                    continue;
                }
                let next = match queue_control.take_queued(None, None) {
                    Ok(next) => next,
                    Err(error) => {
                        let _ = base_tx.send(AgentEvent::Error {
                            message: error.to_string(),
                            auth: false,
                            totals: RunTotals::default(),
                        });
                        break;
                    }
                };
                let input = match next {
                    Some((input, receipts)) => {
                        queue_updates(&base_tx, receipts);
                        input
                    }
                    None if input_closed => match params.stop.race(keep_alive.next_input()).await {
                        Ok(Some(mut input)) => {
                            if let Some(mode) = mode_rx.try_iter().last() {
                                input.mode = mode;
                            }
                            SessionInput::Prompt(input)
                        }
                        _ => break,
                    },
                    None => match params
                        .stop
                        .race(futures_lite::future::or(
                            async {
                                futures_lite::future::or(
                                    async { Idle::Input(input_rx.recv_async().await) },
                                    async {
                                        if !params.auto_wake {
                                            return futures_lite::future::pending().await;
                                        }
                                        mailbox.wait_for_wake().await;
                                        Idle::Mailbox
                                    },
                                )
                                .await
                            },
                            futures_lite::future::or(
                                async {
                                    queue_control.queue_ready().await;
                                    Idle::Queue
                                },
                                async {
                                    match settings_saved_rx.recv_async().await {
                                        Ok(()) => Idle::Save,
                                        Err(_) => futures_lite::future::pending().await,
                                    }
                                },
                            ),
                        ))
                        .await
                        .unwrap_or(Idle::Input(Err(flume::RecvError::Disconnected)))
                    {
                        Idle::Mailbox => {
                            let preamble = mailbox.claim_wake();
                            if !preamble.is_empty() {
                                let snapshot = queue_control.snapshot();
                                let mut input = AgentInput::from_defaults(
                                    String::new(),
                                    seed.as_ref()
                                        .map_or(AgentMode::Build, |seed| seed.chat_mode()),
                                    Vec::new(),
                                    SessionDefaults {
                                        thinking: snapshot.meta.thinking,
                                        fast: snapshot.meta.fast,
                                        workflow: snapshot.meta.workflow,
                                    },
                                    InputSource::Plugin,
                                );
                                input.preamble = preamble;
                                match queue_control.enqueue(&SessionInput::Prompt(input)) {
                                    Ok(receipt) => queue_updates(&base_tx, vec![receipt]),
                                    Err(error) => base_tx.try_send(AgentEvent::Error {
                                        message: error.to_string(),
                                        auth: false,
                                        totals: RunTotals::default(),
                                    }),
                                }
                            }
                            continue;
                        }
                        Idle::Input(Ok(input)) => input,
                        Idle::Input(Err(_)) => {
                            input_closed = true;
                            continue;
                        }
                        Idle::Save => {
                            track.save_settings();
                            continue;
                        }
                        Idle::Queue => continue,
                    },
                };
                if let (Some(plugins), SessionInput::Prompt(input)) = (&params.plugins, &input) {
                    let _ = plugins.fire(
                        session_ref_clone.id(),
                        "TurnStart",
                        serde_json::json!({ "text": input.message }),
                    );
                }
                let cwd = queue_control.snapshot().cwd;
                if spec.vars.apply("{cwd}") != cwd {
                    let next = cwd.clone();
                    let config = params.config.instructions.clone();
                    instructions = tokio::task::spawn_blocking(move || {
                        agent::load_instructions(&next, &config)
                    })
                    .await
                    .expect("instruction loader panicked");
                    spec.instructions = instructions.text.clone();
                    if let Some(seed) = &seed {
                        seed.instruction_files
                            .store(Arc::clone(&instructions.files));
                    }
                }
                spec.vars = template::vars_in(&cwd);
                if let Some(seed) = &seed {
                    seed.set_working_dir(Path::new(&cwd));
                }
                if let Some(plugins) = &params.plugins {
                    spec.slots = Arc::new(plugins.prompts().await);
                }
                spec.background = params.background.seeded();
                let context = spec.clone().builder();
                let live = run_cancels.start(run_id);
                let cancel = live.token().clone();
                let stopping = {
                    let stop = params.stop.clone();
                    let run_cancels = Arc::clone(&run_cancels);
                    AbortOnDropHandle::new(tokio::spawn(async move {
                        stop.cancelled().await;
                        run_cancels.cancel_current();
                    }))
                };
                if let Ok(request) = rewind_rx.try_recv()
                    && rewind_session(
                        request,
                        &mut track,
                        &input_rx,
                        &base_tx.with_run_id(run_id),
                        true,
                        model.context_window,
                        params.plugins.as_deref(),
                    )
                {
                    run_id += 1;
                    continue;
                }

                // MCP connects in the background, so a prompt that beats it waits
                // here instead of shipping a turn without the MCP tools. The wait
                // is racing cancel: a slow server must not pin the whole session.
                if let Some(mcp) = &mcp {
                    let _ = cancel.race(mcp.ready()).await;
                }

                let event_tx = base_tx.with_run_id(run_id);
                let error_tx = event_tx.clone();

                // The model this input runs: a picked one stays the target until
                // it connects, so a failed connect retries it, never the last.
                if let Some(picked) = model_rx
                    .try_iter()
                    .last()
                    .filter(|candidate| params.model_policy.allows(&candidate.spec()))
                    && picked.spec() != model.spec()
                {
                    model = picked;
                    provider = None;
                }
                // Resolved afresh from its spec, so nothing a provider adjusted
                // under the old credentials survives the new ones.
                if rebuild_rx.try_recv().is_ok() {
                    provider = None;
                    if let Ok(fresh) = Model::from_spec(&model.spec()) {
                        model = fresh;
                    }
                }

                let provider = match provider.clone() {
                    Some(connected) => connected,
                    None => {
                        let Some(connected) = connect(&mut model, params.timeouts, &error_tx).await
                        else {
                            if !finish_queue(
                                &mut track,
                                &base_tx,
                                QueueStatus::Error,
                                Some("provider could not connect".into()),
                            ) {
                                break;
                            }
                            run_id += 1;
                            continue;
                        };
                        provider = Some(Arc::clone(&connected));
                        connected
                    }
                };

                model_slot.store(Arc::new(ModelSlot {
                    model: model.clone(),
                    provider: Arc::clone(&provider),
                }));
                while answer_rx.lock().await.try_recv().is_ok() {}

                let input = match input {
                    SessionInput::Prompt(mut input) => {
                        if let Err(error) = resolve_prompt(&mut input, mcp.as_ref()).await {
                            let _ = event_tx.send(error_event(&error, RunTotals::default()));
                            if !finish_queue(
                                &mut track,
                                &event_tx,
                                QueueStatus::Error,
                                Some(error.to_string()),
                            ) {
                                break;
                            }
                            run_id += 1;
                            continue;
                        }
                        input
                    }
                    SessionInput::Compact(compaction) => {
                        let (compactor, compaction_model) = agent::resolve_compaction_model(
                            &provider,
                            &model,
                            params.timeouts,
                            &params.model_policy,
                        )
                        .await;
                        let result = compact_session(
                            &mut track,
                            CompactionRun {
                                compactor: &*compactor,
                                compaction_model: &compaction_model,
                                model: &model,
                                context: &context,
                                mcp: mcp.as_ref(),
                                session_id: &session_ref_clone,
                                cancel: &cancel,
                                config: &params.config,
                                retry: params.timeouts.retry,
                            },
                            &compaction,
                            &event_tx,
                        )
                        .await;
                        let (status, error) = match result {
                            Ok(DoneReason::Cancelled) | Err(AgentError::Cancelled) => {
                                (QueueStatus::Cancelled, None)
                            }
                            Ok(_) => (QueueStatus::Done, None),
                            Err(e) => {
                                error!(error = %e, "compaction error");
                                let _ = event_tx.send(error_event(&e, RunTotals::default()));
                                (QueueStatus::Error, Some(e.to_string()))
                            }
                        };
                        if !finish_queue(&mut track, &event_tx, status, error) {
                            break;
                        }
                        run_id += 1;
                        continue;
                    }
                };

                // Outlives `agent`, so this turn is on disk before the next prompt
                // is read, without the loop body remembering to write it.
                if params.settings.is_none() {
                    adopt_chat_settings(seed.as_deref(), &input);
                }
                let mut turn = track.turn(model.spec(), &input);
                let mut agent = Agent::new(
                    AgentParams {
                        provider: Arc::clone(&provider),
                        model: model.clone(),
                        config: params.config.clone(),
                        tool_output_lines: ToolOutputLines::default(),
                        working_dir: Arc::from(Path::new(&cwd)),
                        permissions: Arc::clone(&task_permissions),
                        session_id: Some(session_ref_clone.clone()),
                        background_for: BackgroundFor::None,
                        task_id: None,
                        mailbox: Some(mailbox.clone()),
                        timeouts: params.timeouts,
                        file_access: Arc::clone(&file_access),
                        prompt_slots: Arc::clone(&spec.slots),
                        subagent_cancels: Arc::clone(&run_cancels.subagents),
                        ledger: turn.ledger(),
                        registry: Arc::clone(ToolRegistry::global_arc()),
                        audience: ToolAudience::MAIN,
                        model_policy: Arc::clone(&params.model_policy),
                    },
                    turn.run_params(event_tx, Arc::clone(&context)),
                )
                .with_loaded_instructions(instructions.loaded.clone(), instructions.files.clone())
                .with_cancel(cancel)
                .with_reauth(params.reauth)
                .with_local_tools(Arc::clone(&params.local_tools))
                .with_mcp(mcp.clone());
                if !params.unattended {
                    agent = agent.with_user_response_rx(Arc::clone(&answer_rx));
                }
                agent = agent.with_interrupt_source(Arc::new(HostInterrupts {
                    session: queue_control.clone(),
                    events: error_tx.clone(),
                }));

                keep_alive.switch_mode(
                    seed.as_ref()
                        .map_or_else(|| input.mode.clone(), |seed| seed.chat_mode()),
                );
                keep_alive.follow(&input);
                let result = agent.run(input).await;
                if result.is_err() {
                    keep_alive.fail();
                }
                drop(agent);
                drop(stopping);

                if let Err(ref e) = result {
                    error!(error = %e, "agent error");
                    let _ = error_tx.send(error_event(e, turn.ledger().totals()));
                }
                drop(turn);
                let (status, error) = match result {
                    Ok(DoneReason::Cancelled) | Err(AgentError::Cancelled) => {
                        (QueueStatus::Cancelled, None)
                    }
                    Ok(_) => (QueueStatus::Done, None),
                    Err(error) => (QueueStatus::Error, Some(error.to_string())),
                };
                if !finish_queue(&mut track, &error_tx, status, error) {
                    break;
                }
                run_id += 1;
            }
            Some(track)
        },
    )));

    (
        InteractiveHandle {
            tool_names,
            session: session_control,
            input_tx,
            answer_tx,
            cancels,
            rewind_tx,
            model_tx,
            mode_tx,
            settings_saved,
            rebuild_tx,
            session_id: session_ref,
            permissions,
            task,
            spend,
        },
        events,
    )
}

/// What a `/compact` runs with besides the request.
struct CompactionRun<'a> {
    compactor: &'a dyn Provider,
    compaction_model: &'a Model,
    /// The session's model, which the summary's frame and gauge are for.
    model: &'a Model,
    context: &'a RunContextBuilder,
    mcp: Option<&'a McpSession>,
    session_id: &'a SessionRef,
    cancel: &'a CancelToken,
    config: &'a AgentConfig,
    retry: RetryPolicy,
}

/// A `/compact` for a host with no App to draw or bill it: its start and a
/// cancelled end are events too, since a host answers its request on `Done`,
/// and the turn's ledger keeps the spending in the session file.
async fn compact_session(
    track: &mut SessionTrack,
    run: CompactionRun<'_>,
    compaction: &Compaction,
    event_tx: &EventSender,
) -> Result<DoneReason, AgentError> {
    let mut turn = track.compaction(run.model.spec());
    let ledger = turn.ledger();
    let transcript = turn.run_params(event_tx.clone(), Arc::clone(run.context));
    let context_size = transcript.gauge.size();
    event_tx.send(AgentEvent::AutoCompacting {
        context_size,
        context_window: run.model.context_window,
        manual: true,
    })?;
    let hooks = AgentHooks {
        registry: ToolRegistry::global(),
        session_id: Some(run.session_id),
        task_id: None,
        model: run.model,
        cancel: run.cancel,
        context_size,
    };
    let reason = agent::compact(
        run.compactor,
        run.compaction_model,
        transcript.history,
        transcript.gauge,
        (run.context)(run.model, compaction.workflow),
        run.mcp,
        event_tx,
        &hooks,
        run.config,
        compaction.instructions.as_deref(),
        run.retry,
        Some(&ledger),
    )
    .await?;
    if reason == DoneReason::Cancelled {
        cancelled(event_tx, context_size, run.model.context_window);
    }
    Ok(reason)
}

fn finish_queue(
    track: &mut SessionTrack,
    events: &EventSender,
    status: QueueStatus,
    error: Option<String>,
) -> bool {
    match track.finish_queue(status, error) {
        Ok(receipts) => {
            queue_updates(events, receipts);
            true
        }
        Err(error) => {
            let _ = events.send(AgentEvent::Error {
                message: error.to_string(),
                auth: false,
                totals: RunTotals::default(),
            });
            false
        }
    }
}

/// Waits for a session task that has nothing left to do but tear MCP down,
/// dropping it if that wedges. Safe to bound: the event stream already ended
/// (see [`run_session`]), and dropping the task cannot resurrect it.
pub async fn await_shutdown(task: SessionTask) -> Option<SessionTrack> {
    task.await.expect("session task panicked").shut_down().await
}

impl SessionEnd {
    /// Gives up on a teardown that wedges. The track comes back either way.
    async fn shut_down(self) -> Option<SessionTrack> {
        if let Some(mcp) = self.mcp {
            let _ = timeout(SESSION_SHUTDOWN_TIMEOUT, mcp.shutdown()).await;
        }
        self.track
    }
}

/// Runs a session body and ends its event stream with it. Teardown is
/// [`await_shutdown`]'s: a wedged shutdown must neither keep a consumer
/// waiting for events that can no longer come nor hold on to the track.
async fn run_session(
    guard: EventStreamGuard,
    mcp: Option<McpHandle>,
    body: impl Future<Output = Option<SessionTrack>>,
) -> SessionEnd {
    let track = body.await;
    drop(guard);
    SessionEnd { track, mcp }
}

fn extract_tool_names(tools: &Value) -> Vec<String> {
    tools
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::atomic::{AtomicBool, Ordering};

    use futures::FutureExt;
    use tempfile::TempDir;
    use test_case::test_case;

    use maki_providers::provider::BoxFuture;
    use maki_providers::{
        ContentBlock, Message, ProviderEvent, RequestOptions, Role, StopReason, StreamResponse,
    };

    use super::*;
    use crate::mcp::McpCommand;
    use crate::permissions::PermissionCheck;
    use crate::prompt::{PromptId, Slot, SlotEntry};
    use crate::session::{SessionSettings, StoredSession};
    use maki_config::{Effect, PermissionRule, ToolKey};
    use maki_storage::sessions::{StoredEffect, StoredRule};

    const RUN_ID: u64 = 7;
    const STREAM_ENDED: &str = "the stream must end with the run, not with teardown";
    const SHUTDOWN_WEDGED: &str = "the MCP shutdown must still be waiting for its ack";
    const TEARDOWN_AFTER_TRACK: &str =
        "MCP teardown must not start before the task hands back its track";
    const TRACK_RETURNED: &str = "shutting down must hand back the track";
    const PROVIDER_ERROR: &str = "provider error";
    const BODY_EVENT: &str = "the body's event must be delivered before the close";
    const STILL_WORKING: &str = "await_shutdown must not return while the task still works";
    const TASK_DROPPED: &str = "await_shutdown dropped a task that had work left";
    const SESSION_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
    const SUMMARY: &str = "we renamed foo to bar";
    const SUMMARY_OUTPUT: u32 = 50;
    /// Differs from the session model's, so a window read off the compactor
    /// shows.
    const COMPACTOR_WINDOW: u32 = 8_000;
    const UNKNOWN_PROVIDER: &str = "no-such-provider";

    #[tokio::test]
    async fn keep_alive_delivers_the_result_before_ending() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        let mut keep_alive = KeepAlive::new(
            mailbox.clone(),
            BackgroundMode::KeepAlive(Duration::from_secs(60)),
        );
        let input = AgentInput::from_defaults(
            SUMMARY.into(),
            AgentMode::Build,
            Vec::new(),
            SessionDefaults::default(),
            InputSource::Headless,
        );
        keep_alive.follow(&input);
        mailbox.track(SUMMARY);
        mailbox.settle(
            SUMMARY,
            Some(crate::BackgroundNotice {
                text: SUMMARY.into(),
                display: None,
                wake: true,
                end: crate::TaskEnd::Finished,
            }),
        );
        let woken = keep_alive.next_input().await.unwrap();
        assert!(woken.message.is_empty());
        assert_eq!(woken.preamble.len(), 1);
        keep_alive.follow(&woken);
        assert!(keep_alive.next_input().await.is_none());
    }

    struct SummaryProvider;

    impl Provider for SummaryProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                Ok(StreamResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: SUMMARY.into(),
                        }],
                        ..Default::default()
                    },
                    usage: TokenUsage {
                        output: SUMMARY_OUTPUT,
                        ..TokenUsage::default()
                    },
                    stop_reason: Some(StopReason::EndTurn),
                    ..Default::default()
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<maki_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    /// A stored two-message session and the track that writes it.
    fn stored_track(tmp: &tempfile::TempDir) -> (SessionTrack, MakiId, StateDir) {
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut stored = StoredSession::new(SESSION_SPEC, "/project");
        stored.push_message(Message::user("rename foo".into()));
        stored.push_message(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "done".into(),
            }],
            ..Default::default()
        });
        let claim = SessionClaim::acquire(stored.id, &storage).unwrap();
        stored.save(&claim, &storage).unwrap();
        let id = stored.id;
        let track = SessionTrack::open(
            Resumed::stored(SessionRef::from(id), stored),
            claim,
            storage.clone(),
            "/project",
        );
        (track, id, storage)
    }

    fn compact_stored(
        track: &mut SessionTrack,
        cancel: &CancelToken,
    ) -> (Result<DoneReason, AgentError>, Vec<AgentEvent>) {
        let model = Model::from_spec(SESSION_SPEC).unwrap();
        let compaction_model = Model {
            context_window: COMPACTOR_WINDOW,
            ..model.clone()
        };
        let context: RunContextBuilder = Arc::new(|model, _| {
            RunContext::fixed(
                String::new(),
                RequestTools::assembled(Value::Array(Vec::new()), &AgentConfig::default(), model),
            )
        });
        let session_id = SessionRef::from(MakiId::generate());
        let (raw_tx, rx) = flume::unbounded();
        let result = crate::test_block_on(compact_session(
            track,
            CompactionRun {
                compactor: &SummaryProvider,
                compaction_model: &compaction_model,
                model: &model,
                context: &context,
                mcp: None,
                session_id: &session_id,
                cancel,
                config: &AgentConfig::default(),
                retry: RetryPolicy::default(),
            },
            &Compaction {
                instructions: None,
                workflow: false,
            },
            &EventSender::new(raw_tx, 0),
        ));
        (
            result,
            rx.try_iter().map(|envelope| envelope.event).collect(),
        )
    }

    /// A host-written prompt never had maki's facts in it. If the frame
    /// claimed it did, a later edit would be announced to a model that never
    /// saw the original.
    #[test_case(None, true ; "rendered_prompt_states_them")]
    #[test_case(Some("custom"), false ; "override_leaves_them_out")]
    fn override_states_no_facts(system_override: Option<&str>, tracked: bool) {
        let mut slots = ResolvedPrompts::default();
        slots.insert(
            PromptId::System,
            Slot::AfterInstructions,
            SlotEntry {
                plugin: Arc::from("memory"),
                content: "tags: a".into(),
            },
        );
        let instructions = agent::Instructions {
            text: "\n\nProject instructions:\nbe nice".into(),
            ..Default::default()
        };
        let spec = ContextSpec {
            system_override: system_override.map(String::from),
            ..ContextSpec::new(
                template::Vars::new(),
                &instructions,
                Arc::new(slots),
                AgentConfig::default(),
                Vec::new(),
                None,
            )
        };
        let model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let facts = spec.render(&model, false).facts;
        assert_eq!(
            facts.is_some_and(|f| !f.instructions.is_empty() && !f.hints.is_empty()),
            tracked
        );
    }

    #[test_case(None, "You analyse information." ; "plugin_template")]
    #[test_case(Some("Explicit SDK policy."), "Explicit SDK policy." ; "sdk_precedence")]
    fn headless_prompt_replacement_keeps_explicit_sdk_precedence(
        explicit: Option<&str>,
        expected: &str,
    ) {
        const PLUGIN_SYSTEM: &str = "You analyse information.";
        const OMITTED_INSTRUCTIONS: &str = "Project instructions must not be appended.";
        let mut prompts = ResolvedPrompts::default();
        prompts.set_template(PromptId::System, PLUGIN_SYSTEM.into());
        let spec = ContextSpec {
            system_override: explicit.map(String::from),
            ..ContextSpec::new(
                template::Vars::new(),
                &agent::Instructions {
                    text: OMITTED_INSTRUCTIONS.into(),
                    ..Default::default()
                },
                Arc::new(prompts),
                AgentConfig::default(),
                Vec::new(),
                None,
            )
        };
        let model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let context = spec.render(&model, false);
        assert_eq!(context.system, expected);
        assert_eq!(context.facts.is_none(), explicit.is_some());
        assert!(!context.system.contains(OMITTED_INSTRUCTIONS));
    }

    #[test]
    fn extract_tool_names_filters_valid_entries() {
        let tools = serde_json::json!([{"name": "read"}, {"type": "function"}, {"name": "bash"}]);
        assert_eq!(extract_tool_names(&tools), vec!["read", "bash"]);
    }

    #[test]
    fn advertised_names_show_tool_search_not_deferred_tools() {
        let base = serde_json::json!([{"name": "read"}]);
        let mcp =
            crate::mcp::test_support::stub_session(&[("srv.fetch_issue", "Fetch a GitHub issue")]);
        let names = advertised_tool_names(&base, Some(&mcp));
        assert_eq!(
            names,
            vec!["read", crate::mcp::TOOL_SEARCH_TOOL_NAME],
            "clients must see the search tool, not deferred definitions"
        );
        assert_eq!(
            base,
            serde_json::json!([{"name": "read"}]),
            "probing must not bake MCP entries into the base tools"
        );
        assert_eq!(advertised_tool_names(&base, None), vec!["read"]);
    }

    /// The ordering the SDK exit rests on: the stream ends when the run ends,
    /// not when MCP teardown does. The handle here takes the `Shutdown` and
    /// never acks it, so the session future is still parked on its own timeout
    /// while the consumer already has the body's error and the end of the
    /// stream. The retained sender is the Lua tool context an idle VM never
    /// collects.
    #[tokio::test]
    async fn the_run_ends_the_stream_and_hands_back_its_track_before_mcp_teardown() {
        let tmp = TempDir::new().unwrap();
        let id = MakiId::generate();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let claim = SessionClaim::acquire(id, &dir).expect("nothing else holds it");
        let mailbox = SessionMailbox::register(id);
        let track = SessionTrack::with_background(
            Resumed::empty(SessionRef::from(id)),
            claim,
            dir,
            mailbox,
            Arc::default(),
            "/project",
        );
        let (guard, mut events) = event_stream();
        let retained = guard.sender(RUN_ID);
        let event_tx = retained.clone();
        let (cmd_tx, cmd_rx) = flume::unbounded();

        let ended = run_session(guard, Some(McpHandle::for_test(cmd_tx)), async move {
            let _ = event_tx.send(AgentEvent::Error {
                message: PROVIDER_ERROR.into(),
                auth: false,
                totals: RunTotals::default(),
            });
            Some(track)
        })
        .await;
        assert!(cmd_rx.is_empty(), "{TEARDOWN_AFTER_TRACK}");

        let envelope = events.next().await.expect(BODY_EVENT);
        assert!(matches!(
            envelope.event,
            AgentEvent::Error { message, .. } if message == PROVIDER_ERROR
        ));
        assert!(
            matches!(events.next().now_or_never(), Some(None)),
            "{STREAM_ENDED}"
        );

        let mut shutdown = pin!(ended.shut_down());
        assert!(
            shutdown.as_mut().now_or_never().is_none(),
            "{SHUTDOWN_WEDGED}"
        );
        let Ok(McpCommand::Shutdown { ack }) = cmd_rx.try_recv() else {
            panic!("{SHUTDOWN_WEDGED}");
        };
        ack.send(()).unwrap();
        assert!(shutdown.await.is_some(), "{TRACK_RETURNED}");
        assert!(retained.send(AgentEvent::Nudge).is_ok());
    }

    /// `await_shutdown` bounds teardown, not the session: a task with work left
    /// runs to completion. Dropping it here would cancel prompts stdin already
    /// queued.
    #[tokio::test]
    async fn await_shutdown_waits_for_a_task_that_still_works() {
        let (release_tx, release_rx) = flume::bounded::<()>(1);
        let finished = Arc::new(AtomicBool::new(false));
        let task = AbortOnDropHandle::new(tokio::spawn({
            let finished = Arc::clone(&finished);
            async move {
                let _ = release_rx.recv_async().await;
                finished.store(true, Ordering::SeqCst);
                SessionEnd::default()
            }
        }));

        let mut shutdown = pin!(await_shutdown(task));
        assert!(
            shutdown.as_mut().now_or_never().is_none(),
            "{STILL_WORKING}"
        );
        release_tx.send(()).unwrap();
        shutdown.await;
        assert!(finished.load(Ordering::SeqCst), "{TASK_DROPPED}");
    }

    /// A host can accept a change and close the session before the driver
    /// first runs; the track it built up front still saves the change.
    #[test_case(true ; "shared_settings")]
    #[test_case(false ; "permissions_without_shared_settings")]
    #[tokio::test]
    async fn a_session_cancelled_before_it_ran_saves_settings_and_permissions(shared: bool) {
        let tmp = tempfile::TempDir::new().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut stored = StoredSession::new("anthropic/claude-sonnet-4-20250514", "/project");
        stored.push_message(Message::user("hello".into()));
        stored.meta.session_rules.push(StoredRule {
            tool: "mcp:server__tool".into(),
            scope: None,
            effect: StoredEffect::Deny,
        });
        let initial = SessionSettings::of(&stored);
        let claim = SessionClaim::acquire(stored.id, &storage).unwrap();
        stored.save(&claim, &storage).unwrap();
        let changed = SessionSettings {
            fast: true,
            yolo: Some(true),
            ..SessionSettings::of(&stored)
        };
        let id: MakiId = stored.id;
        let (handle, _events) = spawn_interactive(InteractiveParams {
            background: BackgroundMode::Detached,
            stop: CancelToken::none(),
            unattended: false,
            auto_wake: true,
            reauth: false,
            plugins: None,
            model: Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap(),
            config: AgentConfig::default(),
            permissions_config: PermissionsConfig::default(),
            timeouts: Timeouts::default(),
            prompt_slots: Arc::default(),
            excluded_tools: Vec::new(),
            mcp_handle: None,
            initial_wd: tmp.path().to_path_buf(),
            resumed: Resumed::stored(SessionRef::from(id), stored),
            claim,
            storage: storage.clone(),
            yolo: false,
            system_prompt_override: None,
            append_system_prompt: None,
            defaults: SessionDefaults::default(),
            model_policy: Arc::default(),
            plugin_rules: Arc::default(),
            project_config: ProjectConfig::for_project(tmp.path()),
            local_tools: LocalTools::default(),
            settings: shared.then(|| changed.clone().shared()),
        });

        assert!(matches!(
            handle
                .permissions
                .check(&ToolKey::parse("server.tool").unwrap(), "", None),
            PermissionCheck::Denied(_)
        ));
        handle.permissions.add_session_rule(PermissionRule {
            tool: ToolKey::native("bash"),
            scope: None,
            effect: Effect::Deny,
        });
        let rules = handle.session.snapshot().meta.session_rules;
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].tool, "server.tool");
        handle.task.abort();
        let _ = handle.task.await;

        let saved = StoredSession::load(id, &storage).unwrap();
        assert_eq!(
            SessionSettings::of(&saved),
            if shared { changed } else { initial }
        );
        assert_eq!(saved.meta.session_rules, rules);
        assert_eq!(
            saved.messages().len(),
            1,
            "the transcript survives the save"
        );
    }

    /// The host has no App: the events draw the compaction, the session file
    /// keeps the summary and what it cost, and the gauge stays the session's.
    #[test]
    fn a_compaction_saves_its_summary_and_its_spending() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut track, id, storage) = stored_track(&tmp);

        let (result, events) = compact_stored(&mut track, &CancelToken::none());
        drop(track);

        assert_eq!(result.unwrap(), DoneReason::Compact);
        assert!(matches!(
            events.first(),
            Some(AgentEvent::AutoCompacting { .. })
        ));
        let window = events.iter().find_map(|event| match event {
            AgentEvent::CompactionDone { context_window, .. } => Some(*context_window),
            _ => None,
        });
        let session_window = Model::from_spec(SESSION_SPEC).unwrap().context_window;
        assert_eq!(window, Some(session_window));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::Compact,
                ..
            })
        ));
        let saved = StoredSession::load(id, &storage).unwrap();
        assert!(
            saved
                .messages()
                .iter()
                .any(|message| message.first_text_content() == Some(SUMMARY))
        );
        assert_eq!(saved.token_usage.output, SUMMARY_OUTPUT);
    }

    /// `compact` leaves a cancel's end to its caller, and the host answers
    /// its request on `Done`.
    #[test]
    fn a_cancelled_compaction_ends_with_done() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut track, id, storage) = stored_track(&tmp);
        let (trigger, cancel) = CancelToken::new();
        trigger.cancel();

        let (result, events) = compact_stored(&mut track, &cancel);
        drop(track);

        assert_eq!(result.unwrap(), DoneReason::Cancelled);
        assert!(matches!(
            &events[..],
            [
                AgentEvent::AutoCompacting { .. },
                AgentEvent::Done {
                    reason: DoneReason::Cancelled,
                    ..
                }
            ]
        ));
        assert_eq!(
            StoredSession::load(id, &storage).unwrap().messages().len(),
            2
        );
    }

    /// A provider that cannot connect yet, for want of a login, leaves the
    /// session up: each input gets the error, and a later one may connect.
    #[tokio::test]
    async fn a_failed_connect_keeps_the_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        maki_providers::test_support::warm_empty_catalog(storage.clone());
        let mut model = Model::from_spec(SESSION_SPEC).unwrap();
        model.provider = UNKNOWN_PROVIDER.into();
        let id = MakiId::generate();
        let claim = SessionClaim::acquire(id, &storage).unwrap();
        let mut stored = StoredSession::new(SESSION_SPEC, &tmp.path().to_string_lossy());
        stored.id = id;
        stored.push_message(Message::user(SUMMARY.into()));
        stored.save(&claim, &storage).unwrap();
        let (handle, mut events) = spawn_interactive(InteractiveParams {
            background: BackgroundMode::Detached,
            stop: CancelToken::none(),
            unattended: false,
            auto_wake: true,
            reauth: false,
            plugins: None,
            model,
            config: AgentConfig::default(),
            permissions_config: PermissionsConfig::default(),
            timeouts: Timeouts::default(),
            prompt_slots: Arc::default(),
            excluded_tools: Vec::new(),
            mcp_handle: None,
            initial_wd: tmp.path().to_path_buf(),
            resumed: Resumed::stored(SessionRef::from(id), stored),
            claim,
            storage,
            yolo: false,
            system_prompt_override: None,
            append_system_prompt: None,
            defaults: SessionDefaults::default(),
            model_policy: Arc::default(),
            plugin_rules: Arc::default(),
            project_config: ProjectConfig::for_project(tmp.path()),
            local_tools: LocalTools::default(),
            settings: None,
        });

        let seed = AgentSeed::live(id).expect("interactive seed before the first prompt");
        assert_eq!(&*seed.working_dir(), tmp.path());
        drop(seed);
        handle.switch_mode(AgentMode::Plan(tmp.path().join("plan.md")));
        assert!(AgentSeed::background_write_block(id).is_some());
        handle.switch_mode(AgentMode::Build);
        assert!(AgentSeed::background_write_block(id).is_none());
        SessionMailbox::task_event(
            id,
            "supervisor".into(),
            Envelope {
                event: AgentEvent::Nudge,
                subagent: None,
                run_id: 0,
            },
        );
        {
            assert!(matches!(events.next().await.unwrap().event,
                AgentEvent::PluginTask { plugin, .. } if &*plugin == "supervisor"));

            assert!(
                handle
                    .rewind(0, SUMMARY.into())
                    .await
                    .unwrap()
                    .messages()
                    .is_empty()
            );
            assert!(matches!(
                events.next().await.unwrap().event,
                AgentEvent::ContextReset {
                    context_size: 0,
                    ..
                }
            ));
            for _ in 0..2 {
                let input = AgentInput::from_defaults(
                    "hello".into(),
                    AgentMode::Build,
                    Vec::new(),
                    SessionDefaults::default(),
                    InputSource::Acp,
                );
                handle.input_tx.send(SessionInput::Prompt(input)).unwrap();
                let envelope = events.next().await.expect("the session answers");
                assert!(matches!(envelope.event, AgentEvent::Error { .. }));
            }
        }
        SessionMailbox::notify(id, SUMMARY.into(), None, true).unwrap();
        {
            loop {
                let event = events
                    .next()
                    .await
                    .expect("the notification wakes the session");
                if matches!(event.event, AgentEvent::Error { .. }) {
                    break;
                }
            }
        }
        assert!(!handle.task.is_finished(), "the session is still up");
    }
}
