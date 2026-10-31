//! Multi-session supervisor: every session owns an `App` + `AgentHandles` and
//! keeps draining agent events while backgrounded; only the focused session
//! renders and receives input. `SpawnCtx` carries the shared resources needed
//! to spawn session runtimes at any point.
//!
//! Terminal input arrives on a channel (see [`InputReader`]), so the loop
//! waits on every event source at once and wakes the moment a plugin action,
//! agent event, or keypress arrives instead of sleeping in `event::poll`.

use std::cell::Cell;
use std::mem;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::eyre;
use tokio::runtime::Handle;

use crate::EventHandle;
use crossterm::event::{
    Event, KeyEvent, KeyEventKind, MouseButton, MouseEvent as CtMouseEvent, MouseEventKind,
};
use maki_config::AgentConfig;
use maki_config::project::TrustQuestion;
use maki_config::{ProjectConfig, UiConfig};
use maki_lua_core::agent_autocmd::HostEvent;
use maki_lua_core::pack::{PackCommand, PackContext, PackPreparation};
use maki_lua_ui::{
    HintReader, InputMode, InputRequest, InputRulesReader, Key, KeymapReader, LuaCommandReader,
    ModelRequest, PlanRequest, SessionRequest, TabRef, TaskRequest, UiAction, UiAttachment,
    UiReply, WinEvent,
};
use maki_types::message::Message;
use maki_types::thinking::ThinkingConfig;

use maki_storage::StateDir;
use maki_storage::id::{MakiId, MakiIdParseError};
use maki_storage::input_history::InputHistory;
use maki_storage::model::persist_model;
use maki_storage::sessions::normalize_title;
use maki_types::cancel::CancelToken;
use maki_types::command::CustomCommand;
use maki_types::event::{AgentEvent, Envelope, SessionEndReason};
use maki_types::session_snapshot::{
    MODE_BUILD, MODE_PLAN, STATUS_IDLE, STATUS_NEEDS_INPUT, STATUS_WORKING,
};
use maki_types::transcript::{SessionExport, Transcript};
use maki_wire::client::ClientEvent;
use maki_wire::configuration::Key as ConfigurationKey;
use maki_wire::methods::MODEL_CONFIG_ID;
use maki_wire::session::SessionOperation;
use ratatui::backend::Backend;
use ratatui::layout::Rect;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::agent::capabilities::{AgentBackend, Feature, UNAVAILABLE_REMOTE};
use crate::agent::remote_facade;
use crate::agent::remote_settings::Setting;
use crate::agent::{
    AgentHandles,
    shared_queue::{Compaction, QueueItem, QueuedInput},
};
use crate::agent::{RemoteAgent, remote_replay, spawn_remote};
use crate::app::mode::Mode;
use crate::app::remote_drafts::RemoteDrafts;
use crate::app::shell::{ShellEvent, spawn_shell};
use crate::app::tasks::TaskWatch;
use crate::app::{App, Msg, Notification, QueuedMessage, SubmitOutcome, turn_response};
use crate::color_compat;
use crate::components::input::Submission;
use crate::components::keybindings::key;
use crate::components::{Action, ExitRequest, Status};
use crate::decor_worker;
use crate::input::InputReader;
use crate::repaint::{Dirty, IDLE_POLL};
use crate::theme;
use crate::{AppSession, OpenSession};

use crate::terminal;

/// Max events handled per frame so a flood cannot starve rendering.
const DRAIN_BUDGET: usize = 256;
const AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

const NOT_LIVE_ERR: &str = "session not live";
const PACK_PREPARING: &str = "Checking packages...";
const LOGIN_DONE: &str = "Logged in: the agent uses it from the next prompt";
const LOGIN_UNANNOUNCED: &str = "The agent could not be told the login ran";
const LOGIN_FAILED: &str = "The agent's login did not finish";
const PACK_BUSY_ERR: &str = "a package command is already running";
const UNKNOWN_MODE_ERR: &str = "unknown mode; expected \"build\" or \"plan\"";
const PACK_PANIC_ERR: &str = "the package command stopped unexpectedly";
const FORKED_MSG: &str = "Forked, {key} to switch back";

/// Tabs carry their in-memory sessions so `/reload` reopens them without a
/// disk round-trip; `session_has_content` tells which ones were saved.
pub(crate) struct ShutdownReport {
    pub exit: ExitRequest,
    pub tabs: Vec<OpenSession>,
    pub focused: usize,
    /// Sessions whose transcript never reached disk. The caller prints them
    /// after dropping the terminal guard, so the line is not lost with the
    /// alternate screen.
    pub unsaved: Vec<MakiId>,
}

pub type AgentFactory = Arc<dyn Fn(&OpenSession) -> Result<RemoteAgent> + Send + Sync>;

pub type PackPreparer = Arc<dyn Fn(&PackCommand, PackContext) -> PackPreparation + Send + Sync>;

pub struct EventLoopParams {
    pub agent_factory: Option<AgentFactory>,
    /// Native maki reviews packages with both runtimes before reloading them.
    pub prepare_pack: PackPreparer,
    pub needs_login: bool,
    pub commands: Vec<CustomCommand>,
    pub sessions: Vec<OpenSession>,
    pub focused: usize,
    pub startup_warnings: Vec<String>,
    pub startup_notice: Option<String>,
    /// Shown in a popup that stays up until dismissed, not a flash.
    pub startup_alert: Option<String>,
    pub storage: StateDir,
    pub config: AgentConfig,
    pub ui_config: UiConfig,
    pub input_history_size: usize,
    pub yolo: bool,
    pub exit_on_done: bool,
    /// How long `exit_on_done` waits for background work, `None` for not at all.
    pub background_wait: Option<Duration>,
    pub lua_command_reader: LuaCommandReader,
    pub keymap_reader: KeymapReader,
    pub hint_reader: HintReader,
    pub input_rules_reader: InputRulesReader,
    pub ui_action_rx: flume::Receiver<UiAction>,
    pub ui_wake_rx: flume::Receiver<()>,
    pub ui_attachment: UiAttachment,
    pub lua_event_handle: EventHandle,
    pub login_catalog: Option<crate::LoginCatalog>,
    pub project_config: ProjectConfig,
    /// What `/trust` would grant and what the `[restricted]` indicator reports.
    /// `None` when the folder is trusted or has nothing to ask about.
    pub trust_question: Option<TrustQuestion>,
    /// Where `/trust` records its grant: {storage} in maki, the user's own
    /// store in a client, whose {storage} is scratch.
    pub trust_store: StateDir,
    /// The first tab's agent runs elsewhere over this connection. No local
    /// provider, MCP server or session store serves it.
    pub remote: Option<RemoteAgent>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionStatus {
    Working,
    NeedsInput,
    Idle,
}

enum PendingCompletion {
    WaitingForQueueDrain(Notification),
    Due(Notification),
}

#[derive(Default)]
struct RunNotificationState {
    response_candidate: Option<String>,
    pending_completion: Option<PendingCompletion>,
    last_attention: Option<Notification>,
}

impl RunNotificationState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn on_queue_item_consumed(&mut self) {
        self.response_candidate = None;
        self.pending_completion = None;
    }

    fn on_turn_complete(&mut self, message: &Message) {
        self.response_candidate = turn_response(message);
    }

    fn on_done(&mut self, event: &AgentEvent) {
        let notification = match event {
            AgentEvent::Done { .. } => Notification::TurnComplete {
                response: self.response_candidate.take(),
            },
            AgentEvent::Error { .. } => {
                self.response_candidate = None;
                Notification::error_completion()
            }
            _ => return,
        };
        self.pending_completion = Some(PendingCompletion::WaitingForQueueDrain(notification));
    }

    fn on_drain(&mut self) {
        self.pending_completion = match self.pending_completion.take() {
            Some(PendingCompletion::WaitingForQueueDrain(notification)) => {
                Some(PendingCompletion::Due(notification))
            }
            pending => pending,
        };
    }

    fn on_manual_exit(&mut self) {
        self.pending_completion = None;
    }

    /// True between `Done`/`Error` and the run's `QueueDrained`. An exit must
    /// not fire in that window: a queued follow-up may still start a new run.
    fn waiting_for_drain(&self) -> bool {
        matches!(
            self.pending_completion,
            Some(PendingCompletion::WaitingForQueueDrain(_))
        )
    }

    fn reconcile(
        &mut self,
        attention: Option<Notification>,
        status: SessionStatus,
        queue_empty: bool,
    ) -> Option<Notification> {
        let settled = attention.is_none() && status == SessionStatus::Idle && queue_empty;
        let prompt = (attention != self.last_attention)
            .then(|| attention.clone())
            .flatten();
        self.last_attention = attention;

        // A due completion is decided on its first reconcile: fire if the
        // session settled, otherwise drop it for good.
        let completion = match self.pending_completion.take() {
            Some(PendingCompletion::Due(notification)) => settled.then_some(notification),
            waiting => {
                self.pending_completion = waiting;
                None
            }
        };
        prompt.or(completion)
    }
}

impl SessionStatus {
    fn of(app: &App) -> Self {
        if app.awaiting_input() {
            Self::NeedsInput
        } else if app.status == Status::Streaming {
            Self::Working
        } else {
            Self::Idle
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Working => STATUS_WORKING,
            Self::NeedsInput => STATUS_NEEDS_INPUT,
            Self::Idle => STATUS_IDLE,
        }
    }
}

fn prepend_preamble(preamble: &mut Vec<Message>, mut leading: Vec<Message>) {
    leading.append(preamble);
    *preamble = leading;
}

fn is_current_top_level(current_run_id: u64, envelope: &Envelope) -> bool {
    envelope.run_id == current_run_id && envelope.subagent.is_none()
}

fn select_notification(
    selected: Option<Notification>,
    candidate: Option<Notification>,
) -> Option<Notification> {
    match (selected, candidate) {
        (Some(current), Some(candidate)) if candidate.is_urgent() && !current.is_urgent() => {
            Some(candidate)
        }
        (selected @ Some(_), _) => selected,
        (None, candidate) => candidate,
    }
}

/// Maki never turns focus reporting on under Windows, so anything that looks
/// like a focus record there is a guess rather than a report.
const TRUSTS_FOCUS_EVENTS: bool = !cfg!(windows);

