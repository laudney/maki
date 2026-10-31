use std::cell::RefCell;
use std::env;
use std::io::{self, IsTerminal};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol_schema::v1::{
    AgentCapabilities, AuthMethod, ErrorCode, Implementation, SessionConfigOption, SessionId,
    SessionInfo, SessionModeId, SessionModeState, SessionUpdate,
};
use color_eyre::eyre::{Context, bail};
use color_eyre::{Report, Result};
use flume::Receiver;
use futures_lite::future;
use jiff::Timestamp;
use maki_config::project::{self, ProjectDecision, Runtime, TrustMode};
use maki_config::{ProjectConfig, StorageConfig};
use maki_lua_core::pack::{
    Interaction, PackCommand, PackPreparation, apply_pack_plan, sanitize_message,
};
use maki_lua_ui::{UiDirs, UiFiles, UiHost, UiStart};
use maki_storage::remote_drafts::RemoteStore;
use maki_storage::{StateDir, logging};
use maki_ui::{
    BUILD_MODE, ClientProject, EventLoopParams, LoginInvocation, PLAN_MODE, RemoteAgent,
    RemoteInput, RemoteSettings, RunOutcome,
};
use maki_wire::client::{AcpClient, ClientError, ClientEvent, Confirmed, Link};
use maki_wire::configuration::{Configuration, KEPT, Key, current_value};
use maki_wire::extensions::Agreed;
use maki_wire::methods::MODEL_CONFIG_ID;
use maki_wire::plugins::{PluginReply, PluginRequest, UNSUPPORTED};
use maki_wire::{extensions, websocket};
use serde_json::json;
use tempfile::TempDir;
use tokio::process::Command;
use tracing::warn;

use crate::ConnectArgs;

const DEFAULT_TOKEN_ENV: &str = "MAKI_ACP_TOKEN";
const CLIENT_NAME: &str = "maki";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const NO_LOAD: &str = "the agent cannot load sessions (no loadSession capability)";
const NO_LIST: &str = "the agent cannot list sessions (no sessionCapabilities.list)";
const NOTICE_SEPARATOR: &str = "; ";
const RESTORE_FAILED: &str = "the agent did not take back";
const RESTORE_KEPT: &str = "it kept";
/// A reload whose agent does not answer in time ends the run rather than
/// mixing the old run's stream into the replay or waiting for good, and the
/// session is the agent's to resume.
const UNSETTLED: &str = "the agent did not answer the reload in time, so the client stopped; resume the session with -r";
const NOT_OFFERED: &str = "it no longer offers it";
/// How long a reload waits for the agent to end a turn it cancels.
const TURN_GRACE: Duration = Duration::from_secs(3);
const NO_RELOAD: &str = "the agent cannot load sessions, so a reload cannot reopen this one (no loadSession capability)";
/// An agent stores a session once it holds a message, so one reloaded
/// before its first prompt is not there to load.
const NEVER_STORED: &str =
    "the agent has no stored copy of the session, so the reload opened a new one in place of";
/// How long a child agent gets to close its sessions, which saves them,
/// after its input closes.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

enum Resume {
    New,
    /// The newest session the agent lists for the workspace, or a new one.
    Latest,
    Named(String),
}

/// What the `session/new` and `session/load` answers both carry.
struct Opened {
    modes: Option<SessionModeState>,
    config_options: Vec<SessionConfigOption>,
    seq: u64,
    replay: Vec<SessionUpdate>,
    unreadable: usize,
    /// The tab's record of what it typed in the session is restored: the
    /// session was loaded, or took the place of one the agent never stored.
    resumed: bool,
    /// Events captured during load and settings restore, in wire order,
    /// for the new UI loop to handle first.
    early: Vec<ClientEvent>,
}

/// What a reload keeps: the link, the agent's word on it, and where the
/// tab keeps what it typed.
struct Connection {
    client: Arc<AcpClient>,
    /// A second receiver of the client's events, idle while a run's loop
    /// reads them, so the channel outlives that loop and a reload reads the
    /// session's replay from it.
    events: Receiver<ClientEvent>,
    extensions: Agreed,
    images: bool,
    login: Option<LoginInvocation>,
    /// The agent loads sessions, which is how a reload reopens this one.
    load: bool,
    store: Option<RemoteStore>,
    /// How long a reload waits for the agent to end the old run's turn and
    /// answer its setting changes.
    grace: Duration,
}

struct Client {
    connection: Connection,
    launch_dir: PathBuf,
    cwd: String,
    /// Backs only the tab's local identity, see [`connect`].
    storage: StateDir,
    no_jit: bool,
    no_plugins: bool,
    exit_on_done: bool,
}