/// How long input keeps an unproven terminal counting as watched. Short on
/// purpose: suppressing wrongly hides a finished turn, notifying wrongly only
/// costs a bell.
const INPUT_IMPLIES_WATCHING: Duration = Duration::from_secs(30);

/// Whether the user is watching this terminal.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Focus {
    /// No focus report has arrived yet. Terminals that never send one (GNU
    /// screen, tmux without `focus-events on`) stay here for good, and one
    /// never arrives while the user simply keeps the window focused, so
    /// recent input is the only evidence available.
    Unproven {
        last_input: Option<Instant>,
    },
    Focused,
    Unfocused,
}

impl Default for Focus {
    fn default() -> Self {
        Self::Unproven { last_input: None }
    }
}

impl Focus {
    /// A prompt parks the agent on the user, so it rings even while they
    /// watch. A finished turn they can already see is just noise.
    fn allows(self, notification: &Notification) -> bool {
        notification.is_urgent() || !self.is_watched()
    }

    fn is_watched(self) -> bool {
        match self {
            Self::Focused => true,
            Self::Unfocused => false,
            Self::Unproven { last_input } => {
                last_input.is_some_and(|at| at.elapsed() < INPUT_IMPLIES_WATCHING)
            }
        }
    }

    fn report(&mut self, reported: Self) {
        if TRUSTS_FOCUS_EVENTS {
            *self = reported;
        }
    }

    /// Typing proves the user was here just now. It never latches: without a
    /// report to clear it, a terminal that cannot send `FocusLost` would go
    /// quiet for good, so the evidence expires on its own.
    fn note_input(&mut self) {
        match self {
            Self::Unfocused => *self = Self::Focused,
            Self::Unproven { last_input } => *last_input = Some(Instant::now()),
            Self::Focused => {}
        }
    }

    /// An editor, a shell or a suspend eats the focus reports we would have
    /// seen, so assume the user may have walked away.
    fn on_resume(&mut self) {
        match self {
            Self::Focused => *self = Self::Unfocused,
            Self::Unproven { last_input } => *last_input = None,
            Self::Unfocused => {}
        }
    }
}

fn parse_session_id(id: &str) -> Result<MakiId, String> {
    id.parse().map_err(|e: MakiIdParseError| e.to_string())
}

/// A remote tab answers to its agent's session id, which is tried before
/// {id} is read as a local one. `Ok(Err(id))` is a local id no tab has open.
fn find_tab(sessions: &[SessionRuntime], id: &str) -> Result<Result<usize, MakiId>, String> {
    if let Some(idx) = sessions
        .iter()
        .position(|rt| rt.app.remote_session_id().as_deref() == Some(id))
    {
        return Ok(Ok(idx));
    }
    let id = parse_session_id(id)?;
    Ok(sessions.iter().position(|rt| rt.id() == id).ok_or(id))
}

/// No session means the focused tab. A session that is open in no tab
/// has none, and the request should not land on some other tab instead.
fn tab_of(sessions: &[SessionRuntime], focused: usize, session: Option<&TabRef>) -> Option<usize> {
    match session {
        None => Some(focused),
        Some(TabRef(name)) => find_tab(sessions, name).ok()?.ok(),
    }
}

/// A plugin holding the id of a tab that has since closed gets
/// `session not live` back, so it knows to stop.
fn resolve_tab(
    sessions: &[SessionRuntime],
    focused: usize,
    id: Option<&str>,
) -> Result<usize, String> {
    match id {
        None => Ok(focused),
        Some(id) => find_tab(sessions, id)?.map_err(|_| NOT_LIVE_ERR.into()),
    }
}

/// Route a plan-surface request to the session it names, or the focused
/// one when it names none. Plan state is per session, so a plugin woken by a
/// background tab's `PlanReady` would otherwise read the wrong plan.
fn plan_reply(sessions: &[SessionRuntime], focused: usize, req: PlanRequest) -> UiReply {
    let idx = resolve_tab(sessions, focused, req.session.as_deref())?;
    Ok(sessions[idx].app.plan_snapshot())
}

/// What a remote tab cannot answer until the agent can be asked for it:
/// the agent owns its sessions, so the tab speaks only for itself.
fn remote_refuses(req: &SessionRequest) -> bool {
    matches!(
        req,
        SessionRequest::Delete { .. }
            | SessionRequest::SetTitle { .. }
            | SessionRequest::New { .. }
    )
}

fn parse_mode(mode: &str) -> Result<Mode, String> {
    match mode {
        MODE_BUILD => Ok(Mode::Build),
        MODE_PLAN => Ok(Mode::Plan),
        other => Err(format!("{UNKNOWN_MODE_ERR}, got {other:?}")),
    }
}

struct SessionRuntime {
    app: App,
    handles: AgentHandles,
    shell_tx: flume::Sender<ShellEvent>,
    shell_rx: flume::Receiver<ShellEvent>,
    last_status: SessionStatus,
    last_title: String,
    last_tasks: TaskWatch,
    notifications: RunNotificationState,
}

impl SessionRuntime {
    fn id(&self) -> MakiId {
        self.app.state.session.id
    }

    /// What the facade calls the tab: its agent's session id when remote.
    fn name(&self) -> String {
        self.app
            .remote_session_id()
            .unwrap_or_else(|| self.id().to_string())
    }

    /// New work cancels an `exit_on_done` exit still waiting on its drain.
    fn reset_run_notifications(&mut self) {
        if self.notifications.waiting_for_drain() {
            self.app.clear_exit_request();
        }
        self.notifications.reset();
    }
}

/// Everything needed to bring up a new session runtime after startup.
struct SpawnCtx {
    agent_factory: Option<AgentFactory>,
    prepare_pack: PackPreparer,
    storage: StateDir,
    config: AgentConfig,
    ui_config: UiConfig,
    input_history_size: usize,
    yolo: bool,
    custom_commands: Arc<[CustomCommand]>,
    lua_command_reader: LuaCommandReader,
    keymap_reader: KeymapReader,
    hint_reader: HintReader,
    input_rules_reader: InputRulesReader,
    lua_event_handle: EventHandle,
    login_catalog: Option<crate::LoginCatalog>,
    trust_question: Option<TrustQuestion>,
    trust_store: StateDir,
    /// Taken by the first runtime. A remote connection serves one tab.
    remote: Cell<Option<RemoteAgent>>,
}

impl SpawnCtx {
    fn spawn_runtime(&self, open: OpenSession) -> Result<SessionRuntime> {
        let session = &open.session;
        let resumed = !session.messages().is_empty();
        let yolo = self.yolo;
        let remote = match self.remote.take() {
            Some(remote) => Some(remote),
            None => self
                .agent_factory
                .as_ref()
                .map(|open_agent| open_agent(&open))
                .transpose()?,
        };
        let mut remote = remote.ok_or_else(|| eyre!("no agent connection"))?;
        let input = remote.input.take();
        let mut snapshot = None;
        if remote.extensions.sessions {
            for event in mem::take(&mut remote.early) {
                match event {
                    ClientEvent::Restored(session) => snapshot = Some(session),
                    event => remote.early.push(event),
                }
            }
        }
        let replay = snapshot.is_none().then(|| {
            (
                mem::take(&mut remote.replay),
                remote.extensions.clone(),
                remote.unreadable,
            )
        });
        let backend = AgentBackend::Remote {
            native: self.agent_factory.is_some(),
            host: remote.extensions.sessions,
            images: remote.images,
            plan: remote.plan_modes,
            settings: remote.extensions.settings,
            btw: remote.extensions.btw,
            usage_ledger: remote.extensions.usage_ledger,
            login: remote.login.is_some() && remote.extensions.login,
            plan_file: remote.extensions.plan,
            reload: remote.reload,
        };
        let handles = spawn_remote(
            remote,
            replay
                .as_ref()
                .and_then(|(replay, extensions, _)| remote_replay::seed(replay, extensions)),
            session.cwd.clone().into(),
            self.lua_event_handle.clone(),
        );
        let mut app = App::new(
            open,
            self.storage.clone(),
            maki_types::mcp::McpSnapshotReader::empty(),
            self.lua_command_reader.clone(),
            self.keymap_reader.clone(),
            self.hint_reader.clone(),
            self.input_rules_reader.clone(),
            self.ui_config.clone(),
            self.input_history_size,
            yolo,
            Arc::clone(&self.custom_commands),
            self.lua_event_handle.clone(),
            self.login_catalog.clone(),
        );
        app.trust_question = self.trust_question.clone();
        app.trust_store = self.trust_store.clone();
        app.backend = backend;
        handles.apply_to_app(&mut app);
        app.seed_remote_settings();
        if let Some(input) = input {
            let max_entries = app.input_box.history().max_entries();
            app.input_box
                .set_history(InputHistory::load_file(input.history, max_entries));
            app.remote_drafts = Some(RemoteDrafts::new(
                input.store,
                handles.remote_link.clone(),
                input.resumed,
            ));
        }
        if self.agent_factory.is_some() {
            let meta = &app.state.session.meta;
            app.remote_drafts = Some(RemoteDrafts::native(
                maki_storage::remote_drafts::RemoteStore::open(&self.storage, "maki"),
                handles.remote_link.clone(),
                maki_storage::remote_drafts::RemoteDraft {
                    input_draft: meta.input_draft.clone(),
                    input_draft_writers: meta.input_draft_writers,
                    queued_messages: Vec::new(),
                },
            ));
        }
        if resumed {
            app.restore_display();
        }
        if let Some(snapshot) = snapshot {
            app.restore_remote_transcript(*snapshot);
        }
        if let Some((replay, extensions, unreadable)) = replay {
            app.draw_remote_replay(replay, &extensions, unreadable);
        }
        if let Some(draft) = app.remote_drafts.as_ref().map(RemoteDrafts::restored) {
            app.restore_remote_input(draft);
        }
        let (shell_tx, shell_rx) = flume::unbounded::<ShellEvent>();
        let last_title = app.state.session.title.clone();
        Ok(SessionRuntime {
            app,
            handles,
            shell_tx,
            shell_rx,
            last_status: SessionStatus::Idle,
            last_title,
            last_tasks: TaskWatch::default(),
            notifications: RunNotificationState::default(),
        })
    }
}