/// Runs no local provider or `init.lua`, none of which serve a remote
/// agent. The UI's own plugins load from this machine: the user's `ui.lua`,
/// a trusted project's from the directory the client started in, and their
/// packages. With {no_plugins} none of them loads, nor any bundled plugin: a
/// client has no builtin it must keep.
pub fn connect(args: ConnectArgs, no_jit: bool, no_plugins: bool) -> Result<()> {
    let launch_dir = env::current_dir().context("read the current directory")?;
    let storage_config = StorageConfig::default();
    logging::init_logging(
        storage_config.max_log_bytes,
        storage_config.max_log_files,
        &project::project_root(&launch_dir),
    );
    logging::install_panic_log_hook();
    let cwd = args
        .cwd
        .unwrap_or_else(|| launch_dir.to_string_lossy().into_owned());
    let resume = match (args.resume, args.continue_session) {
        (Some(id), _) => Resume::Named(id),
        (None, true) => Resume::Latest,
        (None, false) => Resume::New,
    };
    let token = token(args.token_env.as_deref())?;

    // Before the agent starts, so its trust card and install prompts have
    // the terminal to themselves, as the TUI's do.
    let mut trust_warnings = Vec::new();
    let project = local_project(&launch_dir, no_plugins, true, &mut trust_warnings);
    let ui = start_ui(&project.config, no_jit, no_plugins, &trust_warnings)?;

    let store = remote_store(args.url.as_deref(), &args.agent, args.token_env.as_deref());
    let (connection, opened) = tokio::runtime::Handle::current().block_on(open(
        args.url.as_deref(),
        &args.agent,
        token.as_deref(),
        &cwd,
        resume,
        store,
    ))?;

    // The agent owns the session, so this scratch store only backs the tab's
    // local identity; what the client keeps of its own is in the store.
    let scratch = TempDir::new().context("create a scratch state directory")?;
    let client = Client {
        connection,
        launch_dir,
        cwd,
        storage: StateDir::from_path(scratch.path().to_path_buf()),
        no_jit,
        no_plugins,
        exit_on_done: args.exit_on_done,
    };
    let code = client.run(ui, project, opened, args.prompt);
    tokio::runtime::Handle::current().block_on(client.connection.client.shutdown(SHUTDOWN_GRACE));
    let code = code?;
    // `process::exit` skips destructors, so the scratch store goes first.
    drop(client);
    drop(scratch);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

impl Client {
    /// Runs the tab until it exits, and returns its exit code. A reload
    /// restarts the UI host as `maki` rebuilds its plugin host, applying a
    /// package plan in between and reading the project's trust again, and
    /// reopens the same session on the same link.
    fn run(
        &self,
        mut ui: UiStart,
        mut project: ClientProject,
        mut opened: Opened,
        mut prompt: Option<String>,
    ) -> Result<i32> {
        let mut notice = None;
        loop {
            let (remote, config_options) = self.connection.remote(opened, &self.cwd);
            let mut params = EventLoopParams::remote(
                remote,
                &config_options,
                &ui,
                &self.cwd,
                self.storage.clone(),
                project,
                self.exit_on_done,
            );
            params.startup_notice = notice.take();
            let (pack, plugins) = match maki_ui::run(params, prompt.take()).context("run UI")? {
                RunOutcome::Exit { code, .. } => return Ok(code),
                RunOutcome::Reload { pack, plugins, .. } => (pack, plugins),
            };
            let mut notices = Vec::new();
            let mut warnings = Vec::new();
            let mut local_plan = pack.map(|plan| *plan);
            if let Some((name, args, bang)) = plugins.command() {
                let command = PackCommand::parse(name, args, bang)
                    .map_err(|error| color_eyre::eyre::eyre!(error))?;
                let prepared = ui
                    .host
                    .core_handle()
                    .package_context()
                    .map(|context| maki_lua_ui::prepare_pack_command(&command, context))
                    .unwrap_or_else(PackPreparation::failed);
                match prepared {
                    PackPreparation::Ready(plan) => local_plan = Some(plan),
                    PackPreparation::Review { prompt, plan }
                        if Interaction::Tty.confirm(&format!("UI: {prompt}")) =>
                    {
                        local_plan = Some(plan)
                    }
                    PackPreparation::Review { .. } => {}
                    PackPreparation::Complete(report) => {
                        notices.push(format!("UI: {}", report.message()))
                    }
                }
            }
            drop(ui);
            if let Some(report) = local_plan.map(apply_pack_plan) {
                notices.extend(report.changed().then(|| report.summary()));
                warnings = report.failures;
            }
            let mut request = PluginRequest {
                operation: plugins,
                review: None,
            };
            loop {
                match tokio::runtime::Handle::current().block_on(self.connection.plugins(&request))
                {
                    Ok(PluginReply::Review(prompt))
                        if Interaction::Tty.confirm(&format!("Agent: {prompt}")) =>
                    {
                        request.review = Some(prompt)
                    }
                    Ok(PluginReply::Review(_)) => break,
                    Ok(PluginReply::Complete(message)) => {
                        notices.extend((!message.is_empty()).then(|| format!("Agent: {message}")));
                        break;
                    }
                    Err(error) => {
                        notices.push(error.to_string());
                        break;
                    }
                }
            }
            project = local_project(&self.launch_dir, self.no_plugins, false, &mut warnings);
            ui = start_ui(&project.config, self.no_jit, self.no_plugins, &warnings)?;
            let (reopened, told) =
                tokio::runtime::Handle::current().block_on(self.connection.reopen(&self.cwd))?;
            opened = reopened;
            notices.extend(told);
            notice = (!notices.is_empty()).then(|| notices.join(NOTICE_SEPARATOR));
        }
    }
}

/// The client's UI host over the project {project_config} names, with
/// {warnings} shown at its start.
fn start_ui(
    project_config: &ProjectConfig,
    no_jit: bool,
    no_plugins: bool,
    warnings: &[String],
) -> Result<UiStart> {
    let mut ui = UiHost::start(
        Interaction::Tty,
        !no_jit,
        UiFiles::resolve(project_config, no_plugins),
        UiDirs::of_user(),
    )
    .context("initialize lua plugin host")?;
    ui.warnings
        .extend(warnings.iter().map(|warning| sanitize_message(warning)));
    Ok(ui)
}

/// The project of {launch_dir}, as the UI sees it: only its `ui.lua` holds
/// it back, and the answer lives in this machine's state, not the agent's.
/// With {ask}, an unanswered question is put on the card when the terminal
/// can answer it, as the TUI puts it at its start; {warnings} gets what a
/// run that did not ask, or could not record the answer, has to say. What
/// is still not trusted is left to `/trust`, which records its grant in the
/// same state. With {no_plugins} that file never runs, so nothing is asked
/// or said about it.
fn local_project(
    launch_dir: &Path,
    no_plugins: bool,
    ask: bool,
    warnings: &mut Vec<String>,
) -> ClientProject {
    let untrusted = |decision: ProjectDecision| ClientProject {
        config: decision.project_config,
        trust: None,
    };
    if no_plugins {
        return untrusted(project::resolve_noninteractive(
            launch_dir,
            TrustMode::Consult,
            Runtime::Ui,
        ));
    }
    let Ok(storage) = StateDir::resolve() else {
        let decision = project::resolve_noninteractive(launch_dir, TrustMode::Consult, Runtime::Ui);
        warnings.extend(decision.notices());
        return untrusted(decision);
    };
    let decision = project::resolve(&storage, launch_dir, TrustMode::Consult, Runtime::Ui);
    let can_ask = ask && io::stdin().is_terminal() && io::stderr().is_terminal();
    let decision = match decision.state.unanswered().filter(|_| can_ask) {
        Some(question) => {
            let answer = maki_ui::ask_trust(question);
            let decision = project::apply_answer(&storage, decision, answer);
            warnings.extend(decision.warning.clone());
            decision
        }
        None => {
            warnings.extend(decision.notices());
            decision
        }
    };
    ClientProject {
        trust: decision
            .state
            .question()
            .cloned()
            .map(|question| (question, storage)),
        config: decision.project_config,
    }
}

/// The value never leaves this process except in the `Authorization` header.
fn token(token_env: Option<&str>) -> Result<Option<String>> {
    let name = token_env.unwrap_or(DEFAULT_TOKEN_ENV);
    match env::var(name) {
        Ok(token) if token.trim().is_empty() => bail!("{name} is set but empty"),
        Ok(token) => Ok(Some(token)),
        Err(_) if token_env.is_some() => bail!("{name} is not set"),
        Err(_) => Ok(None),
    }
}

async fn open(
    url: Option<&str>,
    agent: &[String],
    token: Option<&str>,
    cwd: &str,
    resume: Resume,
    store: Option<RemoteStore>,
) -> Result<(Connection, Opened)> {
    let link = match (url, agent.split_first()) {
        (None, Some((program, args))) => Link::spawn(Command::from(agent_command(program, args)))
            .with_context(|| format!("start the agent {program}"))?,
        (Some(url), None) => websocket::connect(url, token).await?,
        (None, None) => bail!("give an agent URL, or an agent command after --"),
        (Some(_), Some(_)) => bail!("give an agent URL or an agent command after --, not both"),
    };
    let (client, events) = AcpClient::new(link);
    let info = client
        // Only a spawned agent's invocation can run here, and only where a
        // Ctrl+C during it spares this client and the agent.
        .initialize(
            Implementation::new(CLIENT_NAME, VERSION),
            url.is_none() && cfg!(unix),
        )
        .await
        .context("initialize the ACP connection")?;
    let session = match resume {
        Resume::New => None,
        Resume::Named(id) => {
            require_load(&info.agent_capabilities, false)?;
            Some(SessionId::from(id))
        }
        Resume::Latest => {
            require_load(&info.agent_capabilities, true)?;
            let sessions = client
                .list_all_sessions(cwd.into())
                .await
                .context("list the agent's sessions")?;
            newest(sessions)
        }
    };
    let opened = match session {
        Some(session) => load(&client, &events, session, cwd).await?,
        None => create(&client, cwd).await?,
    };
    let connection = Connection {
        client: Arc::new(client),
        events,
        extensions: extensions::agreed(info.meta.as_ref()),
        images: info.agent_capabilities.prompt_capabilities.image,
        login: terminal_login(agent, &info.auth_methods),
        load: info.agent_capabilities.load_session,
        store,
        grace: TURN_GRACE,
    };
    Ok((connection, opened))
}

async fn create(client: &AcpClient, cwd: &str) -> Result<Opened> {
    let created = client
        .new_session(cwd.into())
        .await
        .context("open a session on the agent")?;
    Ok(Opened {
        modes: created.value.modes,
        config_options: created.value.config_options.unwrap_or_default(),
        seq: created.seq,
        replay: Vec::new(),
        unreadable: 0,
        resumed: false,
        early: Vec::new(),
    })
}

/// The agent answered a load that it holds no such session, by the error
/// code ACP gives a missing resource.
fn not_stored(error: &Report) -> bool {
    matches!(
        error.downcast_ref::<ClientError>(),
        Some(ClientError::Rpc(rpc)) if rpc.code == ErrorCode::ResourceNotFound
    )
}

async fn load(
    client: &AcpClient,
    events: &Receiver<ClientEvent>,
    session: SessionId,
    cwd: &str,
) -> Result<Opened> {
    let loaded = client
        .load_session(events, session.clone(), cwd.into())
        .await
        .with_context(|| format!("load session {session} on the agent"))?;
    Ok(Opened {
        modes: loaded.response.value.modes,
        config_options: loaded.response.value.config_options.unwrap_or_default(),
        seq: loaded.response.seq,
        replay: loaded.replay,
        unreadable: loaded.unreadable,
        resumed: true,
        early: loaded.early,
    })
}

impl Connection {
    /// The tab's agent over this connection, in the session {opened}
    /// describes, and the config options it opened with.
    fn remote(&self, opened: Opened, cwd: &str) -> (RemoteAgent, Vec<SessionConfigOption>) {
        let remote = RemoteAgent {
            client: Arc::clone(&self.client),
            events: self.events.clone(),
            extensions: self.extensions.clone(),
            images: self.images,
            plan_modes: opened.modes.as_ref().is_some_and(|modes| {
                [BUILD_MODE, PLAN_MODE].iter().all(|wanted| {
                    modes
                        .available_modes
                        .iter()
                        .any(|mode| mode.id.to_string() == *wanted)
                })
            }),
            mode: opened
                .modes
                .as_ref()
                .map(|modes| modes.current_mode_id.to_string()),
            session_seq: opened.seq,
            replay: opened.replay,
            unreadable: opened.unreadable,
            settings: RemoteSettings::from_options(&opened.config_options),
            login: self.login.clone(),
            input: self.store.clone().map(|store| RemoteInput {
                history: store.history_file(cwd),
                store,
                resumed: opened.resumed,
            }),
            reload: self.load,
            early: opened.early,
        };
        (remote, opened.config_options)
    }

    /// Loads the session the client is bound to again, which may be one a
    /// `/new` opened, so a new run draws it from its replay, and asks the
    /// agent again for the options and mode it confirmed before, which the
    /// load may reset; returns what the user is told of it: what the agent
    /// did not take back, and a new session opened in place of one the agent
    /// never stored.
    ///
    /// The old run is ended first, so the replay is the only copy drawn:
    /// every request of the agent is answered as refused, including those
    /// the old loop took and will not answer, its turn is cancelled and
    /// waited for, since a turn's updates come before its answer, then its
    /// setting changes, so the snapshot holds what the agent settled on.
    /// Whatever reaches the event channel meanwhile is dropped. A run that
    /// does not end in time fails the reload, and with it the client's run,
    /// rather than mixing its stream into the replay.
    async fn reopen(&self, cwd: &str) -> Result<(Opened, Vec<String>)> {
        if !self.load {
            bail!(NO_RELOAD);
        }
        let session = self
            .client
            .bound_session()
            .context("find the session to reopen")?;
        self.quiesce().await?;
        let before = self.client.confirmed();
        let mut notices = Vec::new();
        let mut opened = match load(&self.client, &self.events, session.clone(), cwd).await {
            Err(error) if not_stored(&error) => {
                notices.push(format!("{NEVER_STORED} {session}"));
                let mut created = create(&self.client, cwd).await?;
                self.carry_input(&session, &mut created);
                created
            }
            opened => opened?,
        };
        // What the agent sends while it answers is read meanwhile, so a
        // full channel never stops the reader short of an answer, and kept,
        // since it already belongs to the new run.
        let early = RefCell::new(Vec::new());
        let unrestored = future::or(
            async { Some(restore(&self.client, &before, &mut opened).await) },
            future::or(self.keep_events(&early), async {
                tokio::time::sleep(self.grace).await;
                None
            }),
        )
        .await;
        let Some(unrestored) = unrestored else {
            let bound = self.client.bound_session().unwrap_or(session);
            bail!("{UNSETTLED} {bound}");
        };
        notices.extend(unrestored);
        opened.early.extend(early.into_inner());
        Ok((opened, notices))
    }

    async fn quiesce(&self) -> Result<()> {
        self.client.retire_requests();
        let settled = future::or(
            async {
                self.client.end_turns(self.grace).await
                    && self.client.settle_settings(self.grace).await
            },
            self.discard_events(),
        )
        .await;
        if !settled {
            bail!("{UNSETTLED} {}", self.client.bound_session()?);
        }
        self.client.retire_requests();
        while self.events.try_recv().is_ok() {}
        Ok(())
    }

    async fn plugins(&self, request: &PluginRequest) -> Result<PluginReply> {
        if !self.extensions.plugins {
            bail!(UNSUPPORTED);
        }
        self.quiesce().await?;
        future::or(self.client.plugins(request), self.discard_events())
            .await
            .map_err(Into::into)
    }

    /// Moves what the tab typed in {replaced} to the session the client is
    /// bound to now, which took its place, so {opened}'s run restores it.
    fn carry_input(&self, replaced: &SessionId, opened: &mut Opened) {
        let (Some(store), Ok(bound)) = (&self.store, self.client.bound_session()) else {
            return;
        };
        if let Err(error) = store.carry(&replaced.to_string(), &bound.to_string()) {
            warn!(%error, %replaced, %bound, "cannot carry the remote draft to the new session");
        }
        opened.resumed = true;
    }

    /// Keeps what reaches the event channel in {early}, answering any
    /// request of the agent as refused, since no loop is there to answer
    /// it. Never ends: it runs beside a wait.
    async fn keep_events<T>(&self, early: &RefCell<Vec<ClientEvent>>) -> Option<T> {
        while let Ok(event) = self.events.recv_async().await {
            match event {
                ClientEvent::Permission(_) | ClientEvent::Question(_) => {
                    self.client.retire_requests();
                }
                event => early.borrow_mut().push(event),
            }
        }
        future::pending().await
    }

    /// Drops what reaches the event channel, answering any new request of
    /// the agent as refused, so the reader never waits on a full channel.
    /// Never ends: it runs beside a wait.
    async fn discard_events<T>(&self) -> T {
        while self.events.recv_async().await.is_ok() {
            self.client.retire_requests();
        }
        future::pending().await
    }
}

/// Asks the agent for what {before} confirmed that {opened} no longer
/// holds, waiting for each answer as the menus do: the model first, since
/// the other options may depend on it, then the rest, then the mode.
/// Returns a line for each change the agent refused.
async fn restore(client: &AcpClient, before: &Confirmed, opened: &mut Opened) -> Vec<String> {
    let confirmed = Confirmed {
        config_options: opened.config_options.clone(),
        mode: opened
            .modes
            .as_ref()
            .map(|modes| modes.current_mode_id.to_string()),
    };
    let mut configuration = Configuration::new(confirmed, opened.seq);
    let mut wanted: Vec<_> = before
        .config_options
        .iter()
        .filter_map(|option| {
            let value = current_value(option)?;
            (value.is_boolean() || value.is_string())
                .then(|| (Key::Option(option.id.to_string()), value))
        })
        .collect();
    wanted.sort_by_key(|(key, _)| key.name() != MODEL_CONFIG_ID);
    if let Some(mode) = &before.mode {
        wanted.push((Key::Mode, mode.clone().into()));
    }
    let mut unrestored = Vec::new();
    for (key, value) in wanted {
        let Some(held) = configuration.value(&key) else {
            if key != Key::Mode {
                unrestored.push(format!("{RESTORE_FAILED} {}: {NOT_OFFERED}", key.name()));
            }
            continue;
        };
        if held == value {
            continue;
        }
        let change = configuration.desire(key.clone(), value, false);

        let answer = client
            .configure(change.key.clone(), change.value.clone())
            .await
            .map_err(|error| error.to_string());
        if let (Err(error), _) = configuration.confirm(&change, answer) {
            let error = match (error.as_str(), configuration.value(&key)) {
                (KEPT, Some(kept)) => format!("{RESTORE_KEPT} {kept}"),
                (KEPT, None) => NOT_OFFERED.into(),
                _ => error,
            };
            unrestored.push(format!("{RESTORE_FAILED} {}: {error}", key.name()));
        }
    }
    opened.seq = configuration.sequence();
    opened.config_options = configuration.confirmed.config_options;
    if let (Some(modes), Some(mode)) = (&mut opened.modes, configuration.confirmed.mode) {
        modes.current_mode_id = SessionModeId::from(mode);
    }
    unrestored
}

/// `None` when the user's store or this directory cannot be read: the tab
/// then keeps nothing past its run.
fn remote_store(
    url: Option<&str>,
    agent: &[String],
    token_env: Option<&str>,
) -> Option<RemoteStore> {
    let state = StateDir::resolve()
        .inspect_err(|error| warn!(%error, "no state dir: remote input is not kept"))
        .ok()?;
    let launch_dir = env::current_dir()
        .inspect_err(|error| warn!(%error, "no current dir: remote input is not kept"))
        .ok()?;
    Some(RemoteStore::open(
        &state,
        &agent_identity(url, agent, token_env, &launch_dir),
    ))
}

/// Agents launched differently never share what the client keeps: a spawn
/// is named by its argv and the directory it resolves that argv in, a URL
/// by itself and the variable its token comes from, never the token.
fn agent_identity(
    url: Option<&str>,
    agent: &[String],
    token_env: Option<&str>,
    launch_dir: &Path,
) -> String {
    match url {
        Some(url) => json!({ "url": url, "tokenEnv": token_env.unwrap_or(DEFAULT_TOKEN_ENV) }),
        None => json!({ "spawn": agent, "dir": launch_dir.to_string_lossy() }),
    }
    .to_string()
}

/// A group of its own, so Ctrl+C in this terminal, as during a login,
/// never reaches the agent: it ends with its pipes.
fn agent_command(program: &str, args: &[String]) -> ProcessCommand {
    let mut command = ProcessCommand::new(program);
    command.args(args);
    #[cfg(unix)]
    command.process_group(0);
    command
}

/// The spawned agent's own invocation with the first terminal login it
/// offered appended, as ACP's terminal auth asks a client to run it.
fn terminal_login(agent: &[String], methods: &[AuthMethod]) -> Option<LoginInvocation> {
    let (program, args) = agent.split_first()?;
    let method = methods.iter().find_map(|method| match method {
        AuthMethod::Terminal(terminal) => Some(terminal),
        _ => None,
    })?;
    Some(LoginInvocation {
        program: program.clone(),
        args: args.iter().chain(&method.args).cloned().collect(),
        env: method.env.clone(),
    })
}

fn require_load(capabilities: &AgentCapabilities, list: bool) -> Result<()> {
    if !capabilities.load_session {
        bail!(NO_LOAD);
    }
    if list && capabilities.session_capabilities.list.is_none() {
        bail!(NO_LIST);
    }
    Ok(())
}

/// ACP does not order the list, so the latest `updatedAt` wins. One that is
/// missing or unreadable ranks lowest, and the first one listed wins a tie.
fn newest(sessions: Vec<SessionInfo>) -> Option<SessionId> {
    let mut best: Option<(Option<Timestamp>, SessionId)> = None;
    for session in sessions {
        let updated = session
            .updated_at
            .as_deref()
            .and_then(|at| at.parse::<Timestamp>().ok());
        if best.as_ref().is_none_or(|(top, _)| updated > *top) {
            best = Some((updated, session.session_id));
        }
    }
    best.map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use std::collections::HashMap;

    use agent_client_protocol_schema::v1::{AuthMethodTerminal, Error as AcpError};
    use flume::Sender;
    use maki_storage::remote_drafts::RemoteDraft;
    use maki_ui::AppSession;
    use maki_wire::client::EVENT_CAPACITY;
    use maki_wire::methods;
    use maki_wire::plugins::PluginOperation;
    use maki_wire::session::RESTORE_METHOD;
    use test_case::test_case;

    use super::*;

    const OLDER: &str = "2026-01-01T00:00:00Z";
    const NEWER: &str = "2026-06-01T00:00:00Z";
    const UNREADABLE: &str = "last tuesday";
    const CWD: &str = "/agent/workspace";
    const AGENT: [&str; 3] = ["maki", "acp", "--yolo"];
    const LOGIN_ARG: &str = "--login";
    const LOGIN_ENV: (&str, &str) = ("MAKI_LOGIN", "1");
    const URL: &str = "wss://agent.example/acp";
    const OTHER_TOKEN_ENV: &str = "OTHER_ACP_TOKEN";
    const LAUNCH_DIR: &str = "/home/user/project";
    const OTHER_LAUNCH_DIR: &str = "/home/user/other";
    const SESSION: &str = "agent-session-1";
    const NEW_SESSION: &str = "agent-session-2";
    const DRAFT: &str = "half a prompt";
    const QUEUED: &str = "and run the tests";
    const READ_ERROR: &str = "incompatible session version 1 (expected 2)";
    const STALE: &str = "what the closed run left unread";
    const REPLAYED: &str = "the session's history";
    const AGENT_REQUEST_ID: &str = "agent-ask-1";
    const STARTUP_MODEL: &str = "anthropic/claude-sonnet-4-5";
    const PICKED_MODEL: &str = "zai/glm-4.6";
    const THINKING: &str = "thinking";
    const LOW: &str = "low";
    const HIGH: &str = "high";
    const REFUSAL: &str = "no such model";
    const SHORT_GRACE: Duration = Duration::from_millis(50);
    const OVER_CAPACITY: usize = EVENT_CAPACITY + 2;
    const GAUGE: (u64, u64) = (10_600, 1_000_000);

    struct Peer {
        from_client: Receiver<String>,
        to_client: Sender<String>,
    }

    impl Peer {
        async fn recv(&self) -> Value {
            serde_json::from_str(&self.from_client.recv_async().await.unwrap()).unwrap()
        }

        fn send(&self, message: Value) {
            self.to_client.send(message.to_string()).unwrap();
        }

        fn respond(&self, request: &Value, result: Value) {
            self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}));
        }

        fn update(&self, text: &str) {
            self.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": SESSION,
                "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}
            }}));
        }
    }

    fn connection(load: bool) -> (Connection, Peer) {
        connection_to(load, SESSION)
    }

    fn connection_to(load: bool, session: &str) -> (Connection, Peer) {
        let (client_tx, from_client) = flume::unbounded();
        let (to_client, client_rx) = flume::unbounded();
        let (client, events) = AcpClient::new(Link::new(client_tx, client_rx));
        let peer = Peer {
            from_client,
            to_client,
        };
        let (created, ()) =
            crate::test_block_on(future::zip(client.new_session(CWD.into()), async {
                let request = peer.recv().await;
                peer.respond(&request, json!(methods::new_session_response(session)));
            }));
        created.unwrap();
        let connection = Connection {
            client: Arc::new(client),
            events,
            extensions: Agreed::default(),
            images: false,
            login: None,
            load,
            store: None,
            grace: TURN_GRACE,
        };
        (connection, peer)
    }

    #[test]
    fn an_agent_without_plugin_administration_is_named() {
        let _runtime = crate::test_runtime().enter();
        let (connection, _peer) = connection(true);
        let request = PluginRequest {
            operation: PluginOperation::Reload,
            review: None,
        };
        assert_eq!(
            crate::test_block_on(connection.plugins(&request))
                .unwrap_err()
                .to_string(),
            UNSUPPORTED
        );
    }

    #[test_case(false ; "generic_acp")]
    #[test_case(true ; "maki_transcript")]
    fn a_reload_keeps_the_bound_sessions_turns_and_gauge(maki: bool) {
        let _runtime = crate::test_runtime().enter();
        let mut snapshot = AppSession::new(STARTUP_MODEL, CWD);
        snapshot.meta.context_size = GAUGE.0 as u32;
        for (role, text) in [("user", DRAFT), ("assistant", REPLAYED)] {
            snapshot.push_message(
                serde_json::from_value(
                    json!({"role": role, "content": [{"type": "text", "text": text}]}),
                )
                .unwrap(),
            );
        }
        let id = if maki {
            snapshot.id.to_string()
        } else {
            SESSION.into()
        };
        let (mut connection, peer) = connection_to(true, &id);
        connection.extensions.sessions = maki;
        crate::test_block_on(async {
            if !maki {
                peer.update(STALE);
                while connection.events.is_empty() {
                    future::yield_now().await;
                }
            }
            let (opened, ()) = future::zip(connection.reopen(CWD), async {
                let request = peer.recv().await;
                assert_eq!(request["method"], "session/load");
                assert_eq!(request["params"]["sessionId"], id);
                if maki {
                    peer.send(json!({"jsonrpc": "2.0", "method": RESTORE_METHOD, "params": snapshot}));
                    peer.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": id, "update": {"sessionUpdate": "usage_update", "used": GAUGE.0, "size": GAUGE.1}}}));
                } else { peer.update(REPLAYED); }
                peer.respond(&request, json!(methods::load_session_response()));
            }).await;
            let (opened, unrestored) = opened.unwrap();
            assert!(opened.resumed);
            assert!(unrestored.is_empty(), "{unrestored:?}");
            if !maki {
                assert_only_replayed(&opened);
            }
            let (remote, _) = connection.remote(opened, CWD);
            assert_eq!(remote.client.bound_session().unwrap().to_string(), id);
            if maki {
                let [
                    ClientEvent::Restored(restored),
                    ClientEvent::Update(_, SessionUpdate::UsageUpdate(usage)),
                ] = remote.early.as_slice()
                else {
                    panic!("the transcript and gauge must survive reload");
                };
                assert_eq!(restored.messages(), snapshot.messages());
                assert_eq!((usage.used, usage.size), GAUGE);
            }
        });
    }

    fn assert_only_replayed(opened: &Opened) {
        assert_eq!(opened.replay.len(), 1);
        assert_eq!(
            serde_json::to_value(&opened.replay[0]).unwrap()["content"]["text"],
            REPLAYED
        );
    }

    async fn answer_load(peer: &Peer, options: Option<Value>) {
        let request = peer.recv().await;
        assert_eq!(request["method"], "session/load");
        peer.update(REPLAYED);
        let mut response = json!(methods::load_session_response());
        if let Some(options) = options {
            response["configOptions"] = options;
        }
        peer.respond(&request, response);
    }

    /// The old turn may still be sending when the reload starts: it is
    /// cancelled and waited for, so an update it sends after the old loop
    /// is gone is not drawn again with the replay.
    #[test]
    fn an_update_of_the_old_turn_is_not_drawn_twice() {
        let _runtime = crate::test_runtime().enter();
        let (connection, peer) = connection(true);
        crate::test_block_on(async {
            let client = Arc::clone(&connection.client);
            let turn = tokio::spawn(async move { client.prompt(Vec::new(), None).await });
            let prompt = peer.recv().await;
            turn.abort();
            let _ = turn.await;
            let (opened, ()) = future::zip(connection.reopen(CWD), async {
                let cancel = peer.recv().await;
                assert_eq!(cancel["method"], "session/cancel");
                peer.update(STALE);
                peer.respond(&prompt, json!({"stopReason": "cancelled"}));
                answer_load(&peer, None).await;
            })
            .await;
            assert_only_replayed(&opened.unwrap().0);
        });
    }

    /// A permission ask the old loop took and never answered is answered
    /// before the load, so the agent's turn does not wait on it.
    #[test]
    fn an_ask_the_old_loop_took_is_cancelled_before_the_load() {
        let _runtime = crate::test_runtime().enter();
        let (connection, peer) = connection(true);
        crate::test_block_on(async {
            peer.send(json!({"jsonrpc": "2.0", "id": AGENT_REQUEST_ID, "method": "session/request_permission", "params": {
                "sessionId": SESSION,
                "toolCall": {"toolCallId": "call-1", "title": "bash"},
                "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}],
            }}));
            assert!(matches!(
                connection.events.recv_async().await,
                Ok(ClientEvent::Permission(_))
            ));
            let (opened, ()) = future::zip(connection.reopen(CWD), async {
                let reply = peer.recv().await;
                assert_eq!(reply["id"], AGENT_REQUEST_ID);
                assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
                answer_load(&peer, None).await;
            })
            .await;
            assert_only_replayed(&opened.unwrap().0);
        });
    }

    fn select(id: &str, current: &str, values: &[&str]) -> Value {
        let options: Vec<Value> = values
            .iter()
            .map(|value| json!({"value": value, "name": value}))
            .collect();
        json!({"id": id, "name": id, "type": "select", "currentValue": current, "options": options})
    }

    fn options(model: &str, thinking: &str) -> Value {
        json!([
            select(MODEL_CONFIG_ID, model, &[STARTUP_MODEL, PICKED_MODEL]),
            select(THINKING, thinking, &[LOW, HIGH]),
        ])
    }

    /// The agent's load starts the session on its startup model, so the
    /// model and thinking picked before the reload are asked for again, the
    /// model first, and a refusal is reported.
    #[test_case(true ; "taken_back")]
    #[test_case(false ; "refused")]
    fn a_reload_asks_again_for_the_model_and_thinking_picked_before(accepted: bool) {
        let _runtime = crate::test_runtime().enter();
        let (connection, peer) = connection(true);
        crate::test_block_on(async {
            picked_before(&connection, &peer, PICKED_MODEL, HIGH).await;
            let (opened, ()) = future::zip(connection.reopen(CWD), async {
                answer_load(&peer, Some(options(STARTUP_MODEL, LOW))).await;
                let model = peer.recv().await;
                assert_eq!(model["method"], "session/set_config_option");
                assert_eq!(model["params"]["configId"], MODEL_CONFIG_ID);
                assert_eq!(model["params"]["value"], PICKED_MODEL);
                match accepted {
                    true => peer.respond(&model, json!({"configOptions": options(PICKED_MODEL, LOW)})),
                    false => peer.send(json!({"jsonrpc": "2.0", "id": model["id"], "error": {"code": -32602, "message": REFUSAL}})),
                }
                let thinking = peer.recv().await;
                assert_eq!(thinking["params"]["configId"], THINKING);
                assert_eq!(thinking["params"]["value"], HIGH);
                let model = if accepted { PICKED_MODEL } else { STARTUP_MODEL };
                peer.respond(&thinking, json!({"configOptions": options(model, HIGH)}));
            })
            .await;
            let (opened, unrestored) = opened.unwrap();
            let held: Vec<Value> = opened
                .config_options
                .iter()
                .map(|option| serde_json::to_value(option).unwrap()["currentValue"].clone())
                .collect();
            match accepted {
                true => {
                    assert!(unrestored.is_empty(), "{unrestored:?}");
                    assert_eq!(held, [json!(PICKED_MODEL), json!(HIGH)]);
                }
                false => {
                    assert_eq!(unrestored.len(), 1, "{unrestored:?}");
                    assert!(unrestored[0].starts_with(RESTORE_FAILED));
                    assert_eq!(held, [json!(STARTUP_MODEL), json!(HIGH)]);
                }
            }
        });
    }

    /// More of the old turn than the event channel holds still lets its
    /// answer through, since the channel is read while the reload waits.
    #[test]
    fn a_full_event_channel_does_not_stop_the_old_turn_ending() {
        let _runtime = crate::test_runtime().enter();
        let (connection, peer) = connection(true);
        crate::test_block_on(async {
            let client = Arc::clone(&connection.client);
            let turn = tokio::spawn(async move { client.prompt(Vec::new(), None).await });
            let prompt = peer.recv().await;
            turn.abort();
            let _ = turn.await;
            let (opened, ()) = future::zip(connection.reopen(CWD), async {
                assert_eq!(peer.recv().await["method"], "session/cancel");
                for _ in 0..OVER_CAPACITY {
                    peer.update(STALE);
                }
                peer.respond(&prompt, json!({"stopReason": "cancelled"}));
                answer_load(&peer, None).await;
            })
            .await;
            assert_only_replayed(&opened.unwrap().0);
        });
    }

    /// A turn that does not end in time fails the reload before any load,
    /// so its stream never mixes into a replay.
    #[test]
    fn a_turn_that_never_ends_fails_the_reload() {
        let _runtime = crate::test_runtime().enter();
        let (mut connection, peer) = connection(true);
        connection.grace = SHORT_GRACE;
        crate::test_block_on(async {
            let client = Arc::clone(&connection.client);
            let turn = tokio::spawn(async move { client.prompt(Vec::new(), None).await });
            let _prompt = peer.recv().await;
            turn.abort();
            let _ = turn.await;
            let error = connection.reopen(CWD).await.err().map(|e| e.to_string());
            assert_eq!(error, Some(format!("{UNSETTLED} {SESSION}")));
            assert_eq!(peer.recv().await["method"], "session/cancel");
            assert!(peer.from_client.is_empty(), "no load may follow");
        });
    }

    /// A change the old loop sent and stopped waiting on is answered before
    /// the snapshot, so the reload asks for what the agent settled on.
    #[test]
    fn a_setting_answered_during_the_wait_is_kept() {
        let _runtime = crate::test_runtime().enter();
        let (connection, peer) = connection(true);
        crate::test_block_on(async {
            let client = Arc::clone(&connection.client);
            let change = tokio::spawn(async move {
                client
                    .configure(Key::Option(MODEL_CONFIG_ID.into()), json!(PICKED_MODEL))
                    .await
            });
            let pick = peer.recv().await;
            change.abort();
            let _ = change.await;
            let (opened, ()) = future::zip(connection.reopen(CWD), async {
                peer.respond(&pick, json!({"configOptions": options(PICKED_MODEL, LOW)}));
                answer_load(&peer, Some(options(STARTUP_MODEL, LOW))).await;
                let again = peer.recv().await;
                assert_eq!(again["params"]["configId"], MODEL_CONFIG_ID);
                assert_eq!(again["params"]["value"], PICKED_MODEL);
                peer.respond(&again, json!({"configOptions": options(PICKED_MODEL, LOW)}));
            })
            .await;
            let (_, unrestored) = opened.unwrap();
            assert!(unrestored.is_empty(), "{unrestored:?}");
        });
    }

    #[test]
    fn a_change_the_agent_answers_but_does_not_take_is_reported() {
        let _runtime = crate::test_runtime().enter();
        let (connection, peer) = connection(true);
        crate::test_block_on(async {
            picked_before(&connection, &peer, PICKED_MODEL, LOW).await;
            let (opened, ()) = future::zip(connection.reopen(CWD), async {
                answer_load(&peer, Some(options(STARTUP_MODEL, LOW))).await;
                let model = peer.recv().await;
                peer.respond(
                    &model,
                    json!({"configOptions": options(STARTUP_MODEL, LOW)}),
                );
            })
            .await;
            let (_, unrestored) = opened.unwrap();
            assert_eq!(
                unrestored,
                [format!(
                    "{RESTORE_FAILED} {MODEL_CONFIG_ID}: {RESTORE_KEPT} {}",
                    json!(STARTUP_MODEL)
                )]
            );
        });
    }

    async fn picked_before(connection: &Connection, peer: &Peer, model: &str, thinking: &str) {
        peer.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": SESSION,
            "update": {"sessionUpdate": "config_option_update", "configOptions": options(model, thinking)}
        }}));
        assert!(connection.events.recv_async().await.is_ok());
    }

    /// More updates than the event channel holds ahead of a restore's
    /// answer still let the answer through, and they reach the new run.
    #[test]
    fn a_restore_reads_the_channel_while_the_agent_answers() {
        let _runtime = crate::test_runtime().enter();
        let (connection, peer) = connection(true);
        crate::test_block_on(async {
            picked_before(&connection, &peer, PICKED_MODEL, LOW).await;
            let (opened, ()) = future::zip(connection.reopen(CWD), async {
                answer_load(&peer, Some(options(STARTUP_MODEL, LOW))).await;
                let model = peer.recv().await;
                for _ in 0..OVER_CAPACITY {
                    peer.update(STALE);
                }
                peer.respond(&model, json!({"configOptions": options(PICKED_MODEL, LOW)}));
            })
            .await;
            let (opened, unrestored) = opened.unwrap();
            assert!(unrestored.is_empty(), "{unrestored:?}");
            // The answer can win the race against the last updates read, which
            // then wait in the channel behind `early` for the new run.
            let kept = opened
                .early
                .into_iter()
                .chain(connection.events.try_iter())
                .filter(|event| matches!(event, ClientEvent::Update(..)))
                .count();
            assert!(kept >= OVER_CAPACITY, "kept {kept}");
        });
    }

    /// A restore the agent never answers fails the reload like a turn
    /// that never ends, instead of leaving the UI down for good.
    #[test]
    fn a_restore_the_agent_never_answers_fails_the_reload() {
        let _runtime = crate::test_runtime().enter();
        let (mut connection, peer) = connection(true);
        connection.grace = SHORT_GRACE;
        crate::test_block_on(async {
            picked_before(&connection, &peer, PICKED_MODEL, LOW).await;
            let (error, ()) = future::zip(connection.reopen(CWD), async {
                answer_load(&peer, Some(options(STARTUP_MODEL, LOW))).await;
                assert_eq!(peer.recv().await["params"]["configId"], MODEL_CONFIG_ID);
            })
            .await;
            let error = error.err().map(|e| e.to_string());
            assert_eq!(error, Some(format!("{UNSETTLED} {SESSION}")));
        });
    }

    /// An option the load no longer offers, and one a successful answer
    /// leaves out, were not taken back either, and nothing is asked for
    /// an option that is gone.
    #[test]
    fn options_the_agent_no_longer_offers_are_reported() {
        let _runtime = crate::test_runtime().enter();
        let (connection, peer) = connection(true);
        crate::test_block_on(async {
            picked_before(&connection, &peer, PICKED_MODEL, HIGH).await;
            let model_only = json!([select(
                MODEL_CONFIG_ID,
                STARTUP_MODEL,
                &[STARTUP_MODEL, PICKED_MODEL]
            )]);
            let (opened, ()) = future::zip(connection.reopen(CWD), async {
                answer_load(&peer, Some(model_only)).await;
                let model = peer.recv().await;
                assert_eq!(model["params"]["configId"], MODEL_CONFIG_ID);
                peer.respond(&model, json!({"configOptions": []}));
            })
            .await;
            let (_, unrestored) = opened.unwrap();
            assert_eq!(
                unrestored,
                [
                    format!("{RESTORE_FAILED} {MODEL_CONFIG_ID}: {NOT_OFFERED}"),
                    format!("{RESTORE_FAILED} {THINKING}: {NOT_OFFERED}"),
                ]
            );
            assert!(
                peer.from_client.is_empty(),
                "nothing asked for a gone option"
            );
        });
    }

    async fn replace_never_stored(peer: &Peer) {
        let load = peer.recv().await;
        assert_eq!(load["method"], "session/load");
        peer.send(json!({"jsonrpc": "2.0", "id": load["id"],
            "error": AcpError::resource_not_found(Some(SESSION.into()))}));
        let new = peer.recv().await;
        assert_eq!(new["method"], "session/new");
        let mut created = json!(methods::new_session_response(NEW_SESSION));
        created["configOptions"] = options(STARTUP_MODEL, LOW);
        peer.respond(&new, created);
    }

    fn typed() -> RemoteDraft {
        RemoteDraft {
            input_draft: Some(DRAFT.into()),
            input_draft_writers: None,
            queued_messages: vec![QUEUED.into()],
        }
    }

    /// A session reloaded before its first prompt was never stored, so the
    /// run goes on in a new session on the same link, with what the client
    /// picked before asked for again and what the tab typed carried over.
    #[test]
    fn a_reload_of_a_session_never_stored_opens_a_new_one() {
        let _runtime = crate::test_runtime().enter();
        let (mut connection, peer) = connection(true);
        let state = TempDir::new().unwrap();
        let store = RemoteStore::open(&StateDir::from_path(state.path().to_path_buf()), URL);
        store
            .claim(SESSION)
            .unwrap()
            .unwrap()
            .save(&typed())
            .unwrap();
        connection.store = Some(store.clone());
        crate::test_block_on(async {
            picked_before(&connection, &peer, PICKED_MODEL, LOW).await;
            let (opened, ()) = future::zip(connection.reopen(CWD), async {
                replace_never_stored(&peer).await;
                let model = peer.recv().await;
                assert_eq!(model["params"]["value"], PICKED_MODEL);
                peer.respond(&model, json!({"configOptions": options(PICKED_MODEL, LOW)}));
            })
            .await;
            let (opened, notices) = opened.unwrap();
            assert!(opened.resumed, "the new run restores what was typed");
            assert_eq!(notices, [format!("{NEVER_STORED} {SESSION}")]);
            assert_eq!(
                connection.client.bound_session().unwrap(),
                SessionId::from(NEW_SESSION)
            );
        });
        assert_eq!(store.claim(NEW_SESSION).unwrap().unwrap().load(), typed());
    }

    /// A restore that times out after the replacement names the session
    /// the agent holds now, the one a resume can find.
    #[test]
    fn a_replacement_that_does_not_settle_names_the_new_session() {
        let _runtime = crate::test_runtime().enter();
        let (mut connection, peer) = connection(true);
        connection.grace = SHORT_GRACE;
        crate::test_block_on(async {
            picked_before(&connection, &peer, PICKED_MODEL, LOW).await;
            let (error, ()) = future::zip(connection.reopen(CWD), async {
                replace_never_stored(&peer).await;
                assert_eq!(peer.recv().await["params"]["configId"], MODEL_CONFIG_ID);
            })
            .await;
            let error = error.err().map(|e| e.to_string());
            assert_eq!(error, Some(format!("{UNSETTLED} {NEW_SESSION}")));
        });
    }

    /// A session the agent holds but cannot read still ends the run with
    /// the read error, and no new session takes its place.
    #[test]
    fn a_reload_the_agent_cannot_read_fails_with_its_error() {
        let _runtime = crate::test_runtime().enter();
        let (connection, peer) = connection(true);
        crate::test_block_on(async {
            let (opened, ()) = future::zip(connection.reopen(CWD), async {
                let load = peer.recv().await;
                peer.send(json!({"jsonrpc": "2.0", "id": load["id"],
                    "error": AcpError::internal_error().data(json!(READ_ERROR))}));
            })
            .await;
            let error = opened.err().expect("the reload fails");
            assert!(
                error
                    .chain()
                    .any(|cause| cause.to_string().contains(READ_ERROR)),
                "{error:?}"
            );
            assert!(peer.from_client.is_empty(), "no session/new may follow");
        });
    }

    #[test]
    fn an_agent_that_cannot_load_sessions_is_not_reopened() {
        let _runtime = crate::test_runtime().enter();
        let (connection, _peer) = connection(false);
        let error = crate::test_block_on(connection.reopen(CWD))
            .err()
            .map(|e| e.to_string());
        assert_eq!(error.as_deref(), Some(NO_RELOAD));
    }

    fn agent() -> Vec<String> {
        AGENT.iter().map(|arg| arg.to_string()).collect()
    }

    fn terminal_method() -> AuthMethod {
        AuthMethod::Terminal(
            AuthMethodTerminal::new("maki-login", "Log in")
                .args(vec![LOGIN_ARG.into()])
                .env(HashMap::from([(LOGIN_ENV.0.into(), LOGIN_ENV.1.into())])),
        )
    }

    /// ACP asks the client to run its own invocation with the method's
    /// arguments appended and its environment over it.
    #[test]
    fn the_login_is_the_agent_invocation_with_the_methods_additions() {
        let _runtime = crate::test_runtime().enter();
        let login = terminal_login(&agent(), &[terminal_method()]).unwrap();
        assert_eq!(login.program, AGENT[0]);
        assert_eq!(login.args, [AGENT[1], AGENT[2], LOGIN_ARG]);
        assert_eq!(login.env[LOGIN_ENV.0], LOGIN_ENV.1);
    }

    /// A relative argv resolves in its launch directory, and two tokens
    /// may name two accounts behind one URL.
    #[test_case(None, &AGENT, None, OTHER_LAUNCH_DIR ; "spawn_in_another_dir")]
    #[test_case(Some(URL), &[], Some(OTHER_TOKEN_ENV), LAUNCH_DIR ; "url_with_another_token_env")]
    fn an_agent_launched_differently_has_another_identity(
        url: Option<&str>,
        agent: &[&str],
        token_env: Option<&str>,
        launch_dir: &str,
    ) {
        let _runtime = crate::test_runtime().enter();
        let agent: Vec<String> = agent.iter().map(|arg| arg.to_string()).collect();
        let base = agent_identity(url, &agent, None, Path::new(LAUNCH_DIR));
        let other = agent_identity(url, &agent, token_env, Path::new(launch_dir));
        assert_ne!(base, other);
    }

    /// A WebSocket agent's program is not on this machine, and an agent may
    /// offer no terminal login at all.
    #[test_case(&[], &[terminal_method()] ; "websocket")]
    #[test_case(&AGENT, &[] ; "no_terminal_method")]
    fn there_is_no_login_to_run(agent: &[&str], methods: &[AuthMethod]) {
        let _runtime = crate::test_runtime().enter();
        let agent: Vec<String> = agent.iter().map(|arg| arg.to_string()).collect();
        assert!(terminal_login(&agent, methods).is_none());
    }

    /// Ctrl+C during a login signals the terminal's foreground group, which
    /// the agent must not be in.
    #[cfg(unix)]
    #[test]
    fn a_spawned_agent_has_its_own_process_group() {
        let _runtime = crate::test_runtime().enter();
        let mut child = agent_command("sleep", &["5".to_owned()]).spawn().unwrap();
        let pid = child.id().to_string();
        let ps = ProcessCommand::new("ps")
            .args(["-o", "pgid=", "-p", &pid])
            .output()
            .unwrap();
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(String::from_utf8_lossy(&ps.stdout).trim(), pid);
    }

    fn info(id: &str, updated_at: Option<&str>) -> SessionInfo {
        let info = SessionInfo::new(id.to_owned(), CWD);
        match updated_at {
            Some(at) => info.updated_at(at.to_owned()),
            None => info,
        }
    }

    #[test_case(vec![info("a", Some(OLDER)), info("b", Some(NEWER))], Some("b") ; "latest_wins")]
    #[test_case(vec![info("a", None), info("b", Some(OLDER))], Some("b") ; "missing_ranks_lowest")]
    #[test_case(vec![info("a", Some(UNREADABLE)), info("b", Some(OLDER))], Some("b") ; "unreadable_ranks_lowest")]
    #[test_case(vec![info("a", Some(NEWER)), info("b", Some(NEWER))], Some("a") ; "first_wins_a_tie")]
    #[test_case(vec![info("a", None)], Some("a") ; "an_undated_session_still_counts")]
    #[test_case(vec![], None ; "none_listed")]
    fn newest_picks_the_latest_update(sessions: Vec<SessionInfo>, expected: Option<&str>) {
        let _runtime = crate::test_runtime().enter();
        assert_eq!(newest(sessions).as_ref().map(|id| id.0.as_ref()), expected);
    }

    #[test_case(false, false, false, Some(NO_LOAD) ; "no_load")]
    #[test_case(true, false, true, Some(NO_LIST) ; "continue_without_list")]
    #[test_case(true, false, false, None ; "resume_without_list")]
    #[test_case(true, true, true, None ; "continue_with_both")]
    fn require_load_names_the_missing_capability(
        load: bool,
        list: bool,
        needs_list: bool,
        expected: Option<&str>,
    ) {
        let _runtime = crate::test_runtime().enter();
        let mut capabilities = json!({ "loadSession": load });
        if list {
            capabilities["sessionCapabilities"] = json!({ "list": {} });
        }
        let capabilities: AgentCapabilities = serde_json::from_value(capabilities).unwrap();
        let error = require_load(&capabilities, needs_list)
            .err()
            .map(|e| e.to_string());
        assert_eq!(error.as_deref(), expected);
    }
}