pub(crate) struct EventLoop<'t> {
    terminal: &'t mut ratatui::DefaultTerminal,
    sessions: Vec<SessionRuntime>,
    focused: usize,
    /// The `(session, task)` pair whose transcript was on screen last frame.
    last_focus: Option<(MakiId, Arc<str>)>,
    /// The mode `ModeChanged` last announced, one for all tabs.
    last_mode: InputMode,
    focus: Focus,
    notifier: Option<terminal::TerminalNotifier>,
    ctx: SpawnCtx,
    input: InputReader,
    ui_action_rx: flume::Receiver<UiAction>,
    ui_wake_rx: flume::Receiver<()>,
    ui_attachment: UiAttachment,
    pack_tx: flume::Sender<Box<PackPreparation>>,
    pack_rx: flume::Receiver<Box<PackPreparation>>,
    /// One package command at a time. The work runs on its own thread, so
    /// without this a second `/packupdate` would race the first over the same
    /// clones and locks.
    pack_running: bool,
}

/// One item from any of the event loop's sources; `None` from `next_wake`
/// means the wait timed out (animation/idle tick).
enum Wake {
    Input(Event),
    InputGone,
    Ui(UiAction),
    /// Nothing to handle. The tick after every wake is what reads plugin
    /// windows.
    PluginWindow,
    /// Nothing to handle either. The tick takes in what the worker finished
    /// for the chat input.
    Decor,
    Agent(usize, Box<maki_types::event::Envelope>),
    Shell(usize, ShellEvent),
    Pack(Box<PackPreparation>),
}

impl<'t> EventLoop<'t> {
    pub(crate) fn new(
        terminal: &'t mut ratatui::DefaultTerminal,
        params: EventLoopParams,
    ) -> Result<Self> {
        let EventLoopParams {
            needs_login,
            commands,
            sessions,
            focused,
            mut startup_warnings,
            startup_notice,
            startup_alert,
            storage,
            config,
            ui_config,
            input_history_size,
            yolo,
            exit_on_done,
            background_wait,
            lua_command_reader,
            keymap_reader,
            hint_reader,
            input_rules_reader,
            ui_action_rx,
            ui_wake_rx,
            ui_attachment,
            lua_event_handle,
            login_catalog,
            project_config: _,
            trust_question,
            trust_store,
            remote,
            agent_factory,
            prepare_pack,
        } = params;
        // A `/reload` generation inherits the handles of the one before it,
        // so every loop has to claim the UI back for itself.
        ui_attachment.attach();

        // Apply the config theme before the warmup thread spawns, or warmup
        // could bake the syntax palette from the old theme. Only the
        // in-memory name is set, so the user's saved pick survives.
        if let Some(ref name) = ui_config.theme {
            match crate::theme::load_by_name(name) {
                Ok(theme) => {
                    crate::theme::set_current_name(name);
                    crate::theme::set(theme);
                }
                Err(e) => startup_warnings.push(format!("config ui.theme: {e}")),
            }
        }

        static PROCESS_WARMUP: std::sync::Once = std::sync::Once::new();
        PROCESS_WARMUP.call_once(|| {
            maki_highlight::pool::spawn(crate::highlight::warmup);
            crate::update::spawn_check();
        });

        let notifier = terminal::TerminalNotifier::new(ui_config.notifications);
        let ctx = SpawnCtx {
            agent_factory,
            prepare_pack,
            storage,
            config,
            ui_config,
            input_history_size,
            yolo,
            custom_commands: Arc::from(commands),
            lua_command_reader,
            keymap_reader,
            hint_reader,
            input_rules_reader,
            lua_event_handle,
            login_catalog,
            trust_question,
            trust_store,
            remote: Cell::new(remote),
        };

        let mut runtimes: Vec<SessionRuntime> = sessions
            .into_iter()
            .map(|open| ctx.spawn_runtime(open))
            .collect::<Result<_>>()?;
        if runtimes.is_empty() {
            return Err(eyre!("event loop needs at least one session"));
        }
        let focused = focused.min(runtimes.len() - 1);
        let app = &mut runtimes[focused].app;
        app.exit_on_done = exit_on_done;
        app.background_wait = background_wait;
        if needs_login {
            app.login_picker.open(app.storage.clone());
        }

        if let Some(notice) = startup_notice {
            app.queue_flash(notice);
        }
        for warning in startup_warnings {
            app.queue_flash(warning);
        }
        if let Some(alert) = startup_alert {
            app.open_alert(alert);
        }

        let (pack_tx, pack_rx) = flume::unbounded();
        Ok(Self {
            terminal,
            sessions: runtimes,
            focused,
            last_focus: None,
            last_mode: InputMode::Insert,
            focus: Focus::default(),
            notifier,
            ctx,
            input: InputReader::spawn(),
            ui_action_rx,
            ui_wake_rx,
            ui_attachment,
            pack_tx,
            pack_rx,
            pack_running: false,
        })
    }

    fn focused_app(&mut self) -> &mut App {
        &mut self.sessions[self.focused].app
    }

    /// Paints a frame and parks the terminal cursor on the cell the input box
    /// reversed, without showing it. macOS anchors IME preedit text to the
    /// cursor, so it has to sit there, but a visible one would invert that
    /// already reversed cell back to plain text and ride along with every cell
    /// the next diff writes.
    ///
    /// `Frame::set_cursor_position` cannot do this: ratatui shows the cursor
    /// whenever a frame asks for a position. Hence the move after the draw,
    /// and the hide that goes with it, so a widget that does ask for one
    /// cannot bring the block cursor back.
    fn paint(&mut self) -> Result<()> {
        let app = &mut self.sessions[self.focused].app;
        let mut cursor = None;
        self.terminal.draw(|f| {
            cursor = app.view(f);
            color_compat::downgrade_if_needed(f.buffer_mut());
        })?;
        if let Some(pos) = cursor {
            self.terminal.hide_cursor()?;
            self.terminal.set_cursor_position(pos)?;
            self.terminal.backend_mut().flush()?;
        }
        Ok(())
    }

    pub(crate) fn run(mut self, initial_prompt: Option<String>) -> Result<ShutdownReport> {
        if let Some(prompt) = initial_prompt {
            let sub = Submission {
                text: prompt,
                images: Vec::new(),
            };
            let actions = self.focused_app().handle_submit(sub);
            self.dispatch(self.focused, actions);
        }
        // The first frame always paints. After that only a poller, an event or
        // an animation tick owes another.
        let mut dirty = Dirty::YES;
        let result = loop {
            dirty |= self.tick();
            match self.drain_channels() {
                Ok(d) => dirty |= d,
                Err(e) => break Err(e),
            }
            self.checkpoint_all();
            if dirty.take()
                && let Err(e) = self.paint()
            {
                break Err(e);
            }

            if let Some(i) = self.sessions.iter().position(|rt| {
                rt.app.exit_request != ExitRequest::None
                    && !rt.notifications.waiting_for_drain()
                    && !rt.app.awaits_background(Instant::now())
            }) {
                // A result settling since the drain above both clears its
                // receipt and wakes the session, so the wake is claimed after
                // the read: its run goes first, and that run's drain exits.
                if self.sessions[i].app.exit_request != ExitRequest::None {
                    // A backgrounded session can finish an `exit_on_done` turn.
                    // Focus it so shutdown reports its exit code and id.
                    self.set_focused(i);
                    self.emit_notifications();
                    break Ok(());
                }
            }

            // Sleeping a whole frame instead of a fraction of one is what
            // makes a spinner cost 12 paints a second instead of 62.
            let cadence = self.sessions[self.focused].app.cadence();
            match self.next_wake(cadence.frame().unwrap_or(IDLE_POLL)) {
                // Any event can change the screen, so paint after handling it
                // rather than asking every handler to prove it did.
                Some(wake) => {
                    dirty = Dirty::YES;
                    if let Err(e) = self.handle_wake(wake) {
                        break Err(e);
                    }
                }
                // Only the clock moved, so motion alone owes the frame. The
                // cadence is the one from before the sleep, so motion that
                // just stopped still gets a last paint to clear itself off
                // the screen.
                None => dirty |= Dirty::from(cadence.moves()),
            }
        };
        // Fatal errors still save every session, kill MCP process groups,
        // and drain the storage writer before the process exits.
        let report = self.shutdown();
        result?;
        report
    }

    /// Wait for the next event from any source, or time out so animations
    /// and periodic polls keep running. `Duration::ZERO` drains whatever is
    /// already pending.
    fn next_wake(&self, timeout: Duration) -> Option<Wake> {
        let mut sel = flume::Selector::new().recv(self.input.receiver(), |res| match res {
            Ok(ev) => Some(Wake::Input(ev)),
            Err(_) => Some(Wake::InputGone),
        });
        if !self.ui_action_rx.is_disconnected() {
            sel = sel.recv(&self.ui_action_rx, |res| res.ok().map(Wake::Ui));
        }
        if !self.ui_wake_rx.is_disconnected() {
            sel = sel.recv(&self.ui_wake_rx, |res| {
                res.ok().map(|()| Wake::PluginWindow)
            });
        }
        sel = sel.recv(decor_worker::wakes(), |res| res.ok().map(|()| Wake::Decor));
        sel = sel.recv(&self.pack_rx, |res| res.ok().map(Wake::Pack));

        for (i, rt) in self.sessions.iter().enumerate() {
            if !rt.handles.agent_rx.is_disconnected() {
                sel = sel.recv(&rt.handles.agent_rx, move |res| {
                    res.ok().map(|env| Wake::Agent(i, Box::new(env)))
                });
            }
            sel = sel.recv(&rt.shell_rx, move |res| {
                res.ok().map(|ev| Wake::Shell(i, ev))
            });
        }
        sel.wait_timeout(timeout).ok().flatten()
    }

    fn handle_wake(&mut self, wake: Wake) -> Result<()> {
        match wake {
            Wake::Input(ev) => self.handle_input(ev),
            Wake::InputGone => return Err(eyre!("terminal input reader stopped")),
            Wake::Ui(action) => self.handle_ui_action(action),
            Wake::PluginWindow | Wake::Decor => {}
            Wake::Agent(i, envelope) => self.handle_agent(i, envelope),
            Wake::Shell(i, event) => self.sessions[i].app.handle_shell_event(event),

            Wake::Pack(preparation) => self.finish_pack(*preparation),
        }
        Ok(())
    }

    /// The one save trigger. A checkpoint writes only on a real change, so
    /// every tool result reaches disk within a frame while an idle session
    /// writes nothing.
    fn checkpoint_all(&mut self) {
        for rt in &mut self.sessions {
            rt.app.checkpoint();
        }
    }

    /// Only the focused session is drawn, so only it can owe a frame; focusing
    /// another is an event, and events always repaint. The rest get
    /// [`App::tick_background`].
    fn tick(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        // A quit ends the whole process, so each session's double press
        // counts the background tasks of all of them.
        let running: usize = self
            .sessions
            .iter()
            .map(|rt| rt.app.running_background_tasks().count())
            .sum();
        for rt in &mut self.sessions {
            rt.app.background_elsewhere = running - rt.app.running_background_tasks().count();
        }
        for (i, rt) in self.sessions.iter_mut().enumerate() {
            if i == self.focused {
                dirty |= rt.app.tick();
            } else {
                rt.app.tick_background();
            }
        }
        // A plan form row outlives the key press that picked it: its handler
        // runs on the Lua thread, and the built-in outcome the row kept
        // follows only once that answers.
        for i in 0..self.sessions.len() {
            let actions = std::mem::take(&mut self.sessions[i].app.pending_actions);
            self.dispatch(i, actions);
        }
        self.replay_typeahead();
        dirty
    }

    fn handle_agent(&mut self, idx: usize, envelope: Box<maki_types::event::Envelope>) {
        let rt = &mut self.sessions[idx];
        let current = is_current_top_level(rt.app.run_id, &envelope);
        match &envelope.event {
            AgentEvent::QueueDrained => {
                if current {
                    rt.notifications.on_drain();
                }
                return;
            }
            AgentEvent::QueueItemConsumed { .. } if current => {
                rt.notifications.on_queue_item_consumed();
                if rt.app.exit_on_done {
                    rt.app.clear_exit_request();
                }
            }
            AgentEvent::TurnComplete(turn) if current => {
                rt.notifications.on_turn_complete(&turn.message);
            }
            event if current => rt.notifications.on_done(event),
            _ => {}
        }
        let actions = self.sessions[idx].app.update(Msg::Agent(envelope));
        self.dispatch(idx, actions);
    }

    fn drain_channels(&mut self) -> Result<Dirty> {
        let mut dirty = Dirty::NO;
        // Leftovers beyond the budget are picked up right after the next draw.
        for _ in 0..DRAIN_BUDGET {
            match self.next_wake(Duration::ZERO) {
                Some(wake) => {
                    self.handle_wake(wake)?;
                    dirty = Dirty::YES;
                }
                None => break,
            }
        }

        // Only an announcer for Lua, not a second authority. Diffing is the
        // sturdiest way to fire the autocmd however the model got changed.
        for rt in &mut self.sessions {
            rt.app.emit_model_change();
        }

        // These only fire Lua autocmds. Anything a handler does comes back
        // as a `UiAction` on the next wake, which repaints then.
        self.emit_focus_changes();
        self.sessions[self.focused]
            .app
            .announce_mode(&mut self.last_mode);
        self.emit_status_changes();
        self.emit_title_changes();
        for rt in &mut self.sessions {
            rt.app.settle_stranded_parts();
        }
        self.emit_task_changes();
        self.emit_notifications();
        // A remote agent's task ends only when its link does. Its last
        // events are drained first, so they still reach the transcript.
        for rt in &mut self.sessions {
            if rt.app.backend.accepts_prompts()
                && rt.handles.is_finished()
                && rt.handles.agent_rx.is_empty()
            {
                rt.app.connection_lost();
                dirty = Dirty::YES;
            }
        }
        // An `exit_on_done` exit waits on `QueueDrained`; a dead agent loop
        // can never send it, so fail instead of hanging forever.
        if let Some(runtime) = self.sessions.iter().find(|rt| {
            rt.app.exit_request != ExitRequest::None
                && rt.notifications.waiting_for_drain()
                && rt.handles.is_finished()
                && rt.handles.agent_rx.is_empty()
        }) {
            return Err(eyre!(
                "agent for session {} stopped before queue drain",
                runtime.id()
            ));
        }
        Ok(dirty)
    }

    fn handle_ui_action(&mut self, action: UiAction) {
        match action {
            UiAction::Flash(msg) => {
                self.focused_app().flash(msg);
            }
            UiAction::SetWindowTitle(title) => {
                if let Err(error) = terminal::set_window_title(&title) {
                    warn!(%error, "failed to set window title");
                }
            }
            UiAction::OpenEditor { path, reply_tx } => {
                let code = self.open_editor(self.focused, &path);
                let _ = reply_tx.send(code);
            }
            UiAction::OpenWin {
                session,
                buf,
                config,
                focus,
                event_tx,
                cmd_rx,
            } => {
                let Some(idx) = tab_of(&self.sessions, self.focused, session.as_ref()) else {
                    debug!(session = ?session, "open_win for a session open in no tab, closed");
                    let _ = event_tx.try_send(WinEvent::Close);
                    return;
                };
                let app = &mut self.sessions[idx].app;
                app.float_mgr.open(buf, config, focus, event_tx, cmd_rx);
                if focus {
                    app.transition_plan(crate::app::mode::PlanTrigger::InteractivePrompt);
                }
            }
            UiAction::Session { req, reply_tx } => {
                self.handle_session_request(req, reply_tx);
            }
            UiAction::ExportSession { id, reply_tx } => {
                let result = self.sessions[self.focused]
                    .handles
                    .remote_link
                    .request::<AppSession>(|_| SessionOperation::Snapshot {
                        session_id: id.into(),
                    });
                let _ = reply_tx.send(result.map(SessionExport::stored));
            }
            UiAction::Model { req, reply_tx } => {
                self.handle_remote_model_request(req, reply_tx);
            }
            UiAction::Input { req, reply_tx } => {
                let (reply, actions) = self.handle_input_request(req);
                let _ = reply_tx.send(reply);
                self.dispatch(self.focused, actions);
            }
            UiAction::Task { req, reply_tx } => {
                let _ = reply_tx.send(self.handle_task_request(req));
            }
            UiAction::Plan { req, reply_tx } => {
                let _ = reply_tx.send(plan_reply(&self.sessions, self.focused, req));
            }
            UiAction::WinSaveView { reply_tx } => {
                let _ = reply_tx.send(self.focused_app().win_view());
            }
            UiAction::WinRestView { scroll_top } => {
                self.focused_app().scroll_to_row(scroll_top);
            }
            // Answered before dispatching, which can hold the loop for as long
            // as `$EDITOR` is open.
            UiAction::Builtin {
                action,
                caller,
                reply_tx,
            } => {
                let (handled, actions) = self.focused_app().run_action(action, Some(&caller));
                let _ = reply_tx.send(handled);
                self.dispatch(self.focused, actions);
            }
            // Only the focused tab can be holding keys. A tab that loses focus
            // hands them over in `set_focused`.
            UiAction::KeybindDone(ticket) => {
                let actions = self.focused_app().keybind_done(ticket);
                self.dispatch(self.focused, actions);
                self.replay_typeahead();
            }
            UiAction::KeybindFallback(ticket) => {
                let actions = self.focused_app().keybind_fallback(ticket);
                self.dispatch(self.focused, actions);
            }
            UiAction::PluginUnloaded(plugin) => {
                for rt in &mut self.sessions {
                    rt.app.clear_plugin_marks(&plugin);
                }
            }
            UiAction::RunCommand {
                cmdline,
                depth,
                caller,
                reply_tx,
            } => {
                // Answer before dispatching: the caller only waits on the name
                // resolving, and dispatch may take a while (or exit the app).
                match self.focused_app().run_cmdline(&cmdline, depth, &caller) {
                    Ok(actions) => {
                        let _ = reply_tx.send(Ok(()));
                        self.dispatch(self.focused, actions);
                    }
                    Err(e) => {
                        warn!(cmdline, depth, error = %e, "run_command refused");
                        let _ = reply_tx.send(Err(e));
                    }
                }
            }
        }
    }

    /// Exits with the editor's status code; `-1` (flashed on the session's
    /// app) when the editor could not be launched.
    /// The agent's plan, edited as a copy here and saved back at the
    /// revision it was read at.
    fn edit_remote_plan(&mut self, idx: usize) {
        let Some(plan) = self.sessions[idx].handles.remote_plan.clone() else {
            return;
        };
        let read = match plan.read() {
            Ok(read) => read,
            Err(error) => return self.sessions[idx].app.flash(error),
        };
        let outcome = {
            let _pause = self.input.pause();
            plan.edit(read, |copy| terminal::open_in_editor(copy, self.terminal))
        };
        self.focus.on_resume();
        self.sessions[idx].app.flash(outcome);
    }

    fn open_editor(&mut self, idx: usize, path: &std::path::Path) -> i32 {
        let result = {
            let _pause = self.input.pause();
            terminal::open_in_editor(path, self.terminal)
        };
        self.focus.on_resume();
        match result {
            Ok(code) => code,
            Err(e) => {
                self.sessions[idx].app.flash(e);
                -1
            }
        }
    }

    fn emit_status_changes(&mut self) {
        for (i, rt) in self.sessions.iter_mut().enumerate() {
            let status = SessionStatus::of(&rt.app);
            if status == rt.last_status {
                continue;
            }
            rt.last_status = status;
            rt.app.fire_session_event(HostEvent::SessionStatusChanged {
                title: &rt.app.state.session.title,
                status: status.as_str(),
                focused: i == self.focused,
            });
        }
    }

    /// `/rename` and title auto-generation change the title without touching
    /// the status diff above, so the title gets its own diff per frame.
    fn emit_title_changes(&mut self) {
        for (i, rt) in self.sessions.iter_mut().enumerate() {
            if rt.app.state.session.title == rt.last_title {
                continue;
            }
            rt.last_title = rt.app.state.session.title.clone();
            rt.app.fire_session_event(HostEvent::SessionTitleChanged {
                title: &rt.last_title,
                focused: i == self.focused,
            });
        }
    }

    /// One diff per frame covers every path that finishes, cancels or errors a
    /// chat, so none of them has to remember to fire an event.
    fn emit_task_changes(&mut self) {
        for rt in &mut self.sessions {
            let app = &rt.app;
            rt.last_tasks
                .diff(rt.app.state.session.id, rt.app.task_states(), |task| {
                    app.fire_session_event(HostEvent::TaskStatusChanged {
                        id: task.id,
                        name: task.name,
                        status: task.status.into(),
                        plugin: task.plugin.map(|plugin| &**plugin),
                    });
                });
        }
    }

    /// Called only after the cancel went out, so a handler that checks
    /// `maki.agent.list()` already finds a stopped plugin task gone.
    fn emit_task_stopped(&self, idx: usize, id: &str) {
        let app = &self.sessions[idx].app;
        let plugin = app
            .task_states()
            .find(|task| &**task.id == id)
            .and_then(|task| task.plugin);
        app.fire_session_event(HostEvent::TaskStopped {
            id,
            plugin: plugin.map(|plugin| &**plugin),
        });
    }

    fn emit_notifications(&mut self) {
        let Some(notifier) = &self.notifier else {
            return;
        };
        let mut selected = None;
        for rt in &mut self.sessions {
            let candidate = rt.notifications.reconcile(
                rt.app.attention(),
                rt.last_status,
                rt.handles.queue.is_empty(),
            );
            selected = select_notification(selected, candidate);
        }
        if let Some(notification) = selected.filter(|n| self.focus.allows(n))
            && let Err(error) = notifier.notify(&notification.message(), notification.reason())
        {
            warn!(notifier = ?notifier.notifier(), %error, "terminal notifications disabled after write failure");
            self.notifier = None;
        }
    }

    /// One diff per frame covers the session picker, the chat cycling keys
    /// and `maki.task.focus`, so none of them has to remember to fire an
    /// event. A session switch is a task switch too, so `TaskFocusChanged`
    /// always follows `SessionFocusChanged`.
    ///
    /// A switch also republishes the input the newly focused tab holds. Only
    /// the focused session ticks, so a background tab never diffs its own
    /// input, and without the announcement a handler keeps acting on the text
    /// of the tab it came from.
    fn emit_focus_changes(&mut self) {
        let rt = &self.sessions[self.focused];
        let current = (rt.id(), rt.app.active_task_id());
        if self.last_focus.as_ref() == Some(&current) {
            return;
        }
        let previous_session = self.last_focus.replace(current.clone()).map(|(id, _)| id);
        let (session_id, task_id) = current;
        let switched = previous_session.is_some() && previous_session != Some(session_id);
        if previous_session != Some(session_id) {
            rt.app.fire_session_event(HostEvent::SessionFocusChanged {
                previous_session_id: previous_session.map(|id| id.to_string()),
            });
        }
        rt.app
            .fire_session_event(HostEvent::TaskFocusChanged { id: &task_id });
        if switched {
            self.focused_app().announce_input();
        }
    }

    /// `List` replies from a background task (the scan can be slow); every
    /// other request is answered synchronously by the event loop, which owns
    /// the live runtimes. A remote tab's agent owns its sessions, so a remote
    /// tab lists the agent's and answers only for itself.
    fn handle_session_request(&mut self, req: SessionRequest, reply_tx: flume::Sender<UiReply>) {
        match req {
            SessionRequest::List => self.list_remote_sessions(reply_tx),
            SessionRequest::Read { id } => {
                let result = self.sessions[self.focused]
                    .handles
                    .remote_link
                    .request::<serde_json::Value>(|current| SessionOperation::Read {
                        session_id: id.map(Into::into).unwrap_or(current),
                    });
                let _ = reply_tx.send(result);
            }
            SessionRequest::SetTitle { id, title } => {
                let result = self.sessions[self.focused]
                    .handles
                    .remote_link
                    .request::<serde_json::Value>(|_| SessionOperation::SetTitle {
                        session_id: id.clone().into(),
                        title: title.clone(),
                    });
                if result.is_ok()
                    && let Some(rt) = self.sessions.iter_mut().find(|rt| rt.name() == id)
                {
                    rt.app
                        .state
                        .session_mut()
                        .set_title(normalize_title(&title));
                }
                let _ = reply_tx.send(result);
            }
            SessionRequest::Delete { id } => {
                let result = if self.sessions.iter().any(|rt| rt.name() == id) {
                    Err("Close the session tab before deleting it".into())
                } else {
                    self.sessions[self.focused]
                        .handles
                        .remote_link
                        .request::<serde_json::Value>(|_| SessionOperation::Delete {
                            session_id: id.into(),
                        })
                };
                let _ = reply_tx.send(result);
            }
            req if self.ctx.agent_factory.is_none() && remote_refuses(&req) => {
                let _ = reply_tx.send(Err(UNAVAILABLE_REMOTE.into()));
            }
            SessionRequest::Live => {
                let list: Vec<_> = self
                    .sessions
                    .iter()
                    .enumerate()
                    .map(|(i, rt)| {
                        json!({
                            "id": rt.name(),
                            "title": rt.app.state.session.title,
                            "status": SessionStatus::of(&rt.app).as_str(),
                            "updated_at": rt.app.state.session.updated_at,
                            "focused": i == self.focused,
                        })
                    })
                    .collect();
                let _ = reply_tx.send(Ok(json!(list)));
            }
            SessionRequest::Current => {
                let _ = reply_tx.send(Ok(json!(self.sessions[self.focused].name())));
            }
            SessionRequest::New {
                transcript,
                prompt,
                focus,
            } => self.create_session(*transcript, prompt, focus, reply_tx),
            SessionRequest::Prompt { id, text } => {
                let idx = match id {
                    None => Ok(self.focused),
                    Some(id) => find_tab(&self.sessions, &id)
                        .and_then(|tab| tab.map_err(|closed| format!("{NOT_LIVE_ERR}: {closed}"))),
                };
                let _ = reply_tx.send(idx.and_then(|idx| self.submit_text(idx, text)));
            }
            SessionRequest::Focus { id } => {
                let reply = match find_tab(&self.sessions, &id) {
                    Ok(Ok(idx)) => {
                        self.set_focused(idx);
                        Ok(())
                    }
                    _ if self.ctx.agent_factory.is_none() => Err(UNAVAILABLE_REMOTE.into()),
                    Ok(Err(id)) => self.focus_session(id),
                    Err(e) => Err(e),
                };
                let _ = reply_tx.send(reply.map(|()| json!(true)));
            }
            // Resolved by id like the rest: a plan form row asking for build
            // mode fires for the session whose plan landed, which may be in
            // the background.
            SessionRequest::SetMode { id, mode } => {
                let reply = parse_mode(&mode).and_then(|mode| {
                    let idx = resolve_tab(&self.sessions, self.focused, id.as_deref())?;
                    self.sessions[idx].app.set_mode(mode);
                    Ok(json!(true))
                });
                let _ = reply_tx.send(reply);
            }
        }
    }

    /// Every page of the agent's sessions, off the loop like a local list.
    fn list_remote_sessions(&self, reply_tx: flume::Sender<UiReply>) {
        let rt = &self.sessions[self.focused];
        let Some(link) = &rt.app.remote_link else {
            let _ = reply_tx.send(Err(UNAVAILABLE_REMOTE.into()));
            return;
        };
        let list = remote_facade::list_sessions(
            Arc::clone(&link.client),
            rt.app.state.session.cwd.clone().into(),
        );
        tokio::spawn(async move {
            let _ = reply_tx.send(list.await);
        });
    }

    /// The input a plugin reads and writes is the focused session's.
    ///
    /// An edit or a submit is answered against the terminal as it is now, not
    /// as the last frame found it: a batch of wakes runs between two paints,
    /// so the size the box would be drawn at has to be asked for here.
    fn handle_input_request(&mut self, req: InputRequest) -> (UiReply, Vec<Action>) {
        let reply = match req {
            InputRequest::Read => Ok(self.focused_app().input_snapshot()),
            InputRequest::Edit(edit) => {
                let area = self.terminal_area();
                self.focused_app().apply_input_edit(edit, area)
            }
            InputRequest::Decorate(req) => self.focused_app().decorate_input(req),
            InputRequest::Submit(req) => {
                let area = self.terminal_area();
                let version = req.version;
                return match self.focused_app().submit_input(req, area) {
                    Ok(actions) => (Ok(json!(true)), actions),
                    Err(err) => {
                        info!(version, error = %err, "input_submit refused");
                        (Err(err), Vec::new())
                    }
                };
            }
        };
        (reply, Vec::new())
    }

    fn terminal_area(&self) -> Rect {
        self.terminal.size().map(Rect::from).unwrap_or_default()
    }

    /// The focused remote tab's agent owns its model and settings, so the
    /// menus it published answer, and a change waits for the agent's answer
    /// off the loop. Its models are opaque values the next prompt switches
    /// to, so a spec is not taken here.
    fn handle_remote_model_request(&mut self, req: ModelRequest, reply_tx: flume::Sender<UiReply>) {
        let app = &self.sessions[self.focused].app;
        let settings = app.remote_settings.as_ref();
        let takes_settings = app.backend.takes_settings();
        let set = match req {
            ModelRequest::Get => Ok(app.model_state()),
            ModelRequest::Available => Ok(remote_facade::model_choices(settings)),
            ModelRequest::Set { spec: Some(_), .. } => Err(UNAVAILABLE_REMOTE.into()),
            ModelRequest::Set {
                spec: None,
                thinking,
                fast,
            } => {
                let thinking = thinking
                    .map(|input| {
                        ThinkingConfig::parse(input.trim(), app.state.thinking)
                            .map(|parsed| (Setting::Thinking, Value::String(parsed.to_string())))
                            .map_err(str::to_owned)
                    })
                    .transpose();
                let fast = fast.map(|on| (Setting::Fast, Value::Bool(on)));
                let changed = thinking.and_then(|thinking| {
                    let changes = thinking.into_iter().chain(fast).collect();
                    remote_facade::set_settings(settings, takes_settings, changes)
                });
                match changed {
                    Ok(answered) => {
                        tokio::spawn(async move {
                            let _ = reply_tx.send(answered.await);
                        });
                        return;
                    }
                    Err(e) => Err(e),
                }
            }
        };
        let _ = reply_tx.send(set);
    }

    fn handle_task_request(&mut self, req: TaskRequest) -> UiReply {
        match req {
            TaskRequest::List => Ok(json!(self.focused_app().tasks())),
            TaskRequest::Focus { id } => self.focused_app().focus_task(&id).map(|()| json!(true)),
            TaskRequest::Remove { id } => self.focused_app().remove_task(&id).map(|()| json!(true)),
            TaskRequest::Stop { id, session } => {
                let action = self.focused_app().stop_task_in_session(&id, &session)?;
                self.dispatch(self.focused, vec![action]);
                Ok(json!(true))
            }
        }
    }

    fn submit_text(&mut self, idx: usize, text: String) -> UiReply {
        let msg = QueuedMessage {
            text,
            images: Vec::new(),
        };
        match self.sessions[idx].app.submit_prompt(msg) {
            SubmitOutcome::Started(actions) => {
                self.dispatch(idx, actions);
                Ok(json!("started"))
            }
            SubmitOutcome::Queued => Ok(json!("queued")),
            SubmitOutcome::Rejected(e) => Err(e.into()),
        }
    }

    /// Without `prompt` or `focus` the session is only stored, and the reply
    /// waits for the write. Otherwise it opens in a tab, which saves it on its
    /// first checkpoint.
    fn create_session(
        &mut self,
        transcript: Transcript,
        prompt: Option<String>,
        focus: bool,
        reply_tx: flume::Sender<UiReply>,
    ) {
        if !self.focused_backend().supports(Feature::Sessions) {
            let _ = reply_tx.send(Err(UNAVAILABLE_REMOTE.into()));
            return;
        }
        let imported = self.sessions[self.focused]
            .app
            .remote_link
            .as_ref()
            .ok_or_else(|| UNAVAILABLE_REMOTE.to_owned())
            .and_then(|link| {
                link.request::<serde_json::Value>(|parent| SessionOperation::Import {
                    parent,
                    transcript: Box::new(transcript),
                })
            })
            .and_then(|reply| {
                reply["sessionId"]
                    .as_str()
                    .ok_or("missing session id".to_owned())?
                    .parse::<MakiId>()
                    .map_err(|error| error.to_string())
            })
            .and_then(|id| {
                OpenSession::load(id, &self.ctx.storage).map_err(|error| error.to_string())
            });
        let open = match imported {
            Ok(open) => open,
            Err(error) => {
                let _ = reply_tx.send(Err(error));
                return;
            }
        };
        if prompt.is_none() && !focus {
            let _ = reply_tx.send(Ok(json!(open.session.id)));
            return;
        }
        let runtime = match self.ctx.spawn_runtime(open) {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = reply_tx.send(Err(error.to_string()));
                return;
            }
        };
        let idx = self.push_runtime(runtime);
        let id = self.sessions[idx].id();
        maki_otel::emit::session_started(maki_otel::emit::START_FRESH, Some(&id.to_string()));
        if let Some(prompt) = prompt {
            let _ = self.submit_text(idx, prompt);
        }
        if focus {
            self.set_focused(idx);
        }
        let _ = reply_tx.send(Ok(json!(id)));
    }

    fn focused_backend(&self) -> AgentBackend {
        self.sessions[self.focused].app.backend
    }

    /// A remote runtime owns its one connection for the life of the tab, so
    /// it is never respawned and nothing forks or loads into it.
    fn refuses_sessions(&mut self, idx: usize) -> bool {
        let app = &mut self.sessions[idx].app;
        if app.backend.supports(Feature::Sessions) {
            return false;
        }
        app.flash(UNAVAILABLE_REMOTE.into());
        true
    }

    fn position(&self, id: MakiId) -> Option<usize> {
        self.sessions.iter().position(|rt| rt.id() == id)
    }

    /// The single place the focused tab changes. Keys held behind a key
    /// handler were typed at the keyboard, so they follow it to the new tab
    /// rather than replay into the one left behind. They keep waiting there:
    /// the handler that switched is usually not done, and its `KeybindDone`
    /// goes to the focused tab.
    fn set_focused(&mut self, idx: usize) {
        if idx == self.focused {
            return;
        }
        let left = mem::replace(&mut self.focused, idx);
        if let Ok([from, to]) = self.sessions.get_disjoint_mut([left, idx]) {
            from.app.hand_typeahead_to(&mut to.app);
        }
    }

    fn push_runtime(&mut self, rt: SessionRuntime) -> usize {
        self.sessions.push(rt);
        self.sessions.len() - 1
    }

    /// Focus a live session, or bring a stored one up: in place when the
    /// focused session is a blank idle one (nothing worth keeping), otherwise
    /// as a new runtime so the session you came from stays live.
    fn focus_session(&mut self, id: MakiId) -> Result<(), String> {
        if let Some(i) = self.position(id) {
            self.set_focused(i);
            return Ok(());
        }
        if !self.focused_backend().supports(Feature::Sessions) {
            return Err(UNAVAILABLE_REMOTE.into());
        }
        // A session another maki owns is refused here, where it costs a flash,
        // rather than at the first write, after a whole conversation.
        let open = OpenSession::load(id, &self.ctx.storage)
            .map_err(|e| format!("Failed to load session: {e}"))?;
        let focused = &self.sessions[self.focused];
        if SessionStatus::of(&focused.app) == SessionStatus::Idle && focused.app.is_blank() {
            let idx = self.focused;
            self.sessions[idx].app.apply_loaded_session(open);
            self.respawn_agent(idx, SessionEndReason::Load);
        } else {
            let idx = self.push_runtime(
                self.ctx
                    .spawn_runtime(open)
                    .map_err(|error| error.to_string())?,
            );
            self.set_focused(idx);
        }
        Ok(())
    }

    /// Handles one input event plus any leftover produced while coalescing
    /// bursts of scroll/drag events.
    fn handle_input(&mut self, raw: Event) {
        let mut pending = Some(raw);
        while let Some(ev) = pending.take() {
            let (msg, leftover) = self.translate(ev);
            if let Some(msg) = msg {
                let actions = self.sessions[self.focused].app.update(msg);
                self.dispatch(self.focused, actions);
            }
            pending = leftover;
        }
    }

    /// Only a real focus report may recolor the caret: resume heuristics and
    /// stray input prove nothing, and no FocusGained follows $EDITOR or fg.
    fn sync_cursor(&self) {
        theme::set_cursor_focused(self.focus != Focus::Unfocused);
    }

    fn translate(&mut self, raw: Event) -> (Option<Msg>, Option<Event>) {
        match raw {
            Event::FocusGained => {
                self.focus.report(Focus::Focused);
                self.sync_cursor();
                (None, None)
            }
            Event::FocusLost => {
                self.focus.report(Focus::Unfocused);
                self.sync_cursor();
                (None, None)
            }
            // The one place the host's view of a keypress is normalized, so
            // it can never disagree with the identity a plugin is handed. A
            // key no notation names passes through untouched: nothing to
            // normalize, and `text_buffer` still reads its SUPER bit.
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.focus.note_input();
                (
                    Some(Msg::Key(Key::from_event(key).map_or(key, KeyEvent::from))),
                    None,
                )
            }
            Event::Key(_) => (None, None),
            Event::Paste(text) => {
                self.focus.note_input();
                (Some(Msg::Paste(text)), None)
            }
            Event::Mouse(mouse) => {
                self.focus.note_input();
                self.translate_mouse(mouse)
            }
            _ => (None, None),
        }
    }

    fn translate_mouse(&mut self, mouse: CtMouseEvent) -> (Option<Msg>, Option<Event>) {
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let scroll_lines = self.focused_app().ui_config.mouse_scroll_lines;
                let (msg, leftover) = self.aggregate_scroll(mouse, scroll_lines);
                (Some(msg), leftover)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let (drag, leftover) = self.coalesce_drag(mouse);
                (Some(Msg::Mouse(drag)), leftover)
            }
            _ => (Some(Msg::Mouse(mouse)), None),
        }
    }

    /// Sums queued scroll events into one delta; the first non-scroll event
    /// drained along the way is returned so it isn't lost.
    fn aggregate_scroll(&self, first: CtMouseEvent, scroll_lines: u32) -> (Msg, Option<Event>) {
        let mut delta = scroll_delta(first.kind, scroll_lines);
        let mut leftover = None;
        while let Ok(next) = self.input.receiver().try_recv() {
            match next {
                Event::Mouse(m)
                    if matches!(
                        m.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) =>
                {
                    delta += scroll_delta(m.kind, scroll_lines);
                }
                other => {
                    leftover = Some(other);
                    break;
                }
            }
        }
        (
            Msg::Scroll {
                column: first.column,
                row: first.row,
                delta,
            },
            leftover,
        )
    }

    /// Keeps only the newest queued drag position; the first non-drag event
    /// drained along the way is returned so it isn't lost.
    fn coalesce_drag(&self, mut latest: CtMouseEvent) -> (CtMouseEvent, Option<Event>) {
        let mut leftover = None;
        while let Ok(next) = self.input.receiver().try_recv() {
            match next {
                Event::Mouse(m) if matches!(m.kind, MouseEventKind::Drag(MouseButton::Left)) => {
                    latest = m;
                }
                other => {
                    leftover = Some(other);
                    break;
                }
            }
        }
        (latest, leftover)
    }

    /// Held keys replay up to the next one with actions, and those run before
    /// the keys after it replay, in whichever tab has the keyboard by then.
    /// So the keys typed after `/fork` land in the fork. Every round replays
    /// at least one key, so this ends.
    fn replay_typeahead(&mut self) {
        loop {
            let actions = self.focused_app().replay_typeahead();
            if actions.is_empty() {
                return;
            }
            self.dispatch(self.focused, actions);
        }
    }

    fn dispatch(&mut self, idx: usize, actions: Vec<Action>) {
        for action in actions {
            self.handle_action(idx, action);
        }
    }

    fn respawn_agent(&mut self, idx: usize, reason: SessionEndReason) {
        if self.ctx.agent_factory.is_some() {
            if let Some(link) = &self.sessions[idx].app.remote_link
                && let Err(error) =
                    link.request::<serde_json::Value>(|session_id| SessionOperation::Close {
                        session_id,
                        reason: Some(reason),
                    })
            {
                self.sessions[idx].app.flash(error);
                return;
            }
            let rt = &self.sessions[idx];
            let open = OpenSession {
                session: (*rt.app.state.session).clone(),
                claim: rt.app.state.claim.clone(),
            };
            match self.ctx.spawn_runtime(open) {
                Ok(next) => {
                    let old = mem::replace(&mut self.sessions[idx], next);
                    old.handles.cancel_all();
                }
                Err(error) => self.sessions[idx].app.flash(error.to_string()),
            }
        } else {
            let rt = &mut self.sessions[idx];
            rt.reset_run_notifications();
            rt.handles.rebind(&mut rt.app);
        }
    }

    fn open_fork(&mut self, idx: usize, open: OpenSession) {
        if self.refuses_sessions(idx) {
            return;
        }
        let runtime = match self.ctx.spawn_runtime(open) {
            Ok(runtime) => runtime,
            Err(error) => {
                self.sessions[idx].app.flash(error.to_string());
                return;
            }
        };
        let fork = self.push_runtime(runtime);
        self.set_focused(fork);
        let id = self.sessions[self.focused].id();
        maki_otel::emit::session_started(maki_otel::emit::START_FRESH, Some(&id.to_string()));
        self.focused_app()
            .flash(FORKED_MSG.replace("{key}", key::SESSIONS.label));
    }

    fn handle_action(&mut self, idx: usize, action: Action) {
        match action {
            Action::SendMessage(input) => {
                let rt = &mut self.sessions[idx];
                rt.reset_run_notifications();
                let mut input = *input;
                prepend_preamble(&mut input.preamble, rt.app.shell.drain_results());
                let run_id = rt.app.run_id;
                rt.handles.queue.push(QueueItem::Message(QueuedInput {
                    text: input.message.clone(),
                    input,
                    run_id,
                    displayed: true,
                }));
            }
            Action::CancelAgent { run_id } => {
                let rt = &mut self.sessions[idx];
                rt.notifications.reset();
                rt.handles.cancel_run(run_id);
            }
            Action::CancelSubagent { tool_use_id } => {
                self.sessions[idx]
                    .handles
                    .cancel_subagent(tool_use_id.clone());
                self.emit_task_stopped(idx, &tool_use_id);
            }
            Action::CancelPluginTask { id, .. } => {
                if let Err(error) =
                    self.sessions[idx]
                        .handles
                        .remote_link
                        .request::<bool>(|session_id| SessionOperation::StopPluginTask {
                            session_id,
                            id: id.clone(),
                        })
                {
                    self.sessions[idx].app.flash(error);
                    return;
                }
                self.emit_task_stopped(idx, &id);
            }
            Action::RestartAgent => self.respawn_agent(idx, SessionEndReason::Reset),
            Action::Rewind(entry) => {
                let rt = &mut self.sessions[idx];
                rt.app.queue.clear();
                rt.handles.cancel_run(rt.app.run_id);
                match rt
                    .handles
                    .remote_link
                    .rewind(entry.turn_index, entry.prompt_text.clone())
                {
                    Ok(snapshot) => rt.app.finish_remote_rewind(snapshot, entry),
                    Err(error) => rt.app.flash(error),
                }
            }
            Action::OpenFork(open) => self.open_fork(idx, *open),
            Action::ChangeModel(spec) => {
                if let Err(e) = self.change_model(&spec) {
                    self.focused_app().flash(e);
                }
            }
            Action::RefreshProvider { slug } => self.refresh_provider(slug),
            Action::Compact(instructions) => {
                let rt = &mut self.sessions[idx];
                rt.reset_run_notifications();
                let run_id = rt.app.run_id;
                let workflow = rt.app.state.workflow;
                rt.handles.queue.push(QueueItem::Compact(Compaction {
                    run_id,
                    instructions,
                    workflow,
                }));
            }
            Action::ToggleMcp(server_name, enabled) => {
                if let Err(error) = self.sessions[idx].handles.send_mcp(server_name, enabled) {
                    self.sessions[idx].app.flash(error);
                }
            }
            Action::ShellCommand {
                id,
                command,
                visible,
            } => {
                let rt = &mut self.sessions[idx];
                let (trigger, cancel) = CancelToken::new();
                rt.app.shell.add_trigger(trigger);
                spawn_shell(
                    command,
                    id,
                    visible,
                    rt.shell_tx.clone(),
                    cancel,
                    self.ctx.config.clone(),
                );
            }
            Action::OpenEditor(path) => {
                self.open_editor(idx, &path);
            }
            Action::EditInputInEditor => {
                let current_text = self.sessions[idx].app.input_box.buffer.value();
                let result = {
                    let _pause = self.input.pause();
                    terminal::edit_temp_content(current_text, self.terminal)
                };
                self.focus.on_resume();
                match result {
                    Ok(edited) if edited == current_text => {}
                    Ok(edited) => self.sessions[idx].app.input_box.rewrite_input(&edited),
                    Err(e) => self.sessions[idx].app.flash(e),
                }
            }
            Action::RemoteLogin => self.remote_login(idx),
            Action::EditRemotePlan => self.edit_remote_plan(idx),
            Action::Btw(question) => {
                let rt = &mut self.sessions[idx];
                match rt.handles.remote_btw.clone() {
                    Some(remote) => rt.app.start_remote_btw(question, remote),
                    None => rt.app.flash(UNAVAILABLE_REMOTE.into()),
                }
            }
            Action::PreparePack(command) => self.start_pack(idx, command),
            Action::Suspend => {
                let _pause = self.input.pause();
                terminal::suspend(self.terminal);
                self.focus.on_resume();
            }
            Action::RefreshUsage => self.refresh_usage(),
            Action::ManualExit => self.sessions[idx].notifications.on_manual_exit(),
        }
    }

    /// The one explicit pick, which is why it alone writes the last-used file.
    /// Opening an old session must not change what the next `maki` starts on.
    fn change_model(&mut self, spec: &str) -> Result<(), String> {
        let app = &self.sessions[self.focused].app;
        if self.ctx.agent_factory.is_some()
            && let (Some(link), Some(configuration)) = (&app.remote_link, &app.remote_settings)
        {
            let key = ConfigurationKey::Option(MODEL_CONFIG_ID.into());
            let change = configuration.lock().desire(key, json!(spec), false);
            let answer = tokio::runtime::Handle::current()
                .block_on(
                    link.client
                        .configure(change.key.clone(), change.value.clone()),
                )
                .map_err(|error| error.to_string());
            let mut core = configuration.lock();
            let (result, _) = core.confirm(&change, answer);
            configuration.refresh(&core);
            result?;
        }
        let app = self.focused_app();
        app.state.session_mut().set_model(spec.to_owned());
        app.record_recent_model(spec);
        persist_model(&self.ctx.storage, spec);
        Ok(())
    }

    /// Prepares a package command on its own thread.
    ///
    /// Preparation fetches over the network through a git child process nothing
    /// here can cancel. Inline it would freeze drawing and input for as long as
    /// the slowest remote takes, so the answer comes back as an event instead.
    fn start_pack(&mut self, idx: usize, command: PackCommand) {
        if self.pack_running {
            self.sessions[idx].app.flash(PACK_BUSY_ERR.to_owned());
            return;
        }
        self.pack_running = true;
        self.sessions[idx].app.flash(PACK_PREPARING.to_owned());
        let handle = self.ctx.lua_event_handle.clone();
        let prepare = self.ctx.prepare_pack.clone();
        let tx = self.pack_tx.clone();
        let runtime = Handle::current();
        std::thread::spawn(move || {
            let _runtime = runtime.enter();
            // Without this a panic drops the sender with no answer, and
            // `pack_running` stays set for the rest of the process, so every
            // later package command reports "already running".
            let preparation =
                catch_unwind(AssertUnwindSafe(|| match handle.core.package_context() {
                    Ok(context) => prepare(&command, context),
                    Err(error) => PackPreparation::failed(error),
                }))
                .unwrap_or_else(|_| PackPreparation::failed(PACK_PANIC_ERR.to_owned()));
            let _ = tx.send(Box::new(preparation));
        });
    }

    /// The answer lands on the focused session, not on the one that asked: a
    /// package command reloads the whole Lua host, and only the focused session
    /// is read for an exit request or seen by whoever answers the review.
    fn finish_pack(&mut self, preparation: PackPreparation) {
        self.pack_running = false;
        let actions = self.focused_app().handle_pack_preparation(preparation);
        self.dispatch(self.focused, actions);
    }

    /// The agent's own login, run here; the agent then rebuilds what its
    /// old credentials connected.
    fn remote_login(&mut self, idx: usize) {
        let Some(login) = self.sessions[idx].handles.remote_login.clone() else {
            return;
        };
        let result = {
            let _pause = self.input.pause();
            terminal::run_login(&login.invocation, self.terminal)
        };
        self.focus.on_resume();
        let app = &mut self.sessions[idx].app;
        let status = match result {
            Ok(status) => status,
            Err(e) => return app.flash(e),
        };
        // Told after any exit: a login that failed late may have saved its
        // credentials already.
        if let Err(e) = login.client.auth_changed() {
            return app.flash(format!("{LOGIN_UNANNOUNCED}: {e}"));
        }
        match status.success() {
            true => app.flash(LOGIN_DONE.into()),
            false => app.flash(format!("{LOGIN_FAILED}: {status}")),
        }
    }

    fn refresh_usage(&mut self) {
        let rt = &mut self.sessions[self.focused];
        if let Some(remote) = rt.handles.remote_quota.clone() {
            rt.app.refresh_remote_quota(remote);
        }
    }

    /// Re-login rebuilds the provider for every tab on it, not just the focused
    /// one. A tab the user has not looked at would otherwise keep using the
    /// credentials that just failed.
    fn refresh_provider(&mut self, _slug: String) {
        self.focused_app().exit_request = ExitRequest::Reload;
    }

    fn shutdown(mut self) -> Result<ShutdownReport> {
        let started = Instant::now();
        let mut phase_start = started;
        let mut lap = || {
            let elapsed = phase_start.elapsed().as_millis() as u64;
            phase_start = Instant::now();
            elapsed
        };
        let exit = self.sessions[self.focused].app.exit_request.clone();
        // The loop already stopped draining `UiAction`, so say so before the
        // handlers run. Dropping the receiver does not, since the Lua runtime
        // holds one of its own, and a handler touching the UI would then park
        // until the shared deadline, taking every later tab's `SessionEnd`
        // down with it.
        self.ui_attachment.detach();
        // `/reload` hands these same sessions to the next generation, so
        // their handlers must not hear that the process is quitting.
        let reason = match exit {
            ExitRequest::Reload | ExitRequest::Plugins(_) | ExitRequest::Pack(_) => {
                SessionEndReason::Reload
            }
            _ => SessionEndReason::Shutdown,
        };
        self.ctx
            .lua_event_handle
            .core
            .end_sessions_blocking(self.sessions.iter().map(SessionRuntime::id), reason);
        let session_end_ms = lap();
        for rt in &self.sessions {
            rt.handles.cancel_all();
        }
        let kill_mcp_ms = lap();
        let mut tabs = Vec::with_capacity(self.sessions.len());
        let mut agent_tasks = Vec::with_capacity(self.sessions.len());
        for rt in self.sessions.drain(..) {
            let SessionRuntime {
                mut app, handles, ..
            } = rt;
            app.checkpoint_now();
            if self.ctx.agent_factory.is_some() {
                match handles.remote_link.request::<AppSession>(|session_id| {
                    SessionOperation::Close {
                        session_id,
                        reason: Some(reason),
                    }
                }) {
                    Ok(mut stored) => {
                        let input = app.unsent();
                        stored.meta.input_draft = input.input_draft;
                        stored.meta.input_draft_writers = input.input_draft_writers;
                        app.state.session = Arc::new(stored);
                    }
                    Err(error) => return Err(eyre!(error)),
                }
            }
            // `app` drops at the end of this iteration, closing the
            // channels the agent loop waits on, so `join_all` can finish.
            tabs.push(OpenSession {
                session: Arc::unwrap_or_clone(app.state.session),
                claim: app.state.claim,
            });
            agent_tasks.push(handles.into_task());
        }
        let save_sessions_ms = lap();
        crate::agent::join_all(agent_tasks, AGENT_SHUTDOWN_TIMEOUT);
        let join_agents_ms = lap();
        let mcp_shutdown_ms = lap();
        let unsaved = Vec::new();
        let storage_drain_ms = lap();
        info!(
            session_end_ms,
            kill_mcp_ms,
            save_sessions_ms,
            join_agents_ms,
            mcp_shutdown_ms,
            storage_drain_ms,
            total_ms = started.elapsed().as_millis() as u64,
            "ui shutdown phases"
        );
        Ok(ShutdownReport {
            exit,
            tabs,
            focused: self.focused,
            unsaved,
        })
    }
}

fn scroll_delta(kind: MouseEventKind, lines: u32) -> i32 {
    if kind == MouseEventKind::ScrollUp {
        lines as i32
    } else {
        -(lines as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::fake_agent::{Peer, SESSION, remote};
    use crate::app::tests::test_app;
    use maki_types::stop_reason::DoneReason;
    use maki_types::usage::TokenUsage;
    use test_case::test_case;

    const OBSERVATION: &str = "failed";
    const SHELL_RESULT: &str = "command finished";
    const NOT_AN_ID: &str = "not-a-session";

    /// The one tab a client runs, bound to the fake agent's `SESSION`.
    fn remote_tab() -> (SessionRuntime, Peer) {
        let (handles, peer) = remote();
        let mut app = test_app();
        handles.apply_to_app(&mut app);
        app.backend = AgentBackend::Remote {
            native: false,
            host: false,
            images: true,
            plan: true,
            settings: true,
            btw: false,
            usage_ledger: false,
            login: false,
            plan_file: false,
            reload: false,
        };
        let (shell_tx, shell_rx) = flume::unbounded();
        let runtime = SessionRuntime {
            app,
            handles,
            shell_tx,
            shell_rx,
            last_status: SessionStatus::Idle,
            last_title: String::new(),
            last_tasks: TaskWatch::default(),
            notifications: RunNotificationState::default(),
        };
        (runtime, peer)
    }

    /// `current` and the one `live` row name it so, which is how the
    /// `sessions` picker marks the agent's listed row live.
    #[test]
    fn a_remote_tab_is_named_by_its_agents_session() {
        let (tab, _peer) = remote_tab();
        assert_eq!(tab.name(), SESSION);
    }

    #[test]
    fn plan_read_accepts_the_agents_session_id() {
        let (tab, _peer) = remote_tab();
        let tabs = [tab];
        let read = PlanRequest {
            session: Some(SESSION.to_owned()),
        };
        assert_eq!(plan_reply(&tabs, 0, read), Ok(tabs[0].app.plan_snapshot()));
    }

    #[test]
    fn a_window_named_by_the_agents_session_opens_in_its_tab() {
        let (tab, _peer) = remote_tab();
        let named = TabRef(Arc::from(SESSION));
        assert_eq!(tab_of(&[tab], 0, Some(&named)), Some(0));
    }

    /// Its local id still names it, and anything else is not a tab.
    #[test]
    fn a_remote_tab_still_answers_to_its_local_id() {
        let (tab, _peer) = remote_tab();
        let local = tab.id();
        let tabs = [tab];
        let other = MakiId::generate();
        assert_eq!(find_tab(&tabs, &local.to_string()), Ok(Ok(0)));
        assert_eq!(find_tab(&tabs, &other.to_string()), Ok(Err(other)));
        assert!(find_tab(&tabs, NOT_AN_ID).is_err());
        assert_eq!(
            resolve_tab(&tabs, 0, Some(&other.to_string())),
            Err(NOT_LIVE_ERR.to_owned())
        );
    }

    #[test_case(SessionRequest::Delete { id: SESSION.into() }, true ; "delete")]
    #[test_case(SessionRequest::SetTitle { id: SESSION.into(), title: OBSERVATION.into() }, true ; "rename")]
    #[test_case(SessionRequest::Current, false ; "current")]
    #[test_case(SessionRequest::List, false ; "list")]
    fn a_remote_tab_refuses_what_only_its_agent_could_do(req: SessionRequest, refused: bool) {
        assert_eq!(remote_refuses(&req), refused);
    }

    fn done_event() -> AgentEvent {
        AgentEvent::Done {
            usage: TokenUsage::default(),
            cost: None,
            list_cost: None,
            context_size: 0,
            context_window: 0,
            num_turns: 1,
            reason: DoneReason::EndTurn,
        }
    }

    fn due_completion() -> RunNotificationState {
        let mut state = RunNotificationState::default();
        state.on_done(&done_event());
        state.on_drain();
        state
    }

    #[test]
    fn completion_waits_for_queue_drain() {
        let mut state = RunNotificationState {
            response_candidate: Some("done".into()),
            ..RunNotificationState::default()
        };

        state.on_done(&done_event());
        assert!(state.waiting_for_drain());
        assert_eq!(state.reconcile(None, SessionStatus::Idle, true), None);

        state.on_drain();
        assert!(!state.waiting_for_drain());
        assert_eq!(
            state.reconcile(None, SessionStatus::Idle, true),
            Some(Notification::TurnComplete {
                response: Some("done".into())
            })
        );
    }

    #[test_case(SessionStatus::Idle, true, true ; "fires_when_settled")]
    #[test_case(SessionStatus::Idle, false, false ; "queued_message_swallows")]
    #[test_case(SessionStatus::Working, true, false ; "busy_session_swallows")]
    fn due_completion_is_decided_on_first_reconcile(
        status: SessionStatus,
        queue_empty: bool,
        fires: bool,
    ) {
        let mut state = due_completion();

        let first = state.reconcile(None, status, queue_empty);
        assert_eq!(first.is_some(), fires);
        assert_eq!(state.reconcile(None, SessionStatus::Idle, true), None);
    }

    #[test]
    fn prompt_wins_over_completion_and_is_not_repeated_unchanged() {
        let prompt = Notification::QuestionRequested;
        let mut state = due_completion();

        assert_eq!(
            state.reconcile(Some(prompt.clone()), SessionStatus::Idle, true),
            Some(prompt.clone())
        );
        assert_eq!(
            state.reconcile(Some(prompt), SessionStatus::NeedsInput, true),
            None
        );
        assert_eq!(state.reconcile(None, SessionStatus::Idle, true), None);
    }

    #[test_case(Focus::Focused, false ; "watching_user_already_saw_the_turn_end")]
    #[test_case(Focus::Unfocused, true ; "away_from_the_terminal")]
    #[test_case(Focus::default(), true ; "unproven_terminal_without_input_notifies")]
    #[test_case(stale_input(), true ; "unproven_terminal_left_alone_notifies")]
    #[test_case(fresh_input(), false ; "unproven_terminal_typed_into_just_now")]
    fn focus_suppresses_only_turn_completions(focus: Focus, completion_fires: bool) {
        let completion = Notification::TurnComplete { response: None };

        assert_eq!(focus.allows(&completion), completion_fires);
        assert!(focus.allows(&Notification::QuestionRequested));
    }

    fn stale_input() -> Focus {
        Focus::Unproven {
            last_input: Instant::now().checked_sub(INPUT_IMPLIES_WATCHING),
        }
    }

    fn fresh_input() -> Focus {
        let mut focus = Focus::default();
        focus.note_input();
        focus
    }

    #[test]
    fn input_never_latches_on_an_unproven_terminal() {
        let mut focus = fresh_input();

        focus.on_resume();

        assert_eq!(focus, Focus::default());
    }

    #[cfg(not(windows))]
    #[test]
    fn a_focus_report_makes_input_and_resume_meaningful() {
        let mut focus = Focus::default();

        focus.report(Focus::Focused);
        assert_eq!(focus, Focus::Focused);

        focus.on_resume();
        assert_eq!(focus, Focus::Unfocused);

        focus.note_input();
        assert_eq!(focus, Focus::Focused);

        focus.report(Focus::Unfocused);
        assert_eq!(focus, Focus::Unfocused);
    }

    #[test]
    fn notification_selection_prefers_priority_then_session_order() {
        let completion = Notification::TurnComplete { response: None };
        let first_prompt = Notification::QuestionRequested;
        let second_prompt = Notification::PermissionRequested { tool: None };

        let selected = select_notification(None, Some(completion));
        let selected = select_notification(selected, Some(first_prompt.clone()));
        let selected = select_notification(selected, Some(second_prompt));

        assert_eq!(selected, Some(first_prompt));
    }

    #[test]
    fn shell_results_do_not_replace_existing_preamble() {
        let mut preamble = vec![Message::observation(OBSERVATION.into())];

        prepend_preamble(
            &mut preamble,
            vec![Message::observation(SHELL_RESULT.into())],
        );

        let text = preamble.iter().map(Message::user_text).collect::<Vec<_>>();
        assert_eq!(text, [Some(SHELL_RESULT), Some(OBSERVATION)]);
    }
}
