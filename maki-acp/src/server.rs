use std::collections::HashMap;
use std::env;
use std::fs;
use std::future::Future;
use std::io::{self, Read};
use std::iter;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::v1::{
    AgentNotification, AgentRequest, AgentResponse, CompactionId, CompactionStatus,
    CompactionUpdate, ConfigOptionUpdate, ContentBlock, CurrentModeUpdate,
    EmbeddedResourceResource, Error as AcpError, ImageContent, InitializeRequest, JsonRpcMessage,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, McpServer, NewSessionRequest,
    Notification, PromptRequest, PromptResponse, Request, RequestId, RequestPermissionRequest,
    RequestPermissionResponse, Response, SessionConfigOption, SessionConfigOptionValue, SessionId,
    SessionInfo, SessionModeId, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse, StopReason,
    TextContent,
};
use color_eyre::eyre::Context;
use flume::{Receiver, Sender, WeakSender};
use futures_lite::future;
use maki_agent::agent::{live_snapshot, transcript_into_session};
use maki_agent::btw::side_question;
use maki_agent::headless::{
    self, BackgroundMode, Compaction, InteractiveHandle, InteractiveParams, SessionInput,
    SessionPlugins,
};
use maki_agent::mcp::config::{RawHttpFields, RawStdioFields, RawTransport};
use maki_agent::mcp::{self, McpHandle};
use maki_agent::permissions::{PermissionAnswer, TaggedAnswer};
use maki_agent::session::{
    FORK_TITLE_PREFIX, Resumed, SessionControl, SessionSettings, SharedSettings, StoredSession,
    save_new, store_new,
};
use maki_agent::tools::{
    LocalTool, LocalTools, QUESTION_TOOL_NAME, ToolAudience, WRITE_TOOL_NAME, local_tool,
};
use maki_agent::types::{AgentEvent, DoneReason, ToolDoneEvent, TurnCompleteEvent};
use maki_agent::{
    AgentInput, AgentMode, CancelToken, CostTally, Envelope, HistorySnapshot, ImageMediaType,
    ImageSource, InputSource, SessionEndReason, SessionEvents,
};
use maki_config::project::{self, Runtime, TrustAnswer, TrustMode, policy_grant};
use maki_config::{MAX_SERVER_NAME_LEN, ModelPolicy, ProjectConfig, SessionDefaults, TrustConfig};
use maki_providers::Timeouts;
use maki_providers::model::Model;
use maki_providers::provider::{self, Provider, available_model_specs, fetch_all_models};
use maki_providers::{AgentError, settle_session};
use maki_storage::StateDir;
use maki_storage::id::{MakiId, SessionRef};
use maki_storage::plans;
use maki_storage::sessions::{
    QueueReceipt, QueueStatus, SessionClaim, SessionError, StoredMode, StoredThinking,
    normalize_title,
};
use maki_types::session_snapshot::{
    STATUS_IDLE, STATUS_NEEDS_INPUT, STATUS_WORKING, SessionQueueSnapshot, SessionSnapshot,
};
use maki_wire::btw::{self, BtwChunk, BtwRequest};
use maki_wire::client::{END_OF_INPUT, EVENT_CAPACITY, Link};
use maki_wire::extensions::{self, Agreed};
use maki_wire::login;
use maki_wire::methods::{self, MODE_PLAN};
use maki_wire::plan::{
    self, BUSY, CONFLICT, NO_PLAN, PLAN_TEXT_LIMIT, PlanRead, PlanText, PlanWrite, PlanWritten,
};
use maki_wire::session::{self as session_ops, QueueUpdate, SessionOperation};
use maki_wire::tool_data;
use maki_wire::usage::{
    self, QuotaAnswer, QuotaRequest, QuotaState, SessionLedger, WireTurn, WireUsage,
};
use maki_wire::{commands, elicitation, plugins, settings};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::task::spawn_blocking;
use tokio_util::task::AbortOnDropHandle;
type Task<T> = AbortOnDropHandle<T>;

fn spawn<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> Task<T> {
    #[cfg(test)]
    let handle = test_runtime().spawn(future);
    #[cfg(not(test))]
    let handle = tokio::spawn(future);
    AbortOnDropHandle::new(handle)
}

#[cfg(test)]
fn test_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::LazyLock<tokio::runtime::Runtime> =
        std::sync::LazyLock::new(|| tokio::runtime::Runtime::new().expect("test runtime"));
    &RUNTIME
}

#[cfg(test)]
fn block_on<T>(future: impl Future<Output = T>) -> T {
    let _entered = test_runtime().enter();
    future::block_on(future)
}
use tracing::{debug, info, warn};

use crate::{AcpParams, SessionEndHook, config_options, permissions, translate};

const FIRST_OUTGOING_REQUEST_ID: i64 = 1000;
/// Turns already recorded were priced when they ran. `always_fast` is a live
/// config value, so reading it here would reprice history the user paid for at
/// standard rates. New turns still honour it.
const RESTORED_FAST: bool = false;
/// From JSON-RPC's range for application errors. A session another maki holds
/// is not an internal error (`-32603`), nothing broke here, and the message
/// already says which session and why.
const SESSION_BUSY_CODE: i32 = -32000;
const EMPTY_QUESTION: &str = "a side question needs a question";
/// A plan file in a private directory, when the state dir cannot hold one.
const PLAN_DIR_PREFIX: &str = "maki-plan-";
const PLAN_FALLBACK_FILE: &str = "plan.md";
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
/// A `_`-prefixed compaction status is ours to define: a hook skipped it.
const COMPACTION_SKIPPED: &str = "_skipped";

/// Ids come from here and are never reused, so a late answer for a closed
/// session cannot match a request of the session that replaced it.
static NEXT_OUTGOING_REQUEST_ID: AtomicI64 = AtomicI64::new(FIRST_OUTGOING_REQUEST_ID);

/// What the client still owes us. Every agent waits on its own answer channel,
/// so a child's permission can be outstanding next to the main agent's.
#[derive(Default)]
struct Pending {
    prompt: Option<RequestId>,
    asks: HashMap<i64, Ask>,
    auth: HashMap<Option<String>, Sender<String>>,
}

/// How to read the answer and who gets it. A subagent's permission carries the
/// channel its own agent waits on, everything else answers the main agent.
enum Ask {
    /// `request_id` is the agent-side id of the ask the tool is parked on, not
    /// the JSON-RPC id and not the client-facing tool call: the answer is
    /// tagged with it so a waiter on any other ask refuses it.
    Permission {
        request_id: String,
        answer_tx: Option<Sender<String>>,
    },
    Elicitation,
}

type PendingState = Arc<Mutex<Pending>>;
/// The session's one plan file, shared by `session/set_mode`, the steer
/// source that may switch a run into plan mode, and the event pump that
/// marks a write to it.
type PlanPath = Arc<Mutex<Option<PathBuf>>>;

struct SessionState {
    handle: InteractiveHandle,
    pump: Task<()>,
    mcp: Option<McpHandle>,
    mcp_config_error: Option<String>,
    current_mode: AgentMode,
    /// The session's one plan file. Leaving plan mode and coming back must
    /// find the plan it left, as it does in the TUI.
    plan_path: PlanPath,
    /// As its provider adjusted it on connect, so the settings it offers are
    /// the ones the session runs.
    model: Model,
    /// The user's thinking, fast, workflow and yolo, shared with the driver
    /// so every save reads the latest. Prompts carry their own run's copy.
    settings: SharedSettings,
    pending: PendingState,
    /// Shared with the run, so reloading the same session reads under this
    /// claim instead of clashing with it.
    claim: SessionClaim,
}

impl SessionState {
    fn sync_seed(&self) {
        if let Some(seed) = maki_agent::AgentSeed::live(self.claim.id()) {
            let settings = self.settings.load();
            seed.set_chat_settings(
                maki_providers::RequestOptions {
                    thinking: settings.thinking.map(Into::into).unwrap_or_default(),
                    fast: settings.fast,
                },
                &self.current_mode,
            );
        }
    }
}

struct Server {
    out_tx: Sender<Value>,
    model_specs: Vec<String>,
    model_policy: Arc<ModelPolicy>,
    client_elicits_form: bool,
    /// The client advertised `sessionCapabilities.compaction`, so it gets
    /// `CompactionUpdate`s.
    client_compaction: bool,
    /// Extensions agreed in `initialize`.
    extensions: Agreed,
    sessions: HashMap<SessionId, SessionState>,
    on_session_end: Option<SessionEndHook>,
    /// The side requests still answering. Dropping the server cancels them,
    /// so a stalled provider cannot keep the writer open after the link.
    side_tasks: Vec<Task<()>>,
    /// The latest model discovery; only its batches merge.
    discovery: u64,
    /// Where a discovery a login starts reports, as the first one does.
    incoming: WeakSender<Incoming>,
    /// Offered the terminal login: the client can run it, and will say so.
    terminal_login: bool,
}

impl Server {
    fn keep_side_task(&mut self, task: Task<()>) {
        self.side_tasks.retain(|task| !task.is_finished());
        self.side_tasks.push(task);
    }

    async fn respond(&self, id: RequestId, result: Result<AgentResponse, AcpError>) {
        send(&self.out_tx, Response::<_, AcpError>::new(id, result)).await;
    }
}

/// Discovery batches carry their generation, so a login's fresh listing
/// cannot be extended by a late batch of the one before it.
enum Incoming {
    Line(String),
    Models(u64, Vec<String>),
    Discovered(u64),
}

/// What [`serve_channels`] reads and writes: one JSON-RPC message per
/// string, with no framing.
pub struct ServerLink {
    pub input: Receiver<String>,
    pub output: Sender<String>,
}

/// A client and a server in one process. Each message still crosses as
/// JSON text, so the server validates exactly what it validates over stdio.
pub fn memory_link() -> (Link, ServerLink) {
    let (client_tx, input) = flume::unbounded();
    let (output, client_rx) = flume::bounded(EVENT_CAPACITY);
    (
        Link::new(client_tx, client_rx),
        ServerLink { input, output },
    )
}

pub async fn serve(params: AcpParams) -> color_eyre::Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    serve_stream(params, stdin, tokio::io::stdout())
        .await
        .context("serve ACP over stdio")
}

/// Newline-framed lines between a byte stream and [`serve_channels`]. A write
/// error ends the server and is returned, so a peer that stopped reading never
/// leaves it waiting on an open input.
pub async fn serve_stream(
    params: AcpParams,
    reader: impl AsyncBufRead + Unpin + Send + 'static,
    writer: impl AsyncWrite + Unpin,
) -> io::Result<()> {
    let (input_tx, input) = flume::unbounded();
    let (output, output_rx) = flume::bounded(EVENT_CAPACITY);
    let stop = input_tx.downgrade();
    let (input_end, input_ended) = CancelToken::new();
    let reader_task = spawn(async move {
        let _end = input_end;
        read_lines(reader, input_tx).await
    });
    let stopping = params.stop.clone();
    let ((), written) = future::zip(
        serve_channels(params, input, output),
        write_lines(writer, output_rx, stop, input_ended, stopping),
    )
    .await;
    reader_task.abort();
    let read = reader_task.await.ok();
    written?;
    read.unwrap_or(Ok(()))
}

/// Reads until `input` disconnects or carries [`END_OF_INPUT`], then closes
/// sessions and drains ready output without waiting for a stalled peer.
pub async fn serve_channels(
    mut params: AcpParams,
    input: Receiver<String>,
    output: Sender<String>,
) {
    let (out_tx, out_rx) = flume::bounded::<Value>(EVENT_CAPACITY);
    let (input_end, input_ended) = CancelToken::new();
    let (writer_end, writer_ended) = CancelToken::new();

    let stop = params.stop.clone();
    let writer_task = spawn(async move {
        let _end = writer_end;
        while let Ok(msg) = out_rx.recv_async().await {
            let Ok(text) = serde_json::to_string(&msg) else {
                continue;
            };
            // Drain ready output on EOF, but do not wait on a stalled peer.
            if !future::or(async { output.send_async(text).await.is_ok() }, async {
                future::or(input_ended.cancelled(), stop.cancelled()).await;
                false
            })
            .await
            {
                return;
            }
        }
    });

    let (in_tx, in_rx) = flume::unbounded::<Incoming>();
    let mut server = Server {
        out_tx,
        model_specs: available_model_specs(&params.model_policy),
        model_policy: Arc::clone(&params.model_policy),
        client_elicits_form: false,
        client_compaction: false,
        extensions: Agreed::default(),
        sessions: HashMap::new(),
        on_session_end: params.on_session_end.clone(),
        side_tasks: Vec::new(),
        discovery: 0,
        incoming: in_tx.downgrade(),
        terminal_login: false,
    };

    // Weak, so a discovery still in flight cannot keep the loop alive once the input ends.
    discover_models(Arc::clone(&params.model_policy), in_tx.downgrade(), 0);
    // Held, so cancelling the server cancels the forwarding with it.
    let _forward = spawn(async move {
        let _end = input_end;
        forward_input(input, in_tx).await;
    });

    while let Some(incoming) = future::or(
        async {
            future::or(writer_ended.cancelled(), params.stop.cancelled()).await;
            None
        },
        async { in_rx.recv_async().await.ok() },
    )
    .await
    {
        match incoming {
            Incoming::Line(line) => {
                handle_line(&mut server, &line, &mut params).await;
                if params.initial.as_ref().is_some_and(|(session, _)| {
                    server
                        .sessions
                        .values()
                        .any(|running| running.claim.id() == session.id)
                }) {
                    params.initial = None;
                }
            }
            Incoming::Models(generation, specs) => {
                refresh_models(&mut server, generation, specs).await
            }
            Incoming::Discovered(generation) => {
                discovered(&mut server, generation, params.timeouts).await
            }
        }
    }

    while let Some(id) = server.sessions.keys().next().cloned() {
        close_session(&mut server, &id, SessionEndReason::Shutdown).await;
    }
    drop(server);
    let _ = writer_task.await;
}

/// The only strong sender of the loop's input, so the loop ends with it.
async fn forward_input(input: Receiver<String>, tx: Sender<Incoming>) {
    while let Ok(line) = input.recv_async().await {
        if line == END_OF_INPUT || tx.send_async(Incoming::Line(line)).await.is_err() {
            return;
        }
    }
}

/// Lives in its own task because `read_line` is not cancel safe: a read
/// dropped while the server ends would eat half a line.
async fn read_lines(mut reader: impl AsyncBufRead + Unpin, tx: Sender<String>) -> io::Result<()> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 || tx.send_async(line).await.is_err() {
            return Ok(());
        }
    }
}

/// `input` is weak, so a reader that reached the end still disconnects it.
async fn write_lines(
    mut writer: impl AsyncWrite + Unpin,
    output: Receiver<String>,
    input: WeakSender<String>,
    input_ended: CancelToken,
    stop: CancelToken,
) -> io::Result<()> {
    let written: io::Result<()> = async {
        while let Ok(message) = output.recv_async().await {
            let write = async {
                writer.write_all(message.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await
            };
            match future::or(async { Some(write.await) }, async {
                future::or(input_ended.cancelled(), stop.cancelled()).await;
                None
            })
            .await
            {
                Some(result) => result?,
                None => return Ok(()),
            }
        }
        Ok(())
    }
    .await;
    written.inspect_err(|error| {
        warn!(%error, "ACP link write failed, stopping the input");
        if let Some(input) = input.upgrade() {
            let _ = input.send(END_OF_INPUT.to_owned());
        }
    })
}

/// Static manifests miss providers that only list their models over the wire
/// (OpenRouter and friends), so the same discovery the TUI runs happens here,
/// in the background. Each batch leaves the moment it lands: the slowest source
/// is a cold catalog download, and a provider the client could already pick from
/// should not wait behind it.
fn discover_models(policy: Arc<ModelPolicy>, tx: WeakSender<Incoming>, generation: u64) {
    // Nobody left to hear the listing, so asking every provider is waste.
    if tx.upgrade().is_none() {
        return;
    }
    tokio::spawn(async move {
        fetch_all_models(
            &policy,
            |batch| {
                if let Some(tx) = tx.upgrade() {
                    let _ = tx.send(Incoming::Models(generation, batch.models));
                }
            },
            None,
        )
        .await;
        if let Some(tx) = tx.upgrade() {
            let _ = tx.send(Incoming::Discovered(generation));
        }
    });
}

/// Discovery lands in batches after the client built its selector from the
/// offline list, so every batch that adds something announces the fuller list.
async fn refresh_models(srv: &mut Server, generation: u64, batch: Vec<String>) {
    if generation != srv.discovery {
        return;
    }
    let known = srv.model_specs.len();
    for spec in batch {
        if !srv.model_specs.contains(&spec) {
            srv.model_specs.push(spec);
        }
    }
    if srv.model_specs.len() == known {
        return;
    }
    for (id, session) in &srv.sessions {
        session_update(
            &srv.out_tx,
            id,
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(session_options(
                &srv.model_specs,
                session,
            ))),
        )
        .await;
    }
}

async fn handle_line(server: &mut Server, line: &str, params: &mut AcpParams) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }

    let raw: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "invalid JSON on stdin");
            server
                .respond(RequestId::Null, Err(AcpError::parse_error()))
                .await;
            return;
        }
    };

    let id = raw.get("id").map(request_id);

    if raw.get("result").is_some() || raw.get("error").is_some() {
        handle_incoming_response(server, &raw);
    } else if let Some(method) = raw.get("method").and_then(Value::as_str) {
        match id {
            Some(id) => handle_request(server, method, id, &raw, params).await,
            None if method == login::CHANGED_METHOD => auth_changed(server, params).await,
            None => handle_notification(server, method, &raw).await,
        }
    } else if let Some(id) = id {
        server.respond(id, Err(AcpError::invalid_request())).await;
    }
}

fn request_id(v: &Value) -> RequestId {
    serde_json::from_value(v.clone()).unwrap_or(RequestId::Null)
}

async fn handle_request(
    srv: &mut Server,
    method: &str,
    id: RequestId,
    raw: &Value,
    params: &mut AcpParams,
) {
    let result = match method {
        "initialize" => {
            let request = parse_params::<InitializeRequest>(raw).ok();
            srv.client_elicits_form = request
                .as_ref()
                .is_some_and(|req| elicitation::supports_form(&req.client_capabilities));
            srv.client_compaction = request.as_ref().is_some_and(|req| {
                req.client_capabilities
                    .session
                    .as_ref()
                    .is_some_and(|session| session.compaction.is_some())
            });
            srv.terminal_login = request
                .as_ref()
                .is_some_and(|req| req.client_capabilities.auth.terminal);
            srv.extensions = request
                .map(|req| extensions::agreed(req.meta.as_ref()))
                .unwrap_or_default();
            srv.extensions.plugins &= params.administration.is_some();
            srv.terminal_login &= srv.extensions.login;
            let response = methods::initialize_response(srv.terminal_login);
            Ok(AgentResponse::InitializeResponse(
                match extensions::response_meta(&srv.extensions) {
                    Some(meta) => response.meta(meta),
                    None => response,
                },
            ))
        }
        plugins::METHOD => {
            let result = plugin_operation(srv, raw, params).await;
            send(&srv.out_tx, Response::<_, AcpError>::new(id, result)).await;
            return;
        }
        session_ops::METHOD => {
            match session_operation(srv, raw, params).await {
                Ok(value) => send(&srv.out_tx, Response::<_, AcpError>::new(id, Ok(value))).await,
                Err(error) => srv.respond(id, Err(error)).await,
            }
            return;
        }
        "session/new" => new_session(srv, raw, params).await,
        "session/load" => load_session(srv, raw, params).await,
        "session/list" => list_sessions(raw, &params.storage).await,
        "session/prompt" => match handle_prompt(srv, raw, &id).await {
            Ok(()) => return,
            Err(e) => Err(e),
        },
        "session/set_mode" => handle_set_mode(srv, raw).await,
        "session/set_config_option" => handle_set_config(srv, raw, params.timeouts).await,
        btw::BTW_METHOD => match handle_btw(srv, raw, &id, params.timeouts) {
            Ok(()) => return,
            Err(e) => Err(e),
        },
        usage::QUOTA_METHOD => match handle_quota(srv, raw, &id, params.timeouts) {
            Ok(()) => return,
            Err(e) => Err(e),
        },
        plan::READ_METHOD => match handle_plan_read(srv, raw, &id).await {
            Ok(()) => return,
            Err(e) => Err(e),
        },
        plan::WRITE_METHOD => match handle_plan_write(srv, raw, &id).await {
            Ok(()) => return,
            Err(e) => Err(e),
        },
        _ => Err(AcpError::method_not_found()),
    };
    let opened = match &result {
        Ok(AgentResponse::NewSessionResponse(response)) => Some(response.session_id.clone()),
        Ok(AgentResponse::LoadSessionResponse(_)) => parse_params::<LoadSessionRequest>(raw)
            .ok()
            .map(|request| request.session_id),
        _ => None,
    };
    srv.respond(id, result).await;
    if let Some(id) = opened {
        session_update(&srv.out_tx, &id, commands::advertised()).await;
    }
}

async fn plugin_operation(
    srv: &mut Server,
    raw: &Value,
    params: &mut AcpParams,
) -> Result<plugins::PluginReply, AcpError> {
    let request = parse_params(raw)?;
    let administer = params
        .administration
        .clone()
        .filter(|_| srv.extensions.plugins)
        .ok_or_else(|| AcpError::method_not_found().data(plugins::UNSUPPORTED))?;
    while let Some(id) = srv.sessions.keys().next().cloned() {
        close_session(srv, &id, SessionEndReason::Reload).await;
    }
    let previous = params.clone();
    let (next, reply) = spawn_blocking(move || administer(request, previous))
        .await
        .map_err(|error| AcpError::internal_error().data(json_str(&error)))?
        .map_err(|error| invalid(&error))?;
    *params = next;
    srv.on_session_end = params.on_session_end.clone();
    srv.model_policy = params.model_policy.clone();
    auth_changed(srv, params).await;
    Ok(reply)
}

fn session_snapshot(
    srv: &Server,
    named: &SessionId,
    storage: &StateDir,
) -> Result<StoredSession, AcpError> {
    let id = parse_session_id(named)?;
    match srv
        .sessions
        .values()
        .find(|session| session.claim.id() == id)
    {
        Some(session) => {
            let mut snapshot = session.handle.session.snapshot();
            snapshot.set_model(session.model.spec());
            snapshot.meta.mode = Some(match session.current_mode {
                AgentMode::Build => StoredMode::Build,
                AgentMode::Plan(_) => StoredMode::Plan,
            });
            Ok(snapshot)
        }
        None => StoredSession::read_only(id, storage).map_err(|error| load_error(error, id)),
    }
}

fn parse_session_id(named: &SessionId) -> Result<MakiId, AcpError> {
    named
        .0
        .parse()
        .map_err(|_| AcpError::resource_not_found(Some(named.to_string())))
}

async fn session_operation(
    srv: &mut Server,
    raw: &Value,
    params: &AcpParams,
) -> Result<Value, AcpError> {
    let operation: SessionOperation = parse_params(raw)?;
    let storage = &params.storage;
    match operation {
        SessionOperation::RetryAuth { session_id, id } => {
            let session = running_session(srv, &session_id)?;
            let answer = session
                .pending
                .lock()
                .unwrap()
                .auth
                .remove(&id)
                .ok_or_else(|| invalid(&"no authentication retry is pending"))?;
            answer
                .try_send(String::new())
                .map_err(|error| invalid(&error))?;
            Ok(json!(true))
        }
        SessionOperation::MoveTool { session_id, id } => {
            running_session(srv, &session_id)?;
            let outcome = match &params.plugins {
                Some(plugins) => plugins.move_tool(id).await,
                None => maki_types::MoveOutcome::NoHook,
            };
            serde_json::to_value(outcome).map_err(|error| invalid(&error))
        }
        SessionOperation::Enqueue {
            session_id,
            id,
            mut input,
        } => {
            let session = running_session(srv, &session_id)?;
            if session.handle.task.is_finished() {
                return Err(AcpError::new(-32603, "session ended"));
            }
            if let SessionInput::Prompt(input) = input.as_mut() {
                input.source = InputSource::Acp;
                if input.mode.plan_path().is_some() {
                    input.mode = plan_mode(&session.plan_path, storage, &env::temp_dir());
                }
            }
            let receipt = session
                .handle
                .session
                .enqueue_with_id(&input, id.unwrap_or_else(MakiId::generate))
                .map_err(|error| invalid(&error))?;
            serde_json::to_value(receipt).map_err(|error| invalid(&error))
        }
        SessionOperation::Rewind {
            session_id,
            turn_index,
            prompt,
        } => {
            let session = running_session(srv, &session_id)?;
            let stored = session
                .handle
                .rewind(turn_index, prompt)
                .await
                .map_err(|error| invalid(&error))?;
            serde_json::to_value(stored).map_err(|error| invalid(&error))
        }
        SessionOperation::TaskInput {
            session_id,
            id,
            mut input,
        } => {
            let inbox = running_session(srv, &session_id)?
                .handle
                .session
                .task_inbox(&id)
                .map_err(|error| invalid(&error))?;
            input.mode = AgentMode::Build;
            input.source = InputSource::Acp;
            inbox.push(*input);
            serde_json::to_value(inbox.snapshot()).map_err(|error| invalid(&error))
        }
        SessionOperation::TaskPop { session_id, id } => {
            let inbox = running_session(srv, &session_id)?
                .handle
                .session
                .task_inbox(&id)
                .map_err(|error| invalid(&error))?;
            inbox.remove(0);
            serde_json::to_value(inbox.snapshot()).map_err(|error| invalid(&error))
        }
        SessionOperation::RemoveTask { session_id, id } => {
            running_session(srv, &session_id)?
                .handle
                .session
                .remove_task(&id)
                .map_err(|error| invalid(&error))?;
            Ok(json!(true))
        }
        SessionOperation::CancelSubagent { session_id, id } => {
            let session = running_session(srv, &session_id)?;
            session.handle.cancels.subagents.cancel(id.clone());
            if let Some(plugins) = &params.plugins {
                plugins.stop_task(&id);
            }
            Ok(json!(true))
        }
        SessionOperation::Close { session_id, reason } => {
            let session = running_session(srv, &session_id)?;
            let control = session.handle.session.clone();
            let model = session.model.spec();
            let reason = reason.unwrap_or(SessionEndReason::Load);
            if reason == SessionEndReason::Reset
                && let Some(plugins) = &params.plugins
            {
                plugins
                    .fire(session.claim.id(), "SessionReset", json!({}))
                    .map_err(|error| invalid(&error))?;
            }
            close_session(srv, &session_id, reason).await;
            let mut stored = control.snapshot();
            stored.set_model(model);
            serde_json::to_value(stored).map_err(|error| invalid(&error))
        }
        SessionOperation::CancelQueued { session_id, id } => {
            let session = running_session(srv, &session_id)?;
            let receipt = session
                .handle
                .session
                .cancel_queued(id)
                .map_err(|error| invalid(&error))?;
            queue_update(&srv.out_tx, &session_id, receipt.clone()).await;
            serde_json::to_value(receipt).map_err(|error| invalid(&error))
        }

        SessionOperation::Mcp { session_id, toggle } => {
            let session = running_session(srv, &session_id)?;
            let mut snapshot = if let Some(mcp) = &session.mcp {
                if let Some((server, enabled)) = toggle {
                    mcp.send(mcp::McpCommand::Toggle { server, enabled });
                }
                (*mcp.reader().load_full()).clone()
            } else {
                mcp::McpSnapshot::default()
            };
            snapshot.config_error = session.mcp_config_error.clone();
            serde_json::to_value(snapshot).map_err(|error| invalid(&error))
        }
        SessionOperation::ChangeDirectory { session_id, path } => {
            let session = running_session(srv, &session_id)?;
            let path = maki_storage::paths::expand_tilde(Path::new(&path));
            let path = PathBuf::from(&session.handle.session.snapshot().cwd).join(path);
            let path = path.canonicalize().map_err(|error| invalid(&error))?;
            if !path.is_dir() {
                return Err(invalid(&"not a directory"));
            }
            let cwd = path.to_string_lossy().into_owned();
            session
                .handle
                .session
                .set_cwd(cwd.clone())
                .map_err(|error| invalid(&error))?;
            if let Some(seed) = maki_agent::AgentSeed::live(session.claim.id()) {
                seed.set_working_dir(&path);
            }
            if let Some(plugins) = &params.plugins {
                plugins
                    .fire(
                        session.claim.id(),
                        "SessionCwdChanged",
                        json!({ "cwd": cwd }),
                    )
                    .map_err(|error| invalid(&error))?;
            }
            Ok(json!(cwd))
        }
        SessionOperation::FireEvent {
            session_id,
            event,
            data,
        } => {
            let session = running_session(srv, &session_id)?;
            let plugins = params
                .plugins
                .as_ref()
                .ok_or_else(|| invalid(&"no plugins attached"))?;
            plugins
                .fire(session.claim.id(), &event, data)
                .map_err(|error| invalid(&error))?;
            Ok(json!(true))
        }
        SessionOperation::StopPluginTask { session_id, id } => {
            let session = running_session(srv, &session_id)?;
            if !session
                .handle
                .session
                .snapshot()
                .subagents()
                .iter()
                .any(|task| task.tool_use_id == id && task.plugin.is_some())
            {
                return Err(invalid(&"plugin task not found"));
            }
            let plugins = params
                .plugins
                .as_ref()
                .ok_or_else(|| invalid(&"no plugins attached"))?;
            plugins.stop_task(&id);
            Ok(json!(true))
        }
        SessionOperation::Snapshot { session_id } => {
            serde_json::to_value(session_snapshot(srv, &session_id, storage)?)
                .map_err(|error| invalid(&error))
        }
        SessionOperation::Read { session_id } => {
            let stored = session_snapshot(srv, &session_id, storage)?;
            let mut snapshot = SessionSnapshot::stored(&stored);
            snapshot.id = session_id.to_string();
            snapshot.queue = Some(SessionQueueSnapshot {
                count: stored
                    .meta
                    .accepted_work
                    .iter()
                    .filter(|work| work.status == QueueStatus::Queued)
                    .count(),
            });
            if let Some(session) = srv
                .sessions
                .values()
                .find(|session| session.claim.id() == stored.id)
            {
                let pending = session.pending.lock().unwrap();
                snapshot.status = if !pending.asks.is_empty() || !pending.auth.is_empty() {
                    STATUS_NEEDS_INPUT
                } else if pending.prompt.is_some()
                    || stored
                        .meta
                        .accepted_work
                        .iter()
                        .any(|work| work.status == QueueStatus::Running)
                {
                    STATUS_WORKING
                } else {
                    STATUS_IDLE
                };
                snapshot.context_window = Some(session.model.context_window);
            }
            let mut value = serde_json::to_value(snapshot)
                .map_err(|error| AcpError::internal_error().data(json_str(&error)))?;
            value["background_tasks"] = json!(
                srv.sessions
                    .get(&session_id)
                    .map(|session| session.handle.session.background_tasks())
                    .unwrap_or_default()
            );
            Ok(value)
        }

        SessionOperation::Import { parent, transcript } => {
            let parent = session_snapshot(srv, &parent, storage)?;
            let mut base = StoredSession::new(&parent.model, &parent.cwd);
            base.meta.mode = parent.meta.mode;
            base.meta.thinking = parent.meta.thinking;
            base.meta.fast = parent.meta.fast;
            base.meta.workflow = parent.meta.workflow;
            let imported =
                transcript_into_session(*transcript, base).map_err(|error| invalid(&error))?;
            let id = store_new(imported, storage).map_err(|error| invalid(&error))?;
            Ok(json!({ "sessionId": id }))
        }
        SessionOperation::Fork { session_id, turn } => {
            let mut stored = session_snapshot(srv, &session_id, storage)?;
            if let Some((index, prompt, writers)) = turn {
                let size = stored.meta.context_size;
                stored.meta.context_size =
                    maki_types::session::truncate_to_turn(&mut stored, index, size);
                stored.meta.input_draft = Some(prompt);
                stored.meta.input_draft_writers = writers;
            }
            let cwd = stored.cwd.clone();
            let claim = SessionClaim::fresh(storage);
            let mut forked = stored
                .fork(claim.id(), &cwd, storage)
                .map_err(|error| AcpError::internal_error().data(json_str(&error)))?;
            forked.update_title_if_default();
            forked.set_title(format!("{FORK_TITLE_PREFIX}{}", forked.title));
            let id = save_new(forked, &claim, storage).map_err(|error| invalid(&error))?;
            Ok(json!({ "sessionId": id }))
        }
        SessionOperation::SetTitle { session_id, title } => {
            let title = normalize_title(&title);
            let id = parse_session_id(&session_id)?;
            if let Some(session) = srv
                .sessions
                .values()
                .find(|session| session.claim.id() == id)
            {
                session
                    .handle
                    .session
                    .set_title(title)
                    .map_err(|error| load_error(error, session.claim.id()))?;
            } else {
                let (mut stored, claim) = StoredSession::claim_and_load(id, storage)
                    .map_err(|error| load_error(error, id))?;
                stored.set_title(title);
                stored
                    .save(&claim, storage)
                    .map_err(|error| load_error(error, id))?;
            }
            Ok(json!(true))
        }
        SessionOperation::Delete { session_id } => {
            let id = parse_session_id(&session_id)?;
            let live = srv
                .sessions
                .iter()
                .find(|(_, session)| session.claim.id() == id)
                .map(|(named, session)| (named.clone(), session.claim.clone()));
            let was_live = live.is_some();
            let claim = match live {
                Some((named, claim)) => {
                    close_session(srv, &named, SessionEndReason::Replaced).await;
                    claim
                }
                None => {
                    SessionClaim::acquire(id, storage).map_err(|error| load_error(error, id))?
                }
            };
            if let Err(error) = StoredSession::delete(&claim, storage)
                && !(was_live && error.is_not_found())
            {
                return Err(load_error(error, id));
            }
            Ok(json!(true))
        }
    }
}

async fn new_session(
    srv: &mut Server,
    raw: &Value,
    params: &AcpParams,
) -> Result<AgentResponse, AcpError> {
    let req: NewSessionRequest = parse_params(raw)?;
    let project_config = trusted_project_config(
        &req.cwd,
        &params.storage,
        params.trust_mode,
        &params.trust_policy,
    );
    let (mcp, mcp_config_error) =
        start_mcp(&req.cwd, &req.mcp_servers, project_config.clone()).await;
    let declared = startup_model(params);
    let claim = SessionClaim::fresh(&params.storage);
    let session_ref = start_session(
        srv,
        params,
        req.cwd,
        Resumed::empty(SessionRef::from(claim.id())),
        claim,
        mcp,
        mcp_config_error,
        project_config,
        PumpUsage::new(
            srv.extensions.usage,
            SessionLedger::default(),
            params.model.subsidised_by.clone(),
        )
        .sharing_ledger(srv.extensions.usage_ledger),
        SessionSettings {
            thinking: params.defaults.thinking,
            fast: params.defaults.fast,
            workflow: params.defaults.workflow,
            yolo: None,
        },
        resolved(&declared, params.timeouts).await,
        declared,
    );
    maki_otel::emit::session_started(maki_otel::emit::START_FRESH, Some(session_ref.as_str()));
    let resp = methods::new_session_response(session_ref.as_str()).config_options(session_options(
        &srv.model_specs,
        running_session(srv, &SessionId::from(session_ref.to_string()))?,
    ));
    Ok(AgentResponse::NewSessionResponse(resp))
}

async fn load_session(
    srv: &mut Server,
    raw: &Value,
    params: &AcpParams,
) -> Result<AgentResponse, AcpError> {
    let req: LoadSessionRequest = parse_params(raw)?;
    let session_ref: SessionRef = req
        .session_id
        .0
        .parse()
        .map_err(|_| AcpError::resource_not_found(Some(req.session_id.0.to_string())))?;
    // Reloading the session we already run keeps its claim, so no other process
    // slips in, and closes the run first so the read includes its last turn.
    // Any other session is read first, so a failed load leaves the running one
    // alone.
    let held = srv
        .sessions
        .iter()
        .find(|(_, state)| state.claim.id() == session_ref.id())
        .map(|(id, state)| (id.clone(), state.claim.clone()));
    let (restored, claim) = match held {
        Some((id, claim)) => {
            close_session(srv, &id, SessionEndReason::Replaced).await;
            (reload_history(&params.storage, &claim)?, claim)
        }
        None => match &params.initial {
            Some((stored, claim)) if stored.id == session_ref.id() => (
                Restored {
                    session: stored.clone(),
                    cwd: Some(PathBuf::from(&stored.cwd)),
                },
                claim.clone(),
            ),
            _ => claim_history(&params.storage, session_ref.id())?,
        },
    };
    let project_config = trusted_project_config(
        &req.cwd,
        &params.storage,
        params.trust_mode,
        &params.trust_policy,
    );
    let (mcp, mcp_config_error) =
        start_mcp(&req.cwd, &req.mcp_servers, project_config.clone()).await;
    let sid = SessionId::from(session_ref.to_string());
    if srv.extensions.sessions {
        send(
            &srv.out_tx,
            Notification {
                method: session_ops::RESTORE_METHOD.into(),
                params: Some(&restored.session),
            },
        )
        .await;
    } else {
        let home = maki_storage::paths::home();
        let replay = translate::Replay {
            cwd: restored.cwd.as_deref().unwrap_or(&req.cwd),
            home: home.as_deref(),
            outputs: srv
                .extensions
                .tool_data
                .then(|| restored.session.tool_outputs()),
            data: restored.session.tool_data(),
            tool_data: srv.extensions.tool_data,
        };
        for update in translate::replay_history(restored.session.messages(), &replay) {
            session_update(&srv.out_tx, &sid, update).await;
        }
    }
    // Priced against the model the session recorded, not the one selected now
    // (which may cost 10x more or less). Later turns add their own exact cost.
    let recorded_model =
        Model::from_spec(&restored.session.model).unwrap_or_else(|_| params.model.clone());
    // Settling writes today's estimate into entries that never recorded a cost.
    // The run saves this session back, so we settle a copy and keep that
    // estimate off disk.
    let mut by_model = restored.session.usage_by_model().clone();
    let restored_cost = settle_session(
        &restored.session.token_usage,
        &mut by_model,
        &recorded_model,
        RESTORED_FAST,
    );
    // Nothing to settle: the list price was recorded per turn, so a load
    // adds the rows back up, as a local load does.
    let restored_list_cost = by_model
        .values()
        .filter_map(|usage| usage.subsidised_list_cost)
        .reduce(|total, cost| total + cost);
    let mut usage = PumpUsage::new(
        srv.extensions.usage,
        SessionLedger {
            total: restored.session.token_usage,
            cost: restored_cost,
            list_cost: restored_list_cost,
            by_model,
        },
        params.model.subsidised_by.clone(),
    )
    .sharing_ledger(srv.extensions.usage_ledger);
    // The session goes on with the startup model, resolved as its turns run
    // it, so the gauge is sized as those turns will size it.
    let declared = startup_model(params);
    let session_model = resolved(&declared, params.timeouts).await;
    // The context gauge a local load shows, after the history it measures.
    // It also seeds the pump, so spending before the next main turn has a
    // gauge to report against.
    session_update(
        &srv.out_tx,
        &sid,
        usage.gauge(
            restored.session.meta.context_size,
            session_model.context_window,
        ),
    )
    .await;
    let settings = SessionSettings::of(&restored.session);
    let yolo = settings.yolo;
    let started = start_session(
        srv,
        params,
        req.cwd,
        Resumed::stored(session_ref, restored.session),
        claim,
        mcp,
        mcp_config_error,
        project_config,
        usage,
        settings,
        session_model,
        declared,
    );
    let session = running_session(srv, &SessionId::from(started.to_string()))?;
    session.handle.permissions.set_session_yolo(yolo);
    maki_otel::emit::session_started(maki_otel::emit::START_RESUME, Some(started.as_str()));
    let mode = match session.current_mode {
        AgentMode::Build => methods::MODE_BUILD,
        AgentMode::Plan(_) => MODE_PLAN,
    };
    let resp = methods::load_session_response()
        .modes(methods::mode_state(mode))
        .config_options(session_options(&srv.model_specs, session));
    Ok(AgentResponse::LoadSessionResponse(resp))
}

/// `session/list`: stored sessions, newest first. Reads the store, not
/// the live session, so it works before any session is open. The
/// optional cwd filters by exact match against the stored cwd, like the
/// CLI and TUI pickers: a path spelled differently (symlink, trailing
/// slash) matches nothing. Without a cwd all stored sessions are
/// listed, like `maki session list --global`. `cursor` is ignored; the
/// response is a single page.
async fn list_sessions(raw: &Value, storage: &StateDir) -> Result<AgentResponse, AcpError> {
    let req: ListSessionsRequest = parse_params(raw)?;
    // The scan stats every session file and tail-reads those with a cold
    // cache, so it runs on a blocking thread, like the TUI picker.
    let storage = storage.clone();
    let internal = |e: &dyn std::fmt::Display| AcpError::internal_error().data(json_str(&e));
    let summaries = spawn_blocking(move || match req.cwd {
        Some(cwd) => StoredSession::list(&cwd.to_string_lossy(), &storage),
        None => StoredSession::list_all(&storage),
    })
    .await
    .map_err(|e| internal(&e))?
    .map_err(|e| internal(&e))?;
    let sessions = summaries
        .into_iter()
        .map(|s| {
            let info = SessionInfo::new(s.id.to_string(), s.cwd).title(s.title);
            match iso_from_epoch(s.updated_at) {
                Some(ts) => info.updated_at(ts),
                None => info,
            }
        })
        .collect();
    Ok(AgentResponse::ListSessionsResponse(
        ListSessionsResponse::new(sessions),
    ))
}

/// Maki stores epoch seconds; ACP requires an ISO 8601 string. The
/// store writes 0 when a transcript's tail meta could not be read,
/// meaning "unknown", which must not be rendered as 1970-01-01.
fn iso_from_epoch(secs: u64) -> Option<String> {
    if secs == 0 {
        return None;
    }
    let secs = i64::try_from(secs).ok()?;
    jiff::Timestamp::from_second(secs)
        .ok()
        .map(|t| t.to_string())
}

async fn queue_update(out: &Sender<Value>, session_id: &SessionId, receipt: QueueReceipt) {
    send(
        out,
        Notification {
            method: session_ops::QUEUE_UPDATE_METHOD.into(),
            params: Some(QueueUpdate {
                session_id: session_id.clone(),
                receipt,
            }),
        },
    )
    .await;
}

/// Spawns a session and installs it as the server's current one. Spawning
/// alone is not a useful state: the event stream has exactly one reader, so it
/// must be handed to the pump here rather than travel any further.
#[allow(clippy::too_many_arguments)]
fn start_session(
    srv: &mut Server,
    params: &AcpParams,
    cwd: PathBuf,
    resumed: Resumed,
    claim: SessionClaim,
    mcp: Option<McpHandle>,
    mcp_config_error: Option<String>,
    project_config: ProjectConfig,
    usage: PumpUsage,
    settings: SessionSettings,
    model: Model,
    declared: Model,
) -> SessionRef {
    let settings = settings.shared();
    let pending = PendingState::default();
    // Without a form to draw on the client the question tool would spin
    // forever waiting for a TUI that does not exist, so it is dropped and the
    // model asks in plain text instead.
    let (excluded_tools, local_tools) = if srv.client_elicits_form || srv.extensions.question {
        let tool = question_tool(
            srv.out_tx.downgrade(),
            Arc::clone(&pending),
            srv.extensions.question,
        );
        let map: LocalTools = Arc::new(HashMap::from([(QUESTION_TOOL_NAME.to_owned(), tool)]));
        (Vec::new(), map)
    } else {
        (vec![QUESTION_TOOL_NAME], LocalTools::default())
    };
    // The ACP process cwd owns env, Lua, and application config, but the client
    // picks the session cwd. So permissions, where a saved answer lands, and
    // MCP config all follow the session's project, not ours.
    let project_trusted = project_config.is_trusted();
    let permissions_config = maki_config::load_permissions(&project_config);
    let plan_path =
        Arc::new(Mutex::new(resumed.session.as_ref().and_then(|session| {
            session.meta.plan_path.as_ref().map(PathBuf::from)
        })));
    let current_mode = match resumed
        .session
        .as_ref()
        .and_then(|session| session.meta.mode)
    {
        Some(StoredMode::Plan) => plan_mode(&plan_path, &params.storage, &env::temp_dir()),
        _ => AgentMode::Build,
    };
    let (handle, events) = headless::spawn_interactive(InteractiveParams {
        plugins: params.plugins.clone(),
        model: declared,
        config: params.config.clone(),
        permissions_config,
        timeouts: params.timeouts,
        prompt_slots: Arc::clone(&params.prompt_slots),
        excluded_tools,
        mcp_handle: mcp.clone(),
        initial_wd: cwd.clone(),
        resumed,
        claim: claim.clone(),
        storage: params.storage.clone(),
        yolo: params.yolo,
        unattended: false,
        auto_wake: srv.extensions.sessions,
        reauth: srv.extensions.sessions,
        system_prompt_override: None,
        append_system_prompt: None,
        defaults: params.defaults,
        model_policy: Arc::clone(&params.model_policy),
        plugin_rules: Arc::clone(&params.plugin_rules),
        project_config,
        local_tools,
        settings: Some(Arc::clone(&settings)),
        background: if params.background {
            BackgroundMode::Detached
        } else {
            BackgroundMode::Blocking
        },
        stop: CancelToken::none(),
    });
    let session_ref = handle.session_id.clone();
    let pump = start_event_pump(
        events,
        handle.answer_tx.clone(),
        session_ref.clone(),
        srv.out_tx.clone(),
        Arc::clone(&pending),
        cwd,
        maki_storage::paths::home(),
        project_trusted,
        usage,
        Compactions::new(srv.client_compaction),
        srv.extensions.tool_data,
        srv.extensions.plan.then(|| Arc::clone(&plan_path)),
        srv.extensions.sessions.then(|| handle.session.clone()),
        params.plugins.clone(),
    );
    srv.sessions.insert(
        SessionId::from(session_ref.to_string()),
        SessionState {
            mcp_config_error,
            handle,
            pump,
            mcp,
            current_mode,
            plan_path,
            model,
            settings,
            pending,
            claim,
        },
    );
    srv.sessions[&SessionId::from(session_ref.to_string())].sync_seed();
    session_ref
}

fn plan_mode(plan_path: &PlanPath, storage: &StateDir, scratch: &Path) -> AgentMode {
    let mut plan_path = plan_path.lock().unwrap();
    if plan_path.is_none() {
        *plan_path = plans::new_plan_path(storage)
            .or_else(|_| private_plan_path(scratch))
            .inspect_err(|error| warn!(%error, "no plan file available"))
            .ok();
    }
    AgentMode::Plan(plan_path.clone().unwrap_or_default())
}

/// A directory only this process made, so no other session or user can
/// share or swap the plan file a plan-mode run may write without asking.
/// It outlives the session, as a plan in the state dir does.
fn private_plan_path(scratch: &Path) -> io::Result<PathBuf> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(PLAN_DIR_PREFIX);
    #[cfg(unix)]
    builder.permissions(fs::Permissions::from_mode(PRIVATE_DIR_MODE));
    Ok(builder.tempdir_in(scratch)?.keep().join(PLAN_FALLBACK_FILE))
}

async fn ask_client(
    out_tx: &Sender<Value>,
    pending: &PendingState,
    ask: Ask,
    request: AgentRequest,
) -> i64 {
    let method = request.method().to_owned();
    let id = NEXT_OUTGOING_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    pending.lock().unwrap().asks.insert(id, ask);
    send(
        out_tx,
        Request {
            id: RequestId::Number(id),
            method: Arc::from(method),
            params: Some(request),
        },
    )
    .await;
    id
}

/// Shadows the Lua `question` tool: sends `elicitation/create` to the client
/// and blocks the tool call until the form comes back. Serializes on the same
/// answer channel as main-agent permissions.
/// Weak, because `LocalTools` is captured verbatim by every `LuaCtx` a tool
/// call parks and an idle VM never collects it. A strong clone here outlives
/// `serve()` and leaves `writer_task` waiting on a sender nobody will drop.
fn question_tool(out_tx: WeakSender<Value>, pending: PendingState, extension: bool) -> LocalTool {
    // The audience the Lua `question` tool carries: shadowing a tool must not
    // widen who may call it.
    local_tool(ToolAudience::MAIN, move |input, ctx| {
        let out_tx = out_tx.clone();
        let pending = Arc::clone(&pending);
        Box::pin(async move {
            let session_id = ctx
                .session_id
                .as_ref()
                .map(ToString::to_string)
                .ok_or("no session")?;
            // Batch/code_execution children dispatch with an empty id; a
            // scope pointing at a tool call the client never saw would get
            // the elicitation rejected or dropped.
            let tool_call_id = ctx.tool_use_id.filter(|id| !id.is_empty());
            let out_tx = out_tx.upgrade().ok_or("connection closed")?;
            let request = elicitation::form_request(&session_id, tool_call_id, &input, extension)?;
            let rx = ctx.user_response_rx.as_ref().ok_or("no answer channel")?;

            let guard = rx.lock().await;
            let request = AgentRequest::CreateElicitationRequest(request);
            let id = ask_client(&out_tx, &pending, Ask::Elicitation, request).await;
            let response = ctx.cancel.race(guard.recv_async()).await;
            // Cleared while still holding the channel, so a stale id cannot
            // clobber whatever ask comes next.
            pending.lock().unwrap().asks.remove(&id);
            drop(guard);

            Ok(match response {
                Ok(Ok(raw)) => elicitation::format_response(&input, &raw),
                _ => elicitation::DISMISSED.to_owned(),
            })
        })
    })
}

/// Servers the client injects on `session/new` and `session/load`. A transport we
/// cannot speak is dropped like a broken `mcp.toml` entry: losing one server beats
/// losing the session.
fn injected_servers(servers: &[McpServer]) -> Vec<(String, RawTransport)> {
    servers
        .iter()
        .filter_map(|server| match server {
            McpServer::Http(http) => Some((
                server_name(&http.name),
                RawTransport::Http(RawHttpFields {
                    url: http.url.clone(),
                    headers: pairs(&http.headers, |h| (&h.name, &h.value)),
                    oauth: None,
                    ca_file: None,
                }),
            )),
            McpServer::Stdio(stdio) => Some((
                server_name(&stdio.name),
                RawTransport::Stdio(RawStdioFields {
                    command: iter::once(stdio.command.to_string_lossy().into_owned())
                        .chain(stdio.args.iter().cloned())
                        .collect(),
                    environment: pairs(&stdio.env, |e| (&e.name, &e.value)),
                }),
            )),
            _ => {
                warn!("ignoring injected MCP server, only http and stdio are supported");
                None
            }
        })
        .collect()
}

/// Clients name their servers freely, maki names them like `mcp.toml` does.
fn server_name(name: &str) -> String {
    name.chars()
        .take(MAX_SERVER_NAME_LEN)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn pairs<T>(items: &[T], split: impl Fn(&T) -> (&String, &String)) -> HashMap<String, String> {
    items
        .iter()
        .map(|item| {
            let (name, value) = split(item);
            (name.clone(), value.clone())
        })
        .collect()
}

/// MCP is per session: the client picks the cwd and may inject its own servers.
/// Returns as soon as the config is read, the first prompt waits for the tools.
async fn start_mcp(
    cwd: &Path,
    servers: &[McpServer],
    project_config: ProjectConfig,
) -> (Option<McpHandle>, Option<String>) {
    let (handle, errors) =
        mcp::start_with_extra(cwd, project_config, injected_servers(servers)).await;
    if !errors.is_empty() {
        warn!(%errors, "MCP config errors");
    }
    (handle, (!errors.is_empty()).then(|| errors.to_string()))
}

fn trusted_project_config(
    cwd: &Path,
    storage: &StateDir,
    mode: TrustMode,
    policy: &TrustConfig,
) -> ProjectConfig {
    let mut decision = project::resolve(storage, cwd, mode, Runtime::Agent);
    let matched = decision
        .state
        .unanswered()
        .and_then(|question| policy_grant(question, policy));
    if let Some(pattern) = matched {
        // ACP has no card, so policy is the only yes a cwd with no stored
        // decision can get. Recorded like any other yes so `maki trust list`
        // shows what this server trusted on the client's behalf. `unanswered`
        // and not `question`: a recorded `Never` is a stored decision, and
        // granting over it would wipe the rejection out of the store.
        info!(%pattern, cwd = %cwd.display(), "ACP folder trusted by trust.paths policy");
        decision = project::apply_answer(storage, decision, TrustAnswer::Trust);
    }
    // ACP never asks, so the restriction notice is part of what it reports.
    for warning in decision.notices() {
        warn!(%warning, "ACP project configuration trust warning");
    }
    decision.project_config
}

/// Stop the old session before the next one starts, so two generations of the
/// same MCP servers never fight over a port or a lock file.
async fn close_session(srv: &mut Server, id: &SessionId, reason: SessionEndReason) {
    let Some(state) = srv.sessions.remove(id) else {
        return;
    };
    if let Some(cb) = &srv.on_session_end {
        cb(state.handle.session_id.id(), reason).await;
    }
    // Complete the outstanding prompt before draining the cancelled run.
    let prompt = state.pending.lock().unwrap().prompt.take();
    if let Some(id) = prompt {
        let resp = PromptResponse::new(StopReason::Cancelled);
        send(
            &srv.out_tx,
            Response::<_, AcpError>::new(id, Ok(AgentResponse::PromptResponse(resp))),
        )
        .await;
    }
    state.handle.task.abort();
    let _ = state.handle.task.await;
    let _ = state.pump.await;
    if let Some(mcp) = state.mcp {
        mcp.shutdown().await;
    }
}

#[derive(Debug)]
struct Restored {
    /// Handed to the run whole, so the turns that follow write back into the
    /// session this read rather than into a blank one under the same id.
    session: StoredSession,
    /// Only set when the session recorded an absolute cwd.
    cwd: Option<PathBuf>,
}

/// History plus the absolute cwd the session recorded in its header. Tool
/// inputs from a past run resolve against that cwd, not the client's current
/// one; a non-absolute recording falls back to the caller's cwd.
///
/// Reads from the server's state dir, the one the run writes back to, rather
/// than resolving a second answer to where sessions live.
///
/// Claims before it reads, since the turns that follow replace this transcript.
fn claim_history(
    storage: &StateDir,
    session_id: MakiId,
) -> Result<(Restored, SessionClaim), AcpError> {
    let (session, claim) = StoredSession::claim_and_load(session_id, storage)
        .map_err(|e| load_error(e, session_id))?;
    Ok((Restored::from(session), claim))
}

/// [`claim_history`] for the session this server already holds.
fn reload_history(storage: &StateDir, claim: &SessionClaim) -> Result<Restored, AcpError> {
    StoredSession::load_claimed(claim, storage)
        .map(Restored::from)
        .map_err(|e| load_error(e, claim.id()))
}

/// Only a session with no file is not found: a client may answer that with
/// a new session, and one that is there but unreadable must not be replaced
/// on the quiet, so it fails with its read error.
fn load_error(e: SessionError, session_id: MakiId) -> AcpError {
    if e.is_busy() {
        // `not found` would send the client looking for a session it can see
        // right there in the list.
        return AcpError::new(SESSION_BUSY_CODE, e.to_string());
    }
    let error = match e.is_not_found() {
        true => AcpError::resource_not_found(Some(format!("session/{session_id}"))),
        false => AcpError::internal_error(),
    };
    error.data(json_str(&e))
}

impl From<StoredSession> for Restored {
    fn from(session: StoredSession) -> Self {
        let cwd = Path::new(&session.cwd)
            .is_absolute()
            .then(|| PathBuf::from(&session.cwd));
        Self { cwd, session }
    }
}

async fn handle_prompt(srv: &Server, raw: &Value, id: &RequestId) -> Result<(), AcpError> {
    let req: PromptRequest = parse_params(raw)?;
    let session = running_session(srv, &req.session_id)?;

    let compaction = commands::compaction(&req.prompt);
    let (message, images) = extract_prompt_content(&req.prompt);
    let carried = srv
        .extensions
        .settings
        .then(|| settings::read(req.meta.as_ref()))
        .flatten()
        .transpose()
        .and_then(|wire| wire.map(|wire| Ok((wire.thinking()?, wire))).transpose())
        .map_err(|e| AcpError::invalid_params().data(json_str(&e)))?;
    // One outstanding id per session, checked and set under the same guard:
    // `input_tx` is unbounded, so a second prompt would queue happily and
    // overwrite the first id, leaving that request unanswered forever. A
    // refused prompt changes no settings.
    let (current, intent) = {
        let mut pending = session.pending.lock().unwrap();
        if pending.prompt.is_some() {
            return Err(AcpError::new(-32603, "a prompt is already running"));
        }
        // The prompt's own copy runs it, as a local `AgentInput` does; the
        // session's intent follows once the prompt is in, and every client
        // hears of the change.
        let current = session.settings.load_full();
        if let Some(instructions) = compaction {
            // The workflow the client queued it under, for this compaction only.
            let workflow = carried
                .as_ref()
                .map_or(current.workflow, |(_, wire)| wire.workflow);
            session
                .handle
                .input_tx
                .send(SessionInput::Compact(Compaction {
                    instructions,
                    workflow,
                }))
                .map_err(|_| AcpError::new(-32603, "session ended"))?;
            pending.prompt = Some(id.clone());
            return Ok(());
        }
        let intent = match &carried {
            Some((thinking, wire)) => SessionSettings {
                thinking: Some(*thinking),
                fast: wire.fast,
                workflow: wire.workflow,
                ..(*current).clone()
            },
            None => (*current).clone(),
        };
        let input = AgentInput::from_defaults(
            message,
            session.current_mode.clone(),
            images,
            SessionDefaults {
                thinking: intent.thinking,
                fast: intent.fast,
                workflow: intent.workflow,
            },
            InputSource::Acp,
        );
        session
            .handle
            .input_tx
            .send(SessionInput::Prompt(input))
            .map_err(|_| AcpError::new(-32603, "session ended"))?;
        pending.prompt = Some(id.clone());
        (current, intent)
    };
    if intent != *current {
        session.settings.store(Arc::new(intent));
        session.sync_seed();
        session_update(
            &srv.out_tx,
            &SessionId::from(session.handle.session_id.to_string()),
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(session_options(
                &srv.model_specs,
                session,
            ))),
        )
        .await;
    }
    Ok(())
}

/// Answered by a task of its own, so neither the provider setup nor the
/// stream delays the request loop, which a running turn needs.
fn handle_btw(
    srv: &mut Server,
    raw: &Value,
    id: &RequestId,
    timeouts: Timeouts,
) -> Result<(), AcpError> {
    let req: BtwRequest = parse_params(raw)?;
    if req.question.trim().is_empty() {
        return Err(AcpError::invalid_params().data(Value::String(EMPTY_QUESTION.into())));
    }
    let session = running_session(srv, &req.session_id)?;
    let session_ref = session.handle.session_id.clone();
    let snapshot = live_snapshot(session_ref.id());
    let task = spawn(answer_btw(
        srv.out_tx.clone(),
        id.clone(),
        connect(session.model.clone(), timeouts),
        snapshot,
        req.question,
        session_ref,
    ));
    srv.keep_side_task(task);
    Ok(())
}

/// Answered by a task, as `_maki/btw` is, with the provider it asked named.
fn handle_quota(
    srv: &mut Server,
    raw: &Value,
    id: &RequestId,
    timeouts: Timeouts,
) -> Result<(), AcpError> {
    let req: QuotaRequest = parse_params(raw)?;
    let session = running_session(srv, &req.session_id)?;
    let task = spawn(answer_quota(
        srv.out_tx.clone(),
        id.clone(),
        session.model.provider_display_name().to_owned(),
        connect(session.model.clone(), timeouts),
    ));
    srv.keep_side_task(task);
    Ok(())
}

async fn answer_quota(
    out_tx: Sender<Value>,
    id: RequestId,
    provider: String,
    connect: impl Future<Output = Result<(Arc<dyn Provider>, Model), AgentError>>,
) {
    let fetched = match connect.await {
        Ok((connected, _)) => connected.fetch_usage().await,
        Err(e) => Err(e),
    };
    let state = match fetched {
        Ok(Some(quota)) => QuotaState::Ready(quota),
        Ok(None) => QuotaState::Unsupported {},
        Err(e) => QuotaState::Error(e.user_message()),
    };
    send(
        &out_tx,
        Response::<_, AcpError>::new(id, Ok(QuotaAnswer { provider, state })),
    )
    .await;
}

fn running_session<'a>(srv: &'a Server, named: &SessionId) -> Result<&'a SessionState, AcpError> {
    srv.sessions
        .get(named)
        .ok_or_else(|| AcpError::resource_not_found(Some(named.to_string())))
}

/// A provider for the session's model, connected inside the side task so a
/// slow setup never holds the request loop.
async fn connect(
    mut model: Model,
    timeouts: Timeouts,
) -> Result<(Arc<dyn Provider>, Model), AgentError> {
    let provider = provider::from_model_async(&mut model, timeouts).await?;
    Ok((Arc::from(provider), model))
}

/// Every chunk goes out before the answer, so the client has the whole
/// side answer when its call returns.
async fn answer_btw(
    out_tx: Sender<Value>,
    id: RequestId,
    connect: impl Future<Output = Result<(Arc<dyn Provider>, Model), AgentError>>,
    snapshot: Option<Arc<HistorySnapshot>>,
    question: String,
    session_ref: SessionRef,
) {
    let result = match connect.await {
        Ok((provider, model)) => {
            let (answer, answers) = flume::unbounded();
            let ask = async move {
                side_question(
                    &*provider,
                    &model,
                    snapshot.as_deref(),
                    &question,
                    Some(&session_ref),
                    &answer,
                )
                .await
            };
            let forward = async {
                while let Ok(answer) = answers.recv_async().await {
                    let chunk = BtwChunk {
                        request_id: id.clone(),
                        delta: answer,
                    };
                    send(
                        &out_tx,
                        Notification {
                            method: Arc::from(btw::CHUNK_METHOD),
                            params: Some(chunk),
                        },
                    )
                    .await;
                }
            };
            future::zip(ask, forward).await.0
        }
        Err(e) => Err(e),
    };
    let answer = result
        .map(|()| Value::Object(serde_json::Map::new()))
        .map_err(|e| AcpError::internal_error().data(Value::String(e.to_string())));
    send(&out_tx, Response::<_, AcpError>::new(id, answer)).await;
}

/// A successful call of a plan-mode prompt that wrote its plan file, with
/// the text after a full write, read back from the file it wrote.
fn plan_written(plan_path: Option<&PlanPath>, event: &ToolDoneEvent) -> Option<PlanWritten> {
    let plan = plan_path?.lock().unwrap().clone()?;
    if event.is_error || !event.wrote_to(&plan) {
        return None;
    }
    let text = (&*event.tool == WRITE_TOOL_NAME)
        .then(|| read_bounded(&plan))
        .flatten();
    Some(PlanWritten {
        path: plan.display().to_string(),
        text,
    })
}

/// At most [`PLAN_TEXT_LIMIT`] bytes are read, so a large plan costs the
/// pump nothing past them; such a plan is named without its text.
fn read_bounded(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(PLAN_TEXT_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= PLAN_TEXT_LIMIT)
        .then(|| String::from_utf8(bytes).ok())
        .flatten()
}

/// The plan file as it is now, under the session's own path.
fn read_plan(path: &Path) -> Result<PlanText, AcpError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(AcpError::internal_error().data(Value::String(e.to_string()))),
    };
    Ok(PlanText {
        path: path.display().to_string(),
        revision: text.as_deref().map(plan::revision),
        text,
    })
}

fn session_plan(srv: &Server, named: &SessionId) -> Result<PathBuf, AcpError> {
    running_session(srv, named)?
        .plan_path
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| AcpError::invalid_params().data(Value::String(NO_PLAN.into())))
}

async fn handle_plan_read(srv: &Server, raw: &Value, id: &RequestId) -> Result<(), AcpError> {
    let req: PlanRead = parse_params(raw)?;
    let text = read_plan(&session_plan(srv, &req.session_id)?)?;
    send(
        &srv.out_tx,
        Response::<_, AcpError>::new(id.clone(), Ok(text)),
    )
    .await;
    Ok(())
}

/// The queue lock excludes a run starting between the check and the write.
async fn handle_plan_write(srv: &Server, raw: &Value, id: &RequestId) -> Result<(), AcpError> {
    let req: PlanWrite = parse_params(raw)?;
    let path = session_plan(srv, &req.session_id)?;
    let session = running_session(srv, &req.session_id)?;
    let busy = || AcpError::internal_error().data(Value::String(BUSY.into()));
    if session.pending.lock().unwrap().prompt.is_some() {
        return Err(busy());
    }
    session
        .handle
        .session
        .while_idle(|| {
            if read_plan(&path)?.revision != req.revision {
                return Err(AcpError::internal_error().data(Value::String(CONFLICT.into())));
            }
            maki_storage::atomic_write(&path, req.text.as_bytes())
                .map_err(|e| AcpError::internal_error().data(Value::String(e.to_string())))
        })
        .ok_or_else(busy)??;
    send(
        &srv.out_tx,
        Response::<_, AcpError>::new(id.clone(), Ok(json!({}))),
    )
    .await;
    Ok(())
}

/// The first plan mode of a session names its plan file, and every later
/// one returns to it.
fn keep_plan_path(plan_path: &mut Option<PathBuf>, requested: AgentMode) -> AgentMode {
    match requested {
        AgentMode::Plan(fresh) => AgentMode::Plan(plan_path.get_or_insert(fresh).clone()),
        AgentMode::Build => AgentMode::Build,
    }
}

async fn handle_set_mode(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: SetSessionModeRequest = parse_params(raw)?;
    let mode_str = req.mode_id.0.to_string();
    let session = srv
        .sessions
        .get_mut(&req.session_id)
        .ok_or_else(|| AcpError::resource_not_found(Some(req.session_id.to_string())))?;
    let new_mode = {
        let mut plan_path = session.plan_path.lock().unwrap();
        // The kept plan file first: a run steered into plan mode may have made
        // one where a fresh allocation would fail.
        match (mode_str.as_str(), plan_path.as_ref()) {
            (MODE_PLAN, Some(kept)) => AgentMode::Plan(kept.clone()),
            _ => keep_plan_path(
                &mut plan_path,
                methods::mode_id_to_agent_mode(&mode_str)
                    .ok_or_else(|| AcpError::new(-32602, format!("unknown mode: {mode_str}")))?,
            ),
        }
    };
    session.current_mode = new_mode;
    session.sync_seed();

    let sid = SessionId::from(session.handle.session_id.to_string());
    session_update(
        &srv.out_tx,
        &sid,
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::from(mode_str))),
    )
    .await;
    Ok(AgentResponse::SetSessionModeResponse(
        SetSessionModeResponse::new(),
    ))
}

async fn handle_set_config(
    srv: &mut Server,
    raw: &Value,
    timeouts: Timeouts,
) -> Result<AgentResponse, AcpError> {
    let req: SetSessionConfigOptionRequest = parse_params(raw)?;
    let session = running_session(srv, &req.session_id)?;
    match req.config_id.0.as_ref() {
        methods::MODEL_CONFIG_ID => switch_model(srv, &req.session_id, req.value, timeouts).await?,
        settings::YOLO_CONFIG_ID => {
            let yolo = boolean(req.value)?;
            // At once, so the checks later in a running turn see it.
            session.handle.permissions.set_session_yolo(Some(yolo));
            change_settings(session, |settings| settings.yolo = Some(yolo))?;
        }
        settings::FAST_CONFIG_ID => {
            let fast = boolean(req.value)?;
            change_settings(session, |settings| settings.fast = fast)?;
        }
        settings::WORKFLOW_CONFIG_ID => {
            let workflow = boolean(req.value)?;
            change_settings(session, |settings| settings.workflow = workflow)?;
        }
        settings::THINKING_CONFIG_ID => {
            let SessionConfigOptionValue::ValueId { value } = req.value else {
                return Err(invalid(&"thinking must be a value id"));
            };
            let thinking = StoredThinking::parse_setting(&value.0).map_err(|e| invalid(&e))?;
            change_settings(session, |settings| settings.thinking = Some(thinking))?;
        }
        other => return Err(invalid(&format!("unknown config option: {other}"))),
    }
    let options = session_options(&srv.model_specs, running_session(srv, &req.session_id)?);
    Ok(AgentResponse::SetSessionConfigOptionResponse(
        SetSessionConfigOptionResponse::new(options),
    ))
}

async fn switch_model(
    srv: &mut Server,
    id: &SessionId,
    value: SessionConfigOptionValue,
    timeouts: Timeouts,
) -> Result<(), AcpError> {
    let SessionConfigOptionValue::ValueId { value } = value else {
        return Err(invalid(&"model must be a value id"));
    };
    let spec = value.0.to_string();
    if !srv.model_policy.allows(&spec) {
        return Err(invalid(&"model is not allowed by policy"));
    }
    let model = Model::from_spec(&spec).map_err(|e| invalid(&e))?;
    let mut adjusted = model.clone();
    let provider = provider::from_model_async(&mut adjusted, timeouts)
        .await
        .unwrap_or_else(|_| provider::unconfigured());
    let session = srv
        .sessions
        .get_mut(id)
        .ok_or_else(|| AcpError::resource_not_found(Some(id.to_string())))?;
    session
        .handle
        .model_tx
        .send(model)
        .map_err(|_| AcpError::new(-32603, "session ended"))?;
    if let Some(seed) = maki_agent::AgentSeed::live(session.claim.id()) {
        seed.model_slot.store(Arc::new(maki_agent::ModelSlot {
            model: adjusted.clone(),
            provider: Arc::from(provider),
        }));
    }
    session.model = adjusted;
    Ok(())
}

/// Stores the change and wakes the driver to save it, so a toggle followed by
/// an exit survives a resume even with no prompt in between.
fn change_settings(
    session: &SessionState,
    change: impl FnOnce(&mut SessionSettings),
) -> Result<(), AcpError> {
    let previous = session.settings.load_full();
    let mut next = (*previous).clone();
    change(&mut next);
    session.settings.store(Arc::new(next));
    session.sync_seed();
    match session.handle.settings_saved.try_send(()) {
        // Full only means a save is already due, and it reads the latest.
        Ok(()) | Err(flume::TrySendError::Full(())) => Ok(()),
        // Nothing is left to save it, so it is not accepted.
        Err(flume::TrySendError::Disconnected(())) => {
            session.settings.store(previous);
            session.sync_seed();
            Err(AcpError::new(-32603, "session ended"))
        }
    }
}

fn boolean(value: SessionConfigOptionValue) -> Result<bool, AcpError> {
    match value {
        SessionConfigOptionValue::Boolean { value } => Ok(value),
        _ => Err(invalid(&"expected a boolean")),
    }
}

fn invalid(detail: &impl std::fmt::Display) -> AcpError {
    AcpError::invalid_params().data(json_str(detail))
}

/// The model a session opens on: the startup model as its spec reads now,
/// as a rebuild reads it, so a session opened after discovery landed runs
/// what discovery reported rather than the offline table `maki acp` started
/// from. A spec that no longer resolves keeps the startup model.
fn startup_model(params: &AcpParams) -> Model {
    Model::from_spec(&params.model.spec()).unwrap_or_else(|_| params.model.clone())
}

/// The model as its provider adjusts it when the driver connects, so the
/// settings offered match what the session runs. A provider that cannot be
/// built leaves the model as declared: the driver reports that failure.
async fn resolved(model: &Model, timeouts: Timeouts) -> Model {
    let mut model = model.clone();
    let _ = provider::from_model_async(&mut model, timeouts).await;
    model
}

/// The model option and the session's settings, in every answer that carries
/// options, so a model switch also says which settings exist now.
fn session_options(specs: &[String], session: &SessionState) -> Vec<SessionConfigOption> {
    std::iter::once(methods::model_config_option(&session.model.spec(), specs))
        .chain(config_options::options(
            &session.model,
            &session.settings.load(),
            session.handle.permissions.is_yolo(),
        ))
        .collect()
}

async fn handle_notification(srv: &Server, method: &str, raw: &Value) {
    match method {
        "session/cancel" => {
            if let Some(id) = raw.pointer("/params/sessionId").and_then(Value::as_str)
                && let Some(session) = srv.sessions.get(&SessionId::from(id.to_owned()))
            {
                // Every answer still in flight belongs to the cancelled turn,
                // so forget the ids and let them be dropped on arrival.
                // Background calls can still wait on these tagged answers.
                for work in session.handle.session.queue() {
                    if work.status == QueueStatus::Queued
                        && let Ok(receipt) = session.handle.session.cancel_queued(work.id)
                    {
                        queue_update(&srv.out_tx, &SessionId::from(id.to_owned()), receipt).await;
                    }
                }
                session.handle.cancels.cancel_current();
            }
        }
        _ => debug!(method, "unknown notification"),
    }
}

/// The terminal login ran: rebuild what was connected under the old
/// credentials, as a local login's `RefreshProvider` and `RefreshModels` do.
async fn auth_changed(srv: &mut Server, params: &AcpParams) {
    srv.discovery += 1;
    srv.model_specs = available_model_specs(&srv.model_policy);
    discover_models(
        Arc::clone(&srv.model_policy),
        srv.incoming.clone(),
        srv.discovery,
    );
    rebuild_sessions(srv, params.timeouts).await;
}

/// Asks the driver to rebuild before its next input, and resolves the
/// session model afresh so the options follow what the configuration
/// allows now. Sent even with the same model ids: a listing can change what
/// a model supports.
async fn rebuild_sessions(srv: &mut Server, timeouts: Timeouts) {
    for (id, session) in &mut srv.sessions {
        let _ = session.handle.rebuild_tx.try_send(());
        let fresh =
            Model::from_spec(&session.model.spec()).unwrap_or_else(|_| session.model.clone());
        session.model = resolved(&fresh, timeouts).await;
        session_update(
            &srv.out_tx,
            id,
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(session_options(
                &srv.model_specs,
                session,
            ))),
        )
        .await;
    }
}

/// The local loop rebuilds its model slots when a discovery lands, so the
/// latest one's end rebuilds here too.
async fn discovered(srv: &mut Server, generation: u64, timeouts: Timeouts) {
    if generation == srv.discovery {
        rebuild_sessions(srv, timeouts).await;
    }
}

fn handle_incoming_response(srv: &Server, raw: &Value) {
    let id = raw.get("id").map(request_id).unwrap_or(RequestId::Null);
    let ask = ask_id(&id).and_then(|id| {
        srv.sessions.values().find_map(|session| {
            let ask = session.pending.lock().unwrap().asks.remove(&id)?;
            Some((session, ask))
        })
    });
    let Some((session, ask)) = ask else {
        warn!(%id, "response for an unknown request id");
        return;
    };
    let (answer, answer_tx) = match &ask {
        Ask::Permission {
            request_id,
            answer_tx,
        } => (
            TaggedAnswer::new(request_id.clone(), permission_answer(raw)).encode(),
            answer_tx.as_ref(),
        ),
        // The waiting question tool parses this; an error response decodes to
        // nothing and counts as a dismissal.
        Ask::Elicitation => (
            raw.get("result")
                .cloned()
                .unwrap_or(Value::Null)
                .to_string(),
            None,
        ),
        // An error answer reads as no run.
    };
    let _ = answer_tx.unwrap_or(&session.handle.answer_tx).send(answer);
}

/// Our asks go out with a numeric id, but JSON-RPC ids are `string | number`
/// and a client may echo ours back as a string. Dropping such a response would
/// park the waiting tool forever.
fn ask_id(id: &RequestId) -> Option<i64> {
    match id {
        RequestId::Number(id) => Some(*id),
        RequestId::Str(id) => id.parse().ok(),
        RequestId::Null => None,
    }
}

/// A response we cannot read still has to answer the agent, or the tool waits
/// on a permission that will never come.
fn permission_answer(raw: &Value) -> PermissionAnswer {
    match raw
        .get("result")
        .map(|result| serde_json::from_value::<RequestPermissionResponse>(result.clone()))
    {
        Some(Ok(resp)) => permissions::outcome_to_answer(&resp.outcome),
        _ => PermissionAnswer::Deny,
    }
}

fn extract_prompt_content(blocks: &[ContentBlock]) -> (String, Vec<ImageSource>) {
    let mut text = String::new();
    let mut images = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text(TextContent { text: t, .. }) => append(&mut text, t),
            ContentBlock::Image(ImageContent {
                data, mime_type, ..
            }) => images.push(ImageSource::new(
                image_media_type(mime_type),
                Arc::from(data.as_str()),
            )),
            ContentBlock::Resource(res) => {
                if let EmbeddedResourceResource::TextResourceContents(trc) = &res.resource {
                    append(&mut text, &format!("--- {} ---\n{}", trc.uri, trc.text));
                }
            }
            ContentBlock::ResourceLink(rl) => append(&mut text, &format!("[Resource: {}]", rl.uri)),
            _ => {}
        }
    }

    (text, images)
}

fn append(text: &mut String, part: &str) {
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(part);
}

fn image_media_type(mime: &str) -> ImageMediaType {
    match mime {
        "image/png" => ImageMediaType::Png,
        "image/gif" => ImageMediaType::Gif,
        "image/webp" => ImageMediaType::Webp,
        _ => ImageMediaType::Jpeg,
    }
}

#[allow(clippy::too_many_arguments)]
fn start_event_pump(
    mut events: SessionEvents,
    answer_tx: Sender<String>,
    session_id: SessionRef,
    out_tx: Sender<Value>,
    pending: PendingState,
    cwd: PathBuf,
    home: Option<PathBuf>,
    project_trusted: bool,
    mut usage: PumpUsage,
    mut compactions: Compactions,
    tool_data: bool,
    plan_path: Option<PlanPath>,
    tasks: Option<SessionControl>,
    plugins: Option<Arc<dyn SessionPlugins>>,
) -> Task<()> {
    spawn(async move {
        usage.background = tasks.clone();
        // What each running call's start told a tool data client, so its
        // completion sends only what changed.
        let sid = SessionId::from(session_id.to_string());
        // A permission request only carries scopes, which are matching keys and
        // not always paths, so the file context a client needs has to come from
        // the tool call that asked for permission. Kept for the turn, like
        // sdk_mode does: the history holds these inputs anyway.
        let mut tool_inputs: HashMap<String, Value> = HashMap::new();

        loop {
            let Some(envelope) = events.next().await else {
                break;
            };
            if let Some(plugins) = &plugins {
                plugins.observe(session_id.id(), &envelope);
            }
            if let Some(tasks) = &tasks
                && (matches!(&envelope.event, AgentEvent::ToolDone(done) if done.backgrounded)
                    || matches!(&envelope.event, AgentEvent::TaskEnded { .. }))
            {
                tasks.record_task(&envelope, None);
            }
            let auth_event = match &envelope.event {
                AgentEvent::PluginTask { envelope, .. } => envelope.as_ref(),
                _ => &envelope,
            };
            let auth_id = auth_event
                .subagent
                .as_ref()
                .map(|info| info.parent_tool_use_id.clone());
            if matches!(auth_event.event, AgentEvent::AuthRequired) {
                let answer = match &auth_event.subagent {
                    Some(info) => info.answer_tx.clone(),
                    None => Some(answer_tx.clone()),
                };
                if let Some(answer) = answer {
                    pending.lock().unwrap().auth.insert(auth_id, answer);
                }
            } else if matches!(
                auth_event.event,
                AgentEvent::Done { .. } | AgentEvent::Error { .. } | AgentEvent::StreamClosed
            ) {
                pending.lock().unwrap().auth.remove(&auth_id);
            }
            let mut envelope = envelope;
            let mut task_plugin = None;
            if let AgentEvent::PluginTask {
                plugin,
                envelope: inner,
            } = envelope.event
            {
                let Some(tasks) = &tasks else { continue };
                if !tasks.record_task(&inner, Some(Arc::clone(&plugin))) {
                    continue;
                }
                if let AgentEvent::TurnComplete(turn) = &inner.event
                    && let Some(update) = usage.turn(turn, false)
                {
                    session_update(&out_tx, &sid, update).await;
                }
                if matches!(inner.event, AgentEvent::PermissionRequest { .. }) {
                    task_plugin = Some(plugin);
                    envelope = *inner;
                } else {
                    send(
                        &out_tx,
                        Notification {
                            method: session_ops::TASK_UPDATE_METHOD.into(),
                            params: Some(session_ops::TaskUpdate {
                                session_id: sid.clone(),
                                envelope: Envelope {
                                    event: AgentEvent::PluginTask {
                                        plugin,
                                        envelope: inner,
                                    },
                                    subagent: None,
                                    run_id: 0,
                                },
                                inbox: None,
                            }),
                        },
                    )
                    .await;
                    if let Some(update) = usage.background_update() {
                        session_update(&out_tx, &sid, update).await;
                    }
                    continue;
                }
            }
            if let Some(tasks) = &tasks
                && task_plugin.is_none()
                && (envelope.subagent.is_some()
                    || matches!(envelope.event, AgentEvent::SubagentHistory { .. }))
            {
                if !tasks.record_task(&envelope, None) {
                    continue;
                }
                if !matches!(envelope.event, AgentEvent::PermissionRequest { .. }) {
                    if let AgentEvent::TurnComplete(turn) = &envelope.event
                        && let Some(update) = usage.turn(turn, false)
                    {
                        session_update(&out_tx, &sid, update).await;
                    }
                    let inbox = envelope
                        .subagent
                        .as_ref()
                        .and_then(|info| info.inbox.as_ref())
                        .map(|inbox| inbox.snapshot());
                    send(
                        &out_tx,
                        Notification {
                            method: session_ops::TASK_UPDATE_METHOD.into(),
                            params: Some(session_ops::TaskUpdate {
                                session_id: sid.clone(),
                                envelope,
                                inbox,
                            }),
                        },
                    )
                    .await;
                    if let Some(update) = usage.background_update() {
                        session_update(&out_tx, &sid, update).await;
                    }
                    continue;
                }
            }
            let Envelope {
                event,
                subagent,
                run_id,
            } = envelope;
            // A subagent's turn spends session money even though its events
            // stay out of the transcript.
            if let AgentEvent::TurnComplete(turn) = &event {
                if let Some(update) = usage.turn(turn, subagent.is_none()) {
                    session_update(&out_tx, &sid, update).await;
                }
                continue;
            }

            let update = match event {
                event @ (AgentEvent::AuthRequired | AgentEvent::TaskEnded { .. }) => {
                    send(
                        &out_tx,
                        Notification {
                            method: session_ops::TASK_UPDATE_METHOD.into(),
                            params: Some(session_ops::TaskUpdate {
                                session_id: sid.clone(),
                                envelope: Envelope {
                                    event,
                                    subagent,
                                    run_id,
                                },
                                inbox: None,
                            }),
                        },
                    )
                    .await;
                    continue;
                }
                AgentEvent::ContextReset {
                    context_size,
                    context_window,
                } => usage.compaction_done(context_size, context_window),
                AgentEvent::QueueUpdated(receipt) => {
                    queue_update(&out_tx, &sid, receipt).await;
                    continue;
                }
                AgentEvent::PermissionRequest {
                    id,
                    tool,
                    scopes,
                    reason,
                    ..
                } => {
                    let tool = tool.to_string();
                    let scope = format!("{tool}: {}", scopes.join(", "));
                    // A plugin escalated this call, and its reason is what the
                    // user most needs to read.
                    let scope = match reason {
                        Some(reason) => format!("{reason} ({scope})"),
                        None => scope,
                    };
                    // The cache is keyed by the call that asked for permission,
                    // so look it up before the remap below. A subagent is left
                    // out on purpose: nothing makes a child's ids unique
                    // against the parent's, and naming the wrong file in a
                    // security prompt is worse than naming none.
                    let raw_input = subagent.is_none().then(|| tool_inputs.get(&id)).flatten();
                    // A child's own tool call never reached the client, nor
                    // this cache, so its permission rides on the `task` call
                    // the client can see, only the child's channel may take the
                    // answer, and the title is all the dialog gets.
                    let task = tasks
                        .as_ref()
                        .and(subagent.as_ref())
                        .and_then(|info| serde_json::to_value(info).ok());
                    let request_id = id.clone();
                    let (id, title, answer_tx) = match subagent {
                        Some(info) => {
                            let Some(answer_tx) = info.answer_tx else {
                                warn!(
                                    subagent = info.name,
                                    parent_tool_use_id = info.parent_tool_use_id,
                                    tool_use_id = id,
                                    scope,
                                    "dropping subagent permission with no answer channel"
                                );
                                continue;
                            };
                            let title = format!("{}: {scope}", info.name);
                            (info.parent_tool_use_id, title, Some(answer_tx))
                        }
                        None => (id, scope, None),
                    };
                    let mut request = RequestPermissionRequest::new(
                        sid.clone(),
                        translate::permission_update(
                            id,
                            title,
                            &tool,
                            raw_input,
                            &cwd,
                            home.as_deref(),
                        ),
                        permissions::permission_options(project_trusted),
                    );
                    if let Some(plugin) = task_plugin {
                        request
                            .meta
                            .get_or_insert_with(Default::default)
                            .insert(session_ops::PLUGIN_TASK_KEY.into(), json!(plugin));
                    }
                    if let Some(task) = task {
                        request
                            .meta
                            .get_or_insert_with(Default::default)
                            .insert(session_ops::TASK_PERMISSION_KEY.into(), task);
                    }
                    let request = AgentRequest::RequestPermissionRequest(request);
                    ask_client(
                        &out_tx,
                        &pending,
                        Ask::Permission {
                            request_id,
                            answer_tx,
                        },
                        request,
                    )
                    .await;
                    continue;
                }
                _ if subagent.is_some() => continue,
                AgentEvent::TextDelta { text } => translate::text_delta(&text),
                AgentEvent::ThinkingDelta { text } => translate::thinking_delta(&text),
                AgentEvent::ToolPending { id, name } => translate::tool_pending(&id, &name),
                AgentEvent::ToolStart(event) => {
                    let mut update = translate::tool_start(&event, &cwd, home.as_deref());
                    if tool_data {
                        update = tool_data::attach(
                            Some(update),
                            &event.id,
                            &AgentEvent::ToolStart(event.clone()),
                        );
                    }
                    if let Some(raw_input) = event.raw_input {
                        tool_inputs.insert(event.id, raw_input);
                    }
                    update
                }
                AgentEvent::ToolOutput { id, content } => translate::tool_output(&id, &content),
                AgentEvent::ToolDone(event) => {
                    let update = translate::tool_done(&event, &cwd, home.as_deref());
                    let update = if tool_data {
                        tool_data::attach(
                            Some(update),
                            &event.id,
                            &AgentEvent::ToolDone(event.clone()),
                        )
                    } else {
                        update
                    };
                    match plan_written(plan_path.as_ref(), &event) {
                        Some(written) => plan::attach(update, &written),
                        None => update,
                    }
                }
                AgentEvent::ToolMovable { ref id, .. } | AgentEvent::ToolParts { ref id, .. }
                    if tool_data =>
                {
                    tool_data::attach(None, &id.clone(), &event)
                }
                AgentEvent::ToolProgress(event) if tool_data => {
                    let id = event.id.clone();
                    tool_data::attach(None, &id, &AgentEvent::ToolProgress(event))
                }
                AgentEvent::ToolResultsSubmitted { .. } => match usage.results_submitted() {
                    Some(update) => update,
                    None => continue,
                },
                AgentEvent::AutoCompacting {
                    context_size,
                    context_window,
                    manual,
                } => {
                    usage.compaction_started(context_size, context_window);
                    match compactions.start(manual) {
                        Some(update) => update,
                        None => continue,
                    }
                }
                AgentEvent::CompactionDone {
                    context_size_after,
                    context_window,
                    summary,
                    ..
                } => {
                    // The gauge first, so a client draws the end at the size
                    // after.
                    let gauge = usage.compaction_done(context_size_after, context_window);
                    session_update(&out_tx, &sid, gauge).await;
                    let Some(update) = compactions.end(CompactionStatus::Completed) else {
                        continue;
                    };
                    let summary = vec![ContentBlock::Text(TextContent::new(summary))];
                    SessionUpdate::CompactionUpdate(update.summary(summary))
                }
                AgentEvent::ObservationLine { text, detail, end } => {
                    translate::notice_detail(&MakiId::generate().to_string(), &text, detail, end)
                }
                AgentEvent::Done { reason, cost, .. } => {
                    usage.settle(cost);
                    let status = match reason {
                        DoneReason::Cancelled => CompactionStatus::Cancelled,
                        _ => CompactionStatus::Other(COMPACTION_SKIPPED.to_owned()),
                    };
                    if let Some(update) = compactions.end(status) {
                        session_update(&out_tx, &sid, SessionUpdate::CompactionUpdate(update))
                            .await;
                    }
                    usage.compacting = false;
                    tool_inputs.clear();
                    if let Some(update) = usage.background_update() {
                        session_update(&out_tx, &sid, update).await;
                    }
                    if let Some(id) = finish_turn(&pending) {
                        let resp = PromptResponse::new(translate::map_done_reason(reason));
                        send(
                            &out_tx,
                            Response::<_, AcpError>::new(
                                id,
                                Ok(AgentResponse::PromptResponse(resp)),
                            ),
                        )
                        .await;
                    }
                    continue;
                }
                AgentEvent::Error {
                    message,
                    auth,
                    totals,
                } => {
                    // A turn that dies on a provider 500 never reaches `Done`,
                    // and the pump outlives the session, so without this the
                    // whole file a `write` was carrying stays pinned forever.
                    if let Some(update) = compactions.end(CompactionStatus::Failed) {
                        let update = SessionUpdate::CompactionUpdate(update.error(message.clone()));
                        session_update(&out_tx, &sid, update).await;
                    }
                    usage.compacting = false;
                    tool_inputs.clear();
                    usage.settle(totals.cost);
                    if let Some(update) = usage.background_update() {
                        session_update(&out_tx, &sid, update).await;
                    }
                    if let Some(id) = finish_turn(&pending) {
                        let error = if auth {
                            AcpError::auth_required()
                        } else {
                            AcpError::internal_error()
                        };
                        let error = error.data(Value::String(message));
                        send(&out_tx, Response::<AgentResponse>::new(id, Err(error))).await;
                    }
                    continue;
                }
                _ => continue,
            };
            session_update(&out_tx, &sid, update).await;
        }
    })
}

/// What a session has spent, and the one place its usage updates are built.
struct PumpUsage {
    background: Option<SessionControl>,
    cost: CostTally,
    /// The usage extension was agreed, so each update also carries
    /// `_meta.maki.usage`.
    extension: bool,
    /// The ledger extension was agreed too, so that metadata carries the
    /// ledger.
    shares_ledger: bool,
    /// Seeded from the stored session on a load.
    ledger: SessionLedger,
    subsidised_by: Option<Arc<str>>,
    /// The last main-agent turn's gauge. A child or compaction turn reports
    /// against another model's window, so it never replaces this one.
    main_gauge: Option<(u32, u32)>,
    /// Between `AutoCompacting` and the end of the compaction. Every main
    /// `Done` and `Error` ends it, because a failed compaction sends no
    /// `CompactionDone`.
    compacting: bool,
}

impl PumpUsage {
    fn new(extension: bool, ledger: SessionLedger, subsidised_by: Option<Arc<str>>) -> Self {
        Self {
            background: None,
            cost: CostTally::starting_at(ledger.cost),
            extension,
            shares_ledger: false,
            ledger,
            subsidised_by,
            main_gauge: None,
            compacting: false,
        }
    }

    /// The ledger rides on the usage metadata, so it needs that too.
    fn sharing_ledger(mut self, agreed: bool) -> Self {
        self.shares_ledger = agreed && self.extension;
        self
    }

    fn update(&self, used: u32, window: u32, turn: Option<WireTurn>) -> SessionUpdate {
        self.build(used, window, turn, false)
    }

    /// Only the extension carries the boundary, so without it nothing goes.
    fn results_submitted(&self) -> Option<SessionUpdate> {
        let (used, window) = self.main_gauge.filter(|_| self.extension)?;
        Some(self.build(used, window, None, true))
    }

    fn build(
        &self,
        used: u32,
        window: u32,
        turn: Option<WireTurn>,
        results_submitted: bool,
    ) -> SessionUpdate {
        let update = translate::context_usage(used, window, self.ledger.cost);
        if !self.extension {
            return update;
        }
        usage::attach(
            update,
            &WireUsage {
                list_cost: self.ledger.list_cost,
                subsidised_by: self.subsidised_by.as_deref().map(str::to_owned),
                turn,
                results_submitted,
                background_tasks: self
                    .background
                    .as_ref()
                    .map(SessionControl::background_tasks),
                ledger: self.shares_ledger.then(|| self.ledger.clone()),
            },
        )
    }

    fn background_update(&self) -> Option<SessionUpdate> {
        self.background.as_ref()?;
        let (used, window) = self.main_gauge?;
        Some(self.update(used, window, None))
    }

    fn settle(&mut self, ledger: Option<f64>) {
        self.cost.settle(ledger);
        self.ledger.cost = self.cost.total();
    }

    /// Every turn spends; only a main one moves the gauge and gets its own
    /// line. `None` until a main gauge exists to report the totals against.
    fn turn(&mut self, turn: &TurnCompleteEvent, main: bool) -> Option<SessionUpdate> {
        self.ledger.bill(turn);
        self.cost.turn(turn.cost);
        self.ledger.cost = self.cost.total();
        let main = main && !self.compacting;
        if main {
            self.subsidised_by = turn.subsidised_by.clone();
            self.main_gauge = Some(translate::turn_gauge(turn));
        }
        let (used, window) = self.main_gauge?;
        Some(self.update(used, window, main.then(|| WireTurn::from(turn))))
    }

    /// A new main gauge with the totals as they stand.
    fn gauge(&mut self, used: u32, window: u32) -> SessionUpdate {
        self.main_gauge = Some((used, window));
        self.update(used, window, None)
    }

    /// Both compaction events carry the main model's window, which a model
    /// switch right before the compaction may have changed.
    fn compaction_started(&mut self, used: u32, window: u32) {
        self.compacting = true;
        self.main_gauge = Some((used, window));
    }

    fn compaction_done(&mut self, after: u32, window: u32) -> SessionUpdate {
        self.compacting = false;
        self.gauge(after, window)
    }
}

/// The compaction under way, for a client that advertised
/// `sessionCapabilities.compaction`. A main `Done` or `Error` before
/// `CompactionDone` ends it, as for [`PumpUsage::compacting`].
struct Compactions {
    reported: bool,
    current: Option<CompactionId>,
}

impl Compactions {
    fn new(reported: bool) -> Self {
        Self {
            reported,
            current: None,
        }
    }

    /// The id is fresh, so it stays unique within the session across loads.
    fn start(&mut self, manual: bool) -> Option<SessionUpdate> {
        if !self.reported {
            return None;
        }
        let id = CompactionId::new(MakiId::generate().to_string());
        self.current = Some(id.clone());
        let update = CompactionUpdate::new(id, CompactionStatus::InProgress);
        Some(SessionUpdate::CompactionUpdate(match manual {
            true => update.meta(commands::manual_meta()),
            false => update,
        }))
    }

    fn end(&mut self, status: CompactionStatus) -> Option<CompactionUpdate> {
        Some(CompactionUpdate::new(self.current.take()?, status))
    }
}

/// Background calls can still wait on tagged answers after the prompt ends.
fn finish_turn(pending: &PendingState) -> Option<RequestId> {
    pending.lock().unwrap().prompt.take()
}

async fn send(out_tx: &Sender<Value>, msg: impl Serialize) {
    if let Ok(json) = serde_json::to_value(JsonRpcMessage::wrap(msg)) {
        let _ = out_tx.send_async(json).await;
    }
}

async fn session_update(out_tx: &Sender<Value>, sid: &SessionId, update: SessionUpdate) {
    let notification =
        AgentNotification::SessionNotification(SessionNotification::new(sid.clone(), update));
    send(
        out_tx,
        Notification {
            method: Arc::from("session/update"),
            params: Some(notification),
        },
    )
    .await;
}

fn parse_params<T: serde::de::DeserializeOwned>(raw: &Value) -> Result<T, AcpError> {
    serde_json::from_value(raw.get("params").cloned().unwrap_or(Value::Null))
        .map_err(|e| AcpError::invalid_params().data(json_str(&e)))
}

fn json_str(e: &impl std::fmt::Display) -> Value {
    Value::String(e.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use maki_agent::headless::SessionEnd;
    use maki_agent::permissions::{PermissionCheck, PermissionError, PermissionManager};
    use maki_agent::tools::PermissionScopes;
    use maki_agent::{
        CancelToken, DEFAULT_DATA_VERSION, DoneReason, EventSender, RunTotals, SubagentInfo,
        ToolProgressEvent, ToolStartEvent, TurnCompleteEvent,
    };
    use maki_config::project::TrustQuestion;
    use maki_config::{AgentConfig, Effect, ToolKey, TrustFileConfig};
    use maki_providers::provider::BoxFuture;
    use maki_providers::{
        ContentBlock as MsgBlock, Message, ModelInfo, ProviderEvent, ProviderUsage, RequestOptions,
        Role, StreamResponse, ThinkingConfig, Timeouts, TokenUsage,
    };
    use maki_storage::StateDir;
    use maki_storage::sessions::{Session, StoredSubagent, StoredTokenUsage};
    use maki_storage::trusted_folders::{CanonicalFolder, TrustStatus, TrustedFolders};

    use tempfile::TempDir;
    use test_case::test_case;
    use tokio::time::timeout;

    use super::*;
    use maki_agent::session::SessionTrack;
    use maki_types::cancel::RunCancels;
    use maki_types::event::{PartStatus, ToolPart};
    use maki_types::snapshot::BufferSnapshot;
    use maki_types::tool::{ChildStatus, ToolChild, ToolData, ToolOutput};
    use maki_types::transcript::SessionExport;
    use maki_wire::display::DisplayMapper;
    use maki_wire::extensions::META_KEY;

    const ANSWERED_ID: i64 = 1001;
    const UNKNOWN_ID: i64 = 1002;
    const DISCOVERED_SPEC: &str = "openrouter/discovered-model";
    const OFFLINE_SPEC: &str = "openai/gpt-5";
    const SELECTED_SPEC: &str = "openai/gpt-5.6-sol";
    const FAILED_LOAD_KEEPS_SESSION: &str = "a load that failed must not close the running session";
    /// Neither resolves in the price tables, so nothing can re-price a restored
    /// session back onto the recorded number by luck.
    const RETIRED_SPEC: &str = "retired-vendor/retired-model-9000";
    const RETIRED_MODEL_ID: &str = "retired-model-9000";
    const RECORDED_COST: f64 = 1.25;
    /// Generous on purpose: the work under test is a few file reads, so any
    /// wait near this long is the deadlock and not a slow machine.
    const STDIN_DEADLOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    const DENY_SCOPE: &str = "acp-session-trust-boundary-test-deny";
    const ALLOW_SCOPE: &str = "acp-session-trust-boundary-test-allow";
    const POLICY_MATCH_GLOB: &str = "**";
    const POLICY_MISS_GLOB: &str = "/nowhere/*";
    const GATED_INIT_SOURCE: &str = "return {}";
    /// The tool a later turn asks about. Nothing allows `bash` by default, so
    /// an `Ok` here can only come from an answer that was applied.
    const NEXT_TURN_TOOL: &str = "bash";
    const NEXT_TURN_SCOPE: &str = "rm -rf /";
    const NEXT_TURN_TOOL_USE_ID: &str = "toolu_next_turn";
    const CLOSE_WAITED: &str = "closing a session must not wait for its background work";
    const STALE_ALLOW: &str = "a stale answer must never allow the next turn's tool";
    const STORED_CALL_ID: &str = "toolu_stored";
    const STORED_SUMMARY: &str = "ran the stored build";

    /// Where fixture servers claim their placeholder session. One dir is enough,
    /// since fresh ids never collide and nothing is written under them.
    static FIXTURE_CLAIMS: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().unwrap());

    /// The client picks the session cwd, so that folder's stored trust decides
    /// whether its `.maki` may widen permissions. Its deny rules need no trust:
    /// a repository can only narrow what the agent may do.
    #[test]
    fn session_project_config_follows_stored_folder_trust() {
        let state = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let maki_dir = project.path().join(".maki");
        std::fs::create_dir(project.path().join(".git")).unwrap();
        std::fs::create_dir(&maki_dir).unwrap();
        std::fs::write(
            maki_dir.join("permissions.toml"),
            format!("[bash]\ndeny = [\"{DENY_SCOPE}\"]\nallow = [\"{ALLOW_SCOPE}\"]\n"),
        )
        .unwrap();
        let storage = StateDir::from_path(state.path().to_path_buf());

        let untrusted = trusted_project_config(
            project.path(),
            &storage,
            TrustMode::Consult,
            &TrustConfig::default(),
        );
        assert!(!untrusted.is_trusted());
        let rules = maki_config::load_permissions(&untrusted).rules;
        assert!(
            rules
                .iter()
                .any(|rule| bash_rule(rule, DENY_SCOPE, Effect::Deny))
        );
        assert!(
            !rules
                .iter()
                .any(|rule| rule.scope.as_deref() == Some(ALLOW_SCOPE))
        );

        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        project::grant(&storage, &TrustQuestion::for_folder(&folder)).unwrap();

        let trusted = trusted_project_config(
            project.path(),
            &storage,
            TrustMode::Consult,
            &TrustConfig::default(),
        );
        assert!(trusted.is_trusted());
        assert_eq!(
            trusted.config_root(),
            ProjectConfig::for_project(project.path()).config_root()
        );
        let rules = maki_config::load_permissions(&trusted).rules;
        assert!(
            rules
                .iter()
                .any(|rule| bash_rule(rule, DENY_SCOPE, Effect::Deny))
        );
        assert!(
            rules
                .iter()
                .any(|rule| bash_rule(rule, ALLOW_SCOPE, Effect::Allow))
        );
    }

    /// ACP resolves folder trust from its dispatch loop, on the same executor
    /// that another thread is blocking on a whole stdin read. Reaching for
    /// stdin on a path that can never ask a question parked the loop until the
    /// client sent bytes it was only going to send after our answer, so the
    /// first session hung forever.
    #[test]
    fn resolving_trust_without_a_prompt_does_not_wait_for_stdin() {
        let state = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        std::fs::create_dir(project.path().join(".git")).unwrap();
        std::fs::create_dir(project.path().join(".maki")).unwrap();
        std::fs::write(project.path().join(".maki/init.lua"), "return {}").unwrap();
        let storage = StateDir::from_path(state.path().to_path_buf());
        let cwd = project.path().to_path_buf();

        let held = std::io::stdin().lock();
        let (done_tx, done_rx) = flume::bounded(1);
        let worker = std::thread::spawn(move || {
            let config =
                trusted_project_config(&cwd, &storage, TrustMode::Consult, &TrustConfig::default());
            let _ = done_tx.send(config.is_trusted());
        });

        let finished = done_rx.recv_timeout(STDIN_DEADLOCK_TIMEOUT);
        drop(held);
        worker.join().unwrap();

        assert_eq!(
            finished,
            Ok(false),
            "a non-interactive trust resolution must not touch stdin"
        );
    }

    /// A session cwd shipping one gated file, so a start there has a real
    /// question for the policy to answer.
    fn gated_project(project: &Path) {
        std::fs::create_dir(project.join(".git")).unwrap();
        std::fs::create_dir(project.join(".maki")).unwrap();
        std::fs::write(project.join(".maki/init.lua"), GATED_INIT_SOURCE).unwrap();
    }

    fn trust_policy(pattern: &str) -> TrustConfig {
        TrustConfig::from_file(TrustFileConfig {
            paths: Some(vec![pattern.to_owned()]),
            prompt: Some(false),
        })
        .expect("valid trust pattern")
    }

    /// The container image's global `init.lua` is the only thing that can
    /// answer for an ACP run, and its answer has to reach the store: the next
    /// start consults that alone, and `maki trust list` has to show it.
    #[test_case(POLICY_MATCH_GLOB, true ; "a_matching_pattern_grants_without_asking")]
    #[test_case(POLICY_MISS_GLOB, false ; "a_non_matching_pattern_leaves_the_folder_untrusted")]
    fn policy_answers_a_session_cwd(pattern: &str, expected_trusted: bool) {
        let state = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        gated_project(project.path());
        let storage = StateDir::from_path(state.path().to_path_buf());

        let config = trusted_project_config(
            project.path(),
            &storage,
            TrustMode::Consult,
            &trust_policy(pattern),
        );

        assert_eq!(config.is_trusted(), expected_trusted);
        let recorded =
            project::resolve(&storage, project.path(), TrustMode::Consult, Runtime::Agent);
        assert_eq!(recorded.project_config.is_trusted(), expected_trusted);
    }

    /// A recorded `Never` is an answer, and `grant` replaces a rejection in the
    /// store. A policy that overturned one would delete the decision the user
    /// went out of their way to give, on every session the client opens.
    #[test]
    fn policy_does_not_overturn_a_recorded_rejection() {
        let state = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        gated_project(project.path());
        let storage = StateDir::from_path(state.path().to_path_buf());
        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        project::deny(&storage, &TrustQuestion::for_folder(&folder)).unwrap();

        let config = trusted_project_config(
            project.path(),
            &storage,
            TrustMode::Consult,
            &trust_policy(POLICY_MATCH_GLOB),
        );

        assert!(!config.is_trusted());
        assert_eq!(
            TrustedFolders::new(&storage).status(&folder).unwrap(),
            TrustStatus::Rejected,
            "the rejection must survive a matching policy"
        );
    }

    fn bash_rule(rule: &maki_config::PermissionRule, scope: &str, effect: Effect) -> bool {
        rule.tool == ToolKey::native("bash")
            && rule.scope.as_deref() == Some(scope)
            && rule.effect == effect
    }

    fn allow_once(id: i64) -> Value {
        serde_json::json!({
            "id": id,
            "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } },
        })
    }

    #[test_case(allow_once(ANSWERED_ID), PermissionAnswer::AllowOnce ; "selected_option")]
    #[test_case(serde_json::json!({ "id": ANSWERED_ID, "result": { "outcome": { "outcome": "cancelled" } } }), PermissionAnswer::Deny ; "cancelled_outcome")]
    #[test_case(serde_json::json!({ "id": ANSWERED_ID, "result": { "nonsense": true } }), PermissionAnswer::Deny ; "unparsable_result")]
    #[test_case(serde_json::json!({ "id": ANSWERED_ID, "error": { "code": -32603 } }), PermissionAnswer::Deny ; "jsonrpc_error")]
    fn permission_answer_maps_response(raw: Value, expected: PermissionAnswer) {
        assert_eq!(permission_answer(&raw), expected);
    }

    fn test_server() -> (Server, flume::Receiver<String>, flume::Receiver<Value>) {
        let (answer_tx, answer_rx) = flume::unbounded();
        let (out_tx, out_rx) = flume::unbounded();
        let claim = SessionClaim::fresh(&StateDir::from_path(FIXTURE_CLAIMS.path().to_path_buf()));
        let session = SessionTrack::open(
            Resumed::empty(SessionRef::from(claim.id())),
            claim.clone(),
            StateDir::from_path(FIXTURE_CLAIMS.path().to_path_buf()),
            "/project",
        )
        .control();
        let handle = InteractiveHandle {
            tool_names: Vec::new(),
            session,
            input_tx: flume::unbounded().0,
            answer_tx,
            cancels: RunCancels::new(),
            rewind_tx: flume::unbounded().0,
            model_tx: flume::unbounded().0,
            mode_tx: flume::unbounded().0,
            settings_saved: flume::bounded(1).0,
            rebuild_tx: flume::bounded(1).0,
            session_id: SessionRef::from(claim.id()),
            permissions: Arc::new(PermissionManager::new(
                maki_config::PermissionsConfig::default(),
                PathBuf::from("/project"),
                ProjectConfig::for_project(Path::new("/project")),
                Arc::default(),
            )),
            task: spawn(async { SessionEnd::default() }),
            spend: Arc::default(),
        };
        let id = SessionId::from(handle.session_id.to_string());
        let server = Server {
            out_tx,
            model_specs: Vec::new(),
            model_policy: Arc::new(ModelPolicy::default()),
            client_elicits_form: false,
            client_compaction: false,
            extensions: Agreed::default(),
            on_session_end: None,
            side_tasks: Vec::new(),
            discovery: 0,
            incoming: flume::unbounded().0.downgrade(),
            terminal_login: false,
            sessions: HashMap::from([(
                id,
                SessionState {
                    mcp_config_error: None,
                    handle,
                    pump: spawn(async {}),
                    mcp: None,
                    current_mode: AgentMode::Build,
                    plan_path: PlanPath::default(),
                    model: Model::from_spec(SELECTED_SPEC).expect("a shipped model"),
                    settings: SessionSettings::default().shared(),
                    pending: Arc::default(),
                    claim,
                },
            )]),
        };
        (server, answer_rx, out_rx)
    }

    #[test]
    fn a_full_output_queue_waits_without_losing_reply_order() {
        block_on(async {
            let (tx, rx) = flume::bounded(1);
            send(
                &tx,
                Response::<_, AcpError>::new(RequestId::Number(ANSWERED_ID), Ok(json!({}))),
            )
            .await;
            let second = send(
                &tx,
                Response::<_, AcpError>::new(RequestId::Number(UNKNOWN_ID), Ok(json!({}))),
            );
            let mut second = Box::pin(second);
            assert!(future::poll_once(&mut second).await.is_none());
            assert_eq!(rx.recv().unwrap()["id"], ANSWERED_ID);
            second.await;
            assert_eq!(rx.recv().unwrap()["id"], UNKNOWN_ID);
        });
    }

    #[test]
    fn prompts_permissions_and_cancellation_stay_with_the_named_session() {
        let (mut srv, first_answers, _first_out) = test_server();
        let (other, second_answers, _second_out) = test_server();
        let (first_tx, first_input) = flume::unbounded();
        let (second_tx, second_input) = flume::unbounded();
        let first = srv.sessions.keys().next().unwrap().clone();
        let second = other.sessions.keys().next().unwrap().clone();
        srv.sessions.extend(other.sessions);
        srv.sessions.get_mut(&first).unwrap().handle.input_tx = first_tx;
        srv.sessions.get_mut(&second).unwrap().handle.input_tx = second_tx;
        let first_run = srv.sessions[&first].handle.cancels.start(0);
        let second_run = srv.sessions[&second].handle.cancels.start(0);
        for (session, request, text) in [
            (&first, ANSWERED_ID, PROMPT_TEXT),
            (&second, UNKNOWN_ID, QUEUED_TEXT),
        ] {
            let prompt = json!({ "params": { "sessionId": session, "prompt": [{ "type": "text", "text": text }] } });
            block_on(handle_prompt(&srv, &prompt, &RequestId::Number(request))).unwrap();
            srv.sessions[session]
                .pending
                .lock()
                .unwrap()
                .asks
                .insert(request, permission_ask(None));
        }
        assert_eq!(sent_prompt(&first_input).message, PROMPT_TEXT);
        assert_eq!(sent_prompt(&second_input).message, QUEUED_TEXT);
        block_on(handle_notification(
            &srv,
            "session/cancel",
            &json!({ "params": { "sessionId": first } }),
        ));
        assert!(first_run.token().is_cancelled());
        assert!(!second_run.token().is_cancelled());
        handle_incoming_response(&srv, &allow_once(UNKNOWN_ID));
        let answer = TaggedAnswer::decode(&second_answers.try_recv().unwrap()).unwrap();
        assert_eq!(answer.answer, PermissionAnswer::AllowOnce);
        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
        let answer = TaggedAnswer::decode(&first_answers.try_recv().unwrap()).unwrap();
        assert_eq!(answer.answer, PermissionAnswer::AllowOnce);
    }

    #[test]
    fn closing_a_session_saves_the_last_task_events_before_releasing_its_claim() {
        block_on(async {
            let (mut srv, _, _) = test_server();
            let id = fixture_session(&srv).claim.id();
            let control = fixture_session(&srv).handle.session.clone();
            let input = AgentInput::from_defaults(
                PROMPT_TEXT.into(),
                AgentMode::Build,
                Vec::new(),
                SessionDefaults::default(),
                InputSource::Acp,
            );
            control.enqueue(&SessionInput::Prompt(input)).unwrap();
            while !fixture_session(&srv).handle.task.is_finished() {
                future::yield_now().await;
            }
            let (release, ready) = flume::bounded(1);
            fixture_session_mut(&mut srv).pump = spawn(async move {
                ready.recv_async().await.unwrap();
                control.record_task(
                    &Envelope {
                        event: AgentEvent::SubagentHistory {
                            tool_use_id: PARENT_TOOL_USE_ID.into(),
                            messages: vec![Message::user(PROMPT_TEXT.into())],
                        },
                        subagent: None,
                        run_id: 0,
                    },
                    None,
                );
            });
            let sid = SessionId::from(id.to_string());
            let mut closing = std::pin::pin!(close_session(&mut srv, &sid, SessionEndReason::Load));
            assert!(future::poll_once(closing.as_mut()).await.is_none());
            release.send(()).unwrap();
            closing.await;
            let storage = StateDir::from_path(FIXTURE_CLAIMS.path().to_path_buf());
            let (stored, _claim) = StoredSession::claim_and_load(id, &storage).unwrap();
            assert_eq!(
                stored.subagent_messages()[PARENT_TOOL_USE_ID][0].first_text_content(),
                Some(PROMPT_TEXT)
            );
        });
    }

    #[test]
    fn mcp_configuration_errors_reach_the_client_without_a_running_server() {
        const CONFIG_ERROR: &str = "cannot parse MCP configuration";
        let (mut server, _, _) = test_server();
        let id = server.sessions.keys().next().unwrap().clone();
        fixture_session_mut(&mut server).mcp_config_error = Some(CONFIG_ERROR.into());
        let params = acp_params(StateDir::from_path(FIXTURE_CLAIMS.path().to_path_buf()));
        let operation = SessionOperation::Mcp {
            session_id: id,
            toggle: None,
        };
        let reply = block_on(session_operation(
            &mut server,
            &json!({ "params": operation }),
            &params,
        ))
        .unwrap();
        assert_eq!(reply["config_error"], CONFIG_ERROR);
    }

    #[test_case(false ; "foreground")]
    #[test_case(true ; "subagent")]
    fn auth_retry_resumes_only_the_session_that_asked(child_auth: bool) {
        let (mut srv, main, output) = test_server();
        srv.extensions.sessions = true;
        let (child_tx, child) = flume::unbounded();
        run_pump(&srv, None, |sender| {
            sender
                .send_envelope(Envelope {
                    event: AgentEvent::AuthRequired,
                    subagent: child_auth.then(|| subagent(Some(child_tx))),
                    run_id: 0,
                })
                .unwrap();
        });
        assert!(
            output
                .try_iter()
                .any(|value| value["method"] == session_ops::TASK_UPDATE_METHOD)
        );
        let session_id = SessionId::from(fixture_session(&srv).handle.session_id.to_string());
        let operation = SessionOperation::RetryAuth {
            session_id,
            id: child_auth.then(|| PARENT_TOOL_USE_ID.into()),
        };
        let params = acp_params(StateDir::from_path(FIXTURE_CLAIMS.path().to_path_buf()));
        let result = block_on(session_operation(
            &mut srv,
            &json!({ "params": operation }),
            &params,
        ))
        .unwrap();
        assert_eq!(result, true);
        let (answer, other) = if child_auth {
            (child, main)
        } else {
            (main, child)
        };
        assert_eq!(answer.try_recv().unwrap(), "");
        assert!(other.is_empty());
    }

    fn fixture_session(srv: &Server) -> &SessionState {
        assert_eq!(srv.sessions.len(), 1);
        srv.sessions.values().next().expect("fixture session")
    }

    fn fixture_session_mut(srv: &mut Server) -> &mut SessionState {
        assert_eq!(srv.sessions.len(), 1);
        srv.sessions.values_mut().next().expect("fixture session")
    }

    fn save_fixture(session: &mut StoredSession, storage: &StateDir) {
        session
            .save(
                &SessionClaim::acquire(session.id, storage).unwrap(),
                storage,
            )
            .unwrap();
    }

    fn acp_params(storage: StateDir) -> AcpParams {
        AcpParams {
            initial: None,
            administration: None,
            plugins: None,
            model: Model::from_spec(SELECTED_SPEC).expect("a shipped model"),
            config: AgentConfig::default(),
            timeouts: Timeouts::default(),
            initial_wd: PathBuf::from("/project"),
            prompt_slots: Arc::default(),
            yolo: false,
            defaults: SessionDefaults::default(),
            model_policy: Arc::default(),
            plugin_rules: Arc::default(),
            trust_mode: TrustMode::Consult,
            trust_policy: Arc::default(),
            on_session_end: None,
            storage,
            background: false,
            stop: CancelToken::none(),
        }
    }

    fn load_request(id: MakiId) -> Value {
        serde_json::json!({
            "params": { "sessionId": id.to_string(), "cwd": "/project", "mcpServers": [] }
        })
    }

    /// A load that cannot happen must not take the running session down with
    /// it: the client still holds that session, and every prompt it sends next
    /// would otherwise answer "no active session".
    #[test_case(false ; "a session that does not exist")]
    #[test_case(true ; "a session another maki holds")]
    #[tokio::test]
    async fn a_failed_load_leaves_the_running_session_alone(held_elsewhere: bool) {
        let tmp = TempDir::new().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut stored: StoredSession = Session::new(SELECTED_SPEC, "/project");
        let claim = SessionClaim::acquire(stored.id, &storage).unwrap();
        stored.save(&claim, &storage).unwrap();
        let _elsewhere = held_elsewhere.then_some(claim);
        let target = match held_elsewhere {
            true => stored.id,
            false => MakiId::generate(),
        };
        let (mut srv, _answer_rx, _out_rx) = test_server();
        let running = fixture_session(&srv).handle.session_id.clone();

        let result = load_session(&mut srv, &load_request(target), &acp_params(storage)).await;

        assert_eq!(
            result.unwrap_err().code,
            if held_elsewhere {
                SESSION_BUSY_CODE.into()
            } else {
                AcpError::resource_not_found(None).code
            }
        );
        assert_eq!(
            srv.sessions
                .values()
                .next()
                .map(|s| s.handle.session_id.clone()),
            Some(running),
            "{FAILED_LOAD_KEEPS_SESSION}"
        );
    }

    /// The gauge comes after the history it measures, so a client draws the
    /// replay first and then sizes it. A loaded session goes on with the
    /// model `maki acp` runs, not the one it recorded, so that model sizes
    /// it, as its next turns will.
    #[test_case(false ; "standard")]
    #[test_case(true ; "with_the_usage_extension")]
    fn a_load_reports_the_stored_context_after_the_replay(extension: bool) {
        let running_window = Model::from_spec(SELECTED_SPEC).unwrap().context_window;
        assert_ne!(
            running_window,
            Model::from_spec(RECORDED_SPEC).unwrap().context_window
        );
        let tmp = TempDir::new().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut stored: StoredSession = Session::new(RECORDED_SPEC, "/project");
        stored.push_message(Message::user(QUEUED_TEXT.into()));
        stored.push_message(Message {
            role: Role::Assistant,
            content: vec![MsgBlock::tool_use(
                STORED_CALL_ID,
                NEXT_TURN_TOOL,
                json!({}),
            )],
            ..Default::default()
        });
        let data = maki_agent::ToolData::new(
            Arc::from(NEXT_TURN_TOOL),
            maki_agent::DEFAULT_DATA_VERSION,
            json!({}),
            STORED_SUMMARY.into(),
        );
        stored.insert_tool_output(
            STORED_CALL_ID.into(),
            Arc::new(maki_agent::ToolOutput::Plain(QUEUED_TEXT.into())),
            Some(Arc::new(data)),
        );
        stored.meta.context_size = STORED_CONTEXT;
        stored.add_model_usage(
            RECORDED_SPEC,
            TokenUsage::default().billed_with_subsidised_list_cost(Some(0.0), Some(LIST_COST)),
        );
        save_fixture(&mut stored, &storage);
        let (mut srv, _answer_rx, out_rx) = test_server();
        srv.extensions.usage = extension;

        block_on(load_session(
            &mut srv,
            &load_request(stored.id),
            &acp_params(storage),
        ))
        .unwrap();

        let kinds: Vec<Value> = out_rx
            .try_iter()
            .filter(|message| message["method"] == "session/update")
            .map(|message| message["params"]["update"].clone())
            .collect();
        assert_eq!(kinds[0]["sessionUpdate"], "user_message_chunk");
        let titles: Vec<_> = kinds
            .iter()
            .filter(|update| update["sessionUpdate"] == "tool_call_update")
            .map(|update| update["title"].clone())
            .collect();
        assert_eq!(titles, [STORED_SUMMARY]);
        let usage = kinds.last().unwrap();
        assert_eq!(usage["sessionUpdate"], "usage_update");
        assert_eq!(usage["used"], STORED_CONTEXT);
        assert_eq!(usage["size"], running_window);
        match extension {
            true => assert_eq!(wire(usage).list_cost, Some(LIST_COST)),
            false => assert!(usage["_meta"].is_null()),
        }
    }

    /// `maki acp` resolved its startup model before discovery ran, so a
    /// session opened later reads the spec again, as a rebuild does, and
    /// never runs the window the startup model was built with.
    #[test_case(true ; "load")]
    #[test_case(false ; "new")]
    fn a_session_opens_on_the_startup_model_as_its_spec_reads_now(load: bool) {
        let tmp = TempDir::new().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut params = acp_params(storage.clone());
        let current = params.model.context_window;
        params.model.context_window = STALE_WINDOW;
        let (mut srv, _answer_rx, out_rx) = test_server();
        srv.sessions.clear();
        let request = match load {
            true => {
                let mut stored: StoredSession = Session::new(SELECTED_SPEC, "/project");
                stored.push_message(Message::user(QUEUED_TEXT.into()));
                let claim = SessionClaim::acquire(stored.id, &storage).unwrap();
                stored.save(&claim, &storage).unwrap();
                load_request(stored.id)
            }
            false => serde_json::json!({ "params": { "cwd": "/project", "mcpServers": [] } }),
        };

        block_on(async {
            match load {
                true => load_session(&mut srv, &request, &params).await.map(drop),
                false => new_session(&mut srv, &request, &params).await.map(drop),
            }
        })
        .unwrap();

        assert_eq!(fixture_session(&srv).model.context_window, current);
        let sizes: Vec<Value> = out_rx
            .try_iter()
            .map(|message| message["params"]["update"].clone())
            .filter(|update| update["sessionUpdate"] == "usage_update")
            .map(|update| update["size"].clone())
            .collect();
        assert!(sizes.iter().all(|size| *size == current), "{sizes:?}");
    }

    fn pending(srv: &Server) -> &PendingState {
        &fixture_session(srv).pending
    }

    fn permission_ask(answer_tx: Option<Sender<String>>) -> Ask {
        Ask::Permission {
            request_id: PARENT_TOOL_USE_ID.to_owned(),
            answer_tx,
        }
    }

    fn server_with_ask(ask: Ask) -> (Server, flume::Receiver<String>, flume::Receiver<Value>) {
        let (server, answer_rx, out_rx) = test_server();
        pending(&server)
            .lock()
            .unwrap()
            .asks
            .insert(ANSWERED_ID, ask);
        (server, answer_rx, out_rx)
    }

    const STORED_CONTEXT: u32 = 12_345;
    /// A window no shipped model has, standing for the offline table's.
    const STALE_WINDOW: u32 = 1_234;
    const CORRUPT_SESSION: &str = "{ not a session";
    const PROMPT_TEXT: &str = "run the tests";
    /// Stands for the id of the session the test server runs.
    const SESSION_UNDER_TEST: &str = "the running session";
    const COMPACT_INSTRUCTIONS: &str = "keep the API notes";
    const COMPACT_PROMPT: &str = "/compact keep the API notes";
    const SUMMARY: &str = "we renamed foo to bar";
    const RECORDED_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
    const PUMP_CWD: &str = "/project";
    const QUEUED_TEXT: &str = "queued before close";
    const STEER_PLAN: &str = "/project/plan.md";
    const NOTICE_LINE: &str = "Background agent finished: find auth";
    const CHILD_NOTICE_LINE: &str = "a child's own notice";
    const HOST_NOTICE: &str = "the prompt changed, so the cache starts over";
    const PROMPT_ID: i64 = 7;
    const SUBAGENT_COST: f64 = 0.25;
    const CHILD_MODEL: &str = "child-model";
    const CHILD_INPUT: u32 = 42;
    const QUOTA_PROVIDER: &str = "Quota Provider";
    const TURN_COST: f64 = 0.5;
    const LEDGER_COST: f64 = 0.75;
    const CONTEXT_WINDOW: u32 = 200_000;
    const PARENT_TOOL_USE_ID: &str = "toolu_1";
    /// A string id no ask can ever carry, since ours are minted as numbers.
    const UNANSWERABLE_ID: &str = "not-an-id";
    const SUBAGENT_NAME: &str = "task";
    const CHILD_TOOL_USE_IDS: [&str; 2] = ["child_write_1", "child_write_2"];
    const PERMISSION_TOOL: &str = "write";
    const MAIN_PERMISSION_TITLE: &str = "write: /project";
    const CHILD_PERMISSION_TITLE: &str = "task: write: /project";
    const ESCALATION_REASON: &str = "plugin flagged this write";
    const ESCALATED_PERMISSION_TITLE: &str = "plugin flagged this write (write: /project)";
    const TURN_ERROR: &str = "provider returned 500";
    /// One tool id arriving from two runs of the same session. `openai_compat`
    /// mints unique ids now, but a provider can still repeat one, and the pump
    /// has to keep the runs apart either way.
    const COLLIDING_TOOL_USE_ID: &str = "maki_unnamed_0";
    /// These cover cost and transcript plumbing, not the trust-scoped wording
    /// of permission options.
    const PUMP_TRUSTED: bool = true;
    fn run_pump(srv: &Server, initial_cost: Option<f64>, feed: impl FnOnce(&EventSender)) {
        let ledger = SessionLedger {
            cost: initial_cost,
            ..SessionLedger::default()
        };
        run_pump_with(srv, PumpUsage::new(false, ledger, None), feed);
    }

    fn run_pump_with(srv: &Server, usage: PumpUsage, feed: impl FnOnce(&EventSender)) {
        let session = fixture_session(srv);
        let (guard, events) = maki_agent::event_stream();
        let pump = start_event_pump(
            events,
            session.handle.answer_tx.clone(),
            session.handle.session_id.clone(),
            srv.out_tx.clone(),
            Arc::clone(&session.pending),
            PathBuf::from(PUMP_CWD),
            None,
            PUMP_TRUSTED,
            usage,
            Compactions::new(srv.client_compaction),
            srv.extensions.tool_data,
            None,
            srv.extensions
                .sessions
                .then(|| session.handle.session.clone()),
            None,
        );
        let sender = guard.sender(0);
        feed(&sender);
        drop(guard);
        block_on(pump).unwrap();
    }

    #[test_case(false ; "tool_task")]
    #[test_case(true ; "plugin_task")]
    fn native_tasks_keep_their_scope_tool_data_and_stored_transcript(plugin_task: bool) {
        const PLUGIN: &str = "supervisor";
        let scoped = |envelope: Envelope| {
            if plugin_task {
                Envelope {
                    event: AgentEvent::PluginTask {
                        plugin: PLUGIN.into(),
                        envelope: Box::new(envelope),
                    },
                    subagent: None,
                    run_id: 0,
                }
            } else {
                envelope
            }
        };
        let (mut srv, _, out) = test_server();
        srv.extensions.sessions = true;
        let control = fixture_session(&srv).handle.session.clone();
        let input = AgentInput::from_defaults(
            PROMPT_TEXT.into(),
            AgentMode::Build,
            Vec::new(),
            SessionDefaults::default(),
            InputSource::Acp,
        );
        control.enqueue(&SessionInput::Prompt(input)).unwrap();
        let (answers, answered) = flume::unbounded();
        let done = ToolDoneEvent {
            tool: PERMISSION_TOOL.into(),
            data: Some(Arc::new(ToolData::new(
                PERMISSION_TOOL.into(),
                DEFAULT_DATA_VERSION,
                json!({}),
                PROMPT_TEXT.into(),
            ))),
            ..ToolDoneEvent::error(COLLIDING_TOOL_USE_ID.into(), TURN_ERROR)
        };
        run_pump(&srv, None, |sender| {
            let send = |envelope| sender.send_envelope(scoped(envelope)).unwrap();
            send(child(AgentEvent::ToolProgress(Box::new(
                ToolProgressEvent {
                    id: COLLIDING_TOOL_USE_ID.into(),
                    path: Vec::new(),
                    append: PROMPT_TEXT.into(),
                    state: None,
                    child: None,
                },
            ))));
            send(child(AgentEvent::ToolDone(Box::new(done))));
            send(child(AgentEvent::TurnComplete(turn_complete(TURN_COST))));
            let mut ask = child(permission_request(COLLIDING_TOOL_USE_ID));
            ask.subagent.as_mut().unwrap().answer_tx = Some(answers);
            send(ask);
            send(Envelope {
                event: AgentEvent::SubagentHistory {
                    tool_use_id: PARENT_TOOL_USE_ID.into(),
                    messages: vec![Message::user(PROMPT_TEXT.into())],
                },
                subagent: None,
                run_id: 0,
            });
        });
        let sent = out.try_iter().collect::<Vec<_>>();
        let ask = sent
            .iter()
            .find(|value| value["method"] == "session/request_permission")
            .unwrap();
        assert_eq!(
            ask["params"]["_meta"][session_ops::TASK_PERMISSION_KEY]["parent_tool_use_id"],
            PARENT_TOOL_USE_ID
        );
        handle_incoming_response(&srv, &allow_once(ask["id"].as_i64().unwrap()));
        assert_eq!(
            answered.try_recv().unwrap(),
            TaggedAnswer::new(COLLIDING_TOOL_USE_ID, PermissionAnswer::AllowOnce).encode()
        );
        assert_eq!(
            ask["params"]["_meta"][session_ops::PLUGIN_TASK_KEY].as_str(),
            plugin_task.then_some(PLUGIN)
        );
        let updates = sent
            .into_iter()
            .filter(|value| value["method"] == session_ops::TASK_UPDATE_METHOD)
            .map(|value| {
                serde_json::from_value::<session_ops::TaskUpdate>(value["params"].clone()).unwrap()
            })
            .map(|mut update| {
                if let AgentEvent::PluginTask { plugin, envelope } = update.envelope.event {
                    assert_eq!(&*plugin, PLUGIN);
                    update.envelope = *envelope;
                }
                update
            })
            .collect::<Vec<_>>();
        assert!(updates.iter().any(|update| matches!(&update.envelope.event,
            AgentEvent::ToolProgress(progress) if progress.path.is_empty() && progress.append == PROMPT_TEXT)));
        let done = updates
            .iter()
            .find_map(|update| match &update.envelope.event {
                AgentEvent::ToolDone(done) => {
                    Some((update.envelope.subagent.as_ref().unwrap(), done))
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(done.0.parent_tool_use_id, PARENT_TOOL_USE_ID);
        assert_eq!(done.1.data.as_ref().unwrap().summary, PROMPT_TEXT);
        assert!(updates.iter().any(|update| matches!(&update.envelope.event, AgentEvent::TurnComplete(turn) if turn.cost == Some(TURN_COST))));
        let stored = control.snapshot();
        assert_eq!(
            stored.subagents()[0].plugin.as_deref(),
            plugin_task.then_some(PLUGIN)
        );
        assert_eq!(stored.subagents()[0].cost, Some(TURN_COST));
        assert_eq!(
            stored.tool_data()[COLLIDING_TOOL_USE_ID].summary,
            PROMPT_TEXT
        );
        assert_eq!(
            stored.subagent_messages()[PARENT_TOOL_USE_ID][0].first_text_content(),
            Some(PROMPT_TEXT)
        );
    }

    fn turn_complete(cost: f64) -> Box<TurnCompleteEvent> {
        Box::new(TurnCompleteEvent {
            message: Message::user(String::new()),
            usage: TokenUsage::default(),
            model: SELECTED_SPEC.to_owned(),
            cost: Some(cost),
            subsidised_list_cost: None,
            subsidised_by: None,
            context_size: None,
            context_window: CONTEXT_WINDOW,
        })
    }

    fn subagent(answer_tx: Option<Sender<String>>) -> SubagentInfo {
        SubagentInfo {
            parent_tool_use_id: PARENT_TOOL_USE_ID.to_owned(),
            name: SUBAGENT_NAME.to_owned(),
            prompt: None,
            model: None,
            opts: None,
            answer_tx,
            inbox: None,
            parent_call: None,
        }
    }

    fn done_event() -> AgentEvent {
        done_with(DoneReason::EndTurn)
    }

    fn done_with(reason: DoneReason) -> AgentEvent {
        AgentEvent::Done {
            usage: TokenUsage::default(),
            cost: None,
            list_cost: None,
            context_size: 0,
            context_window: CONTEXT_WINDOW,
            num_turns: 1,
            reason,
        }
    }

    fn write_tool_start(tool_use_id: &str) -> AgentEvent {
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: tool_use_id.to_owned(),
            tool: Arc::from(PERMISSION_TOOL),
            summary: String::new(),
            annotation: None,
            input: None,
            raw_input: Some(serde_json::json!({"path": PUMP_CWD, "content": QUEUED_TEXT})),
            output: None,
            data_version: DEFAULT_DATA_VERSION,
        }))
    }

    fn permission_request(tool_use_id: &str) -> AgentEvent {
        AgentEvent::PermissionRequest {
            id: tool_use_id.to_owned(),
            tool: ToolKey::native(PERMISSION_TOOL),
            scopes: vec![PUMP_CWD.to_owned()],
            reason: None,
            choices: Vec::new(),
        }
    }

    #[test_case(false ; "answers_reach_the_agent_that_asked_even_out_of_order")]
    #[test_case(true ; "cancel_keeps_the_asks_a_moved_call_may_wait_on")]
    #[tokio::test]
    async fn concurrent_subagent_permissions(cancel: bool) {
        let (srv, main_rx, out_rx) = test_server();
        let children = [flume::unbounded(), flume::unbounded()];
        run_pump(&srv, None, |sender| {
            for ((answer_tx, _), child_id) in children.iter().zip(CHILD_TOOL_USE_IDS) {
                let info = subagent(Some(answer_tx.clone()));
                for event in [
                    AgentEvent::TextDelta {
                        text: QUEUED_TEXT.to_owned(),
                    },
                    permission_request(child_id),
                ] {
                    sender
                        .send_envelope(Envelope {
                            event,
                            subagent: Some(info.clone()),
                            run_id: 0,
                        })
                        .unwrap();
                }
            }
            sender.try_send(permission_request(PARENT_TOOL_USE_ID));
        });

        let requests: Vec<_> = out_rx.try_iter().collect();
        assert_eq!(
            requests.len(),
            3,
            "of the subagent events only permissions reach the client"
        );
        for (request, title) in requests.iter().zip([
            CHILD_PERMISSION_TITLE,
            CHILD_PERMISSION_TITLE,
            MAIN_PERMISSION_TITLE,
        ]) {
            assert_eq!(request["method"], "session/request_permission");
            let call = &request["params"]["toolCall"];
            assert_eq!(
                call["toolCallId"], PARENT_TOOL_USE_ID,
                "a child permission rides on the task call the client knows"
            );
            assert_eq!(call["title"], title);
        }

        if cancel {
            block_on(handle_notification(
                &srv,
                "session/cancel",
                &json!({ "params": { "sessionId": fixture_session(&srv).handle.session_id.as_str() } }),
            ));
        }
        for (request, option) in requests
            .iter()
            .zip(["reject_once", "allow_once", "allow_always"])
            .rev()
        {
            handle_incoming_response(
                &srv,
                &serde_json::json!({
                    "id": request["id"],
                    "result": {"outcome": {"outcome": "selected", "optionId": option}},
                }),
            );
        }
        let child_rx = children.map(|(_, rx)| rx);
        for ((rx, expected), asked_id) in child_rx
            .iter()
            .chain([&main_rx])
            .zip([
                PermissionAnswer::Deny,
                PermissionAnswer::AllowOnce,
                PermissionAnswer::AllowSession,
            ])
            .zip(CHILD_TOOL_USE_IDS.iter().chain([&PARENT_TOOL_USE_ID]))
        {
            let expected = TaggedAnswer::new(*asked_id, expected).encode();
            assert_eq!(rx.try_recv().ok(), Some(expected));
            assert!(rx.is_empty(), "each answer is delivered only once");
        }
    }

    /// Without a channel of its own a child's answer would land in the main
    /// agent's, and the main agent's next wait would eat it as its own.
    #[tokio::test]
    async fn a_subagent_permission_without_an_answer_channel_is_never_asked() {
        let (srv, main_rx, out_rx) = test_server();
        run_pump(&srv, None, |sender| {
            sender
                .send_envelope(Envelope {
                    event: permission_request(CHILD_TOOL_USE_IDS[0]),
                    subagent: Some(subagent(None)),
                    run_id: 0,
                })
                .unwrap();
        });

        assert!(out_rx.is_empty(), "the client is never asked");
        assert!(main_rx.is_empty(), "the main agent keeps its own turn");
        assert!(
            pending(&srv).lock().unwrap().asks.is_empty(),
            "nothing is left waiting for an answer"
        );
    }

    /// Runs one permission wait the way a turn does, on the session's own
    /// answer channel. Whatever is queued when this is called is what the
    /// waiter finds, which is the shape of the bug: answers outlive the turn
    /// that asked for them.
    async fn wait_for_permission(
        srv: &Server,
        answer_rx: &flume::Receiver<String>,
        request_id: &str,
    ) -> Result<(), PermissionError> {
        let session = fixture_session(srv);
        let (guard, _events) = maki_agent::event_stream();
        let event_tx = guard.sender(0);
        let rx = tokio::sync::Mutex::new(answer_rx.clone());
        session
            .handle
            .permissions
            .enforce(
                &ToolKey::native(NEXT_TURN_TOOL),
                &PermissionScopes::single(NEXT_TURN_SCOPE.to_owned()),
                None,
                &event_tx,
                Some(&rx),
                request_id,
                &CancelToken::none(),
                None,
                None,
                None,
            )
            .await
            // No layers here, so nothing can have answered in a plugin's name.
            .map(|_| ())
    }

    fn queue_answer(srv: &Server, request_id: &str, answer: PermissionAnswer) {
        let session = fixture_session(srv);
        session
            .handle
            .answer_tx
            .send(TaggedAnswer::new(request_id, answer).encode())
            .unwrap();
    }

    fn next_turn_is_allowed(srv: &Server) -> bool {
        let session = fixture_session(srv);
        matches!(
            session.handle.permissions.check(
                &ToolKey::native(NEXT_TURN_TOOL),
                NEXT_TURN_SCOPE,
                None
            ),
            PermissionCheck::Allowed
        )
    }

    /// The race the ids exist for: the cancel lands while the permission
    /// envelope is still queued in the pump, so the pump still asks and the
    /// client answers it for real. With nobody parked, that answer waits in
    /// the channel for the next turn.
    #[tokio::test]
    async fn an_answer_for_a_cancelled_ask_is_not_consumed_by_the_next_turn() {
        let (srv, answer_rx, out_rx) = test_server();
        block_on(handle_notification(
            &srv,
            "session/cancel",
            &json!({ "params": { "sessionId": fixture_session(&srv).handle.session_id.as_str() } }),
        ));
        run_pump(&srv, None, |sender| {
            sender.try_send(permission_request(PARENT_TOOL_USE_ID));
        });

        let request = out_rx.try_recv().expect("the pump still asked the client");
        handle_incoming_response(
            &srv,
            &serde_json::json!({
                "id": request["id"],
                "result": {"outcome": {"outcome": "selected", "optionId": "allow_always"}},
            }),
        );

        queue_answer(&srv, NEXT_TURN_TOOL_USE_ID, PermissionAnswer::Deny);
        let outcome = wait_for_permission(&srv, &answer_rx, NEXT_TURN_TOOL_USE_ID).await;

        assert!(outcome.is_err(), "{STALE_ALLOW}");
        assert!(!next_turn_is_allowed(&srv), "{STALE_ALLOW}");
        assert!(
            answer_rx.is_empty(),
            "the stale answer was dropped, not left"
        );
    }

    /// A mismatched answer is dropped without ending the wait: the tool is
    /// still blocked on the ask the user was shown, and only that ask's answer
    /// may end it.
    #[tokio::test]
    async fn an_answer_naming_another_ask_is_discarded_and_the_wait_continues() {
        let (srv, answer_rx, ..) = test_server();
        queue_answer(&srv, PARENT_TOOL_USE_ID, PermissionAnswer::Deny);
        queue_answer(&srv, NEXT_TURN_TOOL_USE_ID, PermissionAnswer::AllowOnce);

        let outcome = wait_for_permission(&srv, &answer_rx, NEXT_TURN_TOOL_USE_ID).await;

        assert!(
            outcome.is_ok(),
            "the deny named another ask and must not have ended this wait"
        );
        assert!(
            answer_rx.is_empty(),
            "both answers were taken off the queue"
        );
    }

    #[test_case(PermissionAnswer::AllowSession, true ; "allow_applies_the_decision")]
    #[test_case(PermissionAnswer::Deny, false ; "deny_applies_the_decision")]
    #[tokio::test]
    async fn a_matching_answer_is_applied(answer: PermissionAnswer, allowed: bool) {
        let (srv, answer_rx, ..) = test_server();
        queue_answer(&srv, NEXT_TURN_TOOL_USE_ID, answer);

        let outcome = wait_for_permission(&srv, &answer_rx, NEXT_TURN_TOOL_USE_ID).await;

        assert_eq!(outcome.is_ok(), allowed);
        assert_eq!(next_turn_is_allowed(&srv), allowed);
    }

    /// The close marker rides the same FIFO as the events, so a turn still
    /// streaming when the session got replaced is reported in full and the
    /// client's outstanding `session/prompt` is answered instead of hanging.
    #[tokio::test]
    async fn event_pump_delivers_everything_queued_before_the_close() {
        let (srv, .., out_rx) = test_server();
        pending(&srv).lock().unwrap().prompt = Some(RequestId::Number(PROMPT_ID));
        run_pump(&srv, None, |sender| {
            sender.try_send(AgentEvent::TextDelta {
                text: QUEUED_TEXT.to_owned(),
            });
            sender.try_send(done_event());
        });

        let chunk = out_rx.try_recv().expect("the queued text reaches the wire");
        let update = &chunk["params"]["update"];
        assert_eq!(update["sessionUpdate"], "agent_message_chunk");
        assert_eq!(update["content"]["text"], QUEUED_TEXT);

        let answer = out_rx.try_recv().expect("the pending prompt is answered");
        assert_eq!(answer["id"], PROMPT_ID);
        assert_eq!(answer["result"]["stopReason"], "end_turn");
        assert!(pending(&srv).lock().unwrap().prompt.is_none());
    }

    /// An accepted observation's line arrives as a finished card, in the order
    /// the agent sent it, and the turn's permission and answer routing stay as
    /// they were. A host notice is not sent, and a child's line stays in the
    /// child, like the rest of its stream.
    #[tokio::test]
    async fn an_accepted_notice_reaches_the_client_in_stream_order() {
        let (srv, .., out_rx) = test_server();
        pending(&srv).lock().unwrap().prompt = Some(RequestId::Number(PROMPT_ID));
        run_pump(&srv, None, |sender| {
            sender.try_send(AgentEvent::ObservationLine {
                text: NOTICE_LINE.to_owned(),
                detail: None,
                end: None,
            });
            sender.try_send(AgentEvent::Notice {
                text: HOST_NOTICE.to_owned(),
            });
            sender
                .send_envelope(Envelope {
                    event: AgentEvent::ObservationLine {
                        text: CHILD_NOTICE_LINE.to_owned(),
                        detail: None,
                        end: None,
                    },
                    subagent: Some(subagent(None)),
                    run_id: 0,
                })
                .unwrap();
            sender.try_send(AgentEvent::TextDelta {
                text: QUEUED_TEXT.to_owned(),
            });
            sender.try_send(write_tool_start(PARENT_TOOL_USE_ID));
            sender.try_send(permission_request(PARENT_TOOL_USE_ID));
            sender.try_send(done_event());
        });

        let wire: Vec<Value> = out_rx.try_iter().collect();
        let text = serde_json::to_value(translate::text_delta(QUEUED_TEXT)).unwrap();
        assert_eq!(wire.len(), 5, "{wire:?}");
        let card = &wire[0]["params"]["update"];
        assert_eq!(card["sessionUpdate"], "tool_call");
        assert_eq!(card["title"], NOTICE_LINE);
        assert_eq!(card["status"], "completed");
        assert_eq!(wire[1]["params"]["update"], text);
        assert_eq!(
            wire[2]["params"]["update"]["sessionUpdate"],
            "tool_call_update"
        );
        assert_eq!(wire[3]["method"], "session/request_permission");
        assert_eq!(
            wire[3]["params"]["toolCall"]["title"],
            MAIN_PERMISSION_TITLE
        );
        assert_eq!(wire[4]["id"], PROMPT_ID);
        assert_eq!(wire[4]["result"]["stopReason"], "end_turn");
    }

    /// Clients only prompt a login on `auth_required`, so a rejected credential
    /// must not come back as a generic failure.
    #[test_case(true, AcpError::auth_required() ; "auth_error_asks_for_a_login")]
    #[test_case(false, AcpError::internal_error() ; "other_error_is_internal")]
    #[tokio::test]
    async fn a_failed_turn_answers_the_prompt_with_its_error_kind(auth: bool, expected: AcpError) {
        let (srv, .., out_rx) = test_server();
        pending(&srv).lock().unwrap().prompt = Some(RequestId::Number(PROMPT_ID));
        run_pump(&srv, None, |sender| {
            sender.try_send(AgentEvent::Error {
                message: TURN_ERROR.to_owned(),
                auth,
                totals: RunTotals::default(),
            });
        });

        let answer = out_rx.try_recv().expect("the pending prompt is answered");
        assert_eq!(answer["id"], PROMPT_ID);
        assert_eq!(answer["error"]["code"], i32::from(expected.code));
        assert_eq!(answer["error"]["data"], TURN_ERROR);
    }

    /// A cached input holds the whole file a `write` is about to lay down, and
    /// this pump lives as long as the session does, so a turn that died on a
    /// provider error has to let go of it just like a turn that finished.
    #[test_case(None, true ; "a_live_turn_shows_the_file_its_tool_call_named")]
    #[test_case(Some(done_event()), false ; "a_finished_turn_releases_its_tool_inputs")]
    #[test_case(Some(AgentEvent::Error { message: TURN_ERROR.to_owned(), auth: false, totals: RunTotals::default() }), false ; "a_failed_turn_releases_its_tool_inputs")]
    #[tokio::test]
    async fn a_terminal_event_releases_the_turns_tool_inputs(
        terminal: Option<AgentEvent>,
        keeps_input: bool,
    ) {
        let (srv, .., out_rx) = test_server();
        run_pump(&srv, None, |sender| {
            sender.try_send(write_tool_start(PARENT_TOOL_USE_ID));
            if let Some(event) = terminal {
                sender.try_send(event);
            }
            sender.try_send(permission_request(PARENT_TOOL_USE_ID));
        });

        let request = out_rx
            .try_iter()
            .last()
            .expect("the permission request reaches the client");
        let call = &request["params"]["toolCall"];
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(call["title"], MAIN_PERMISSION_TITLE);
        assert_eq!(!call["rawInput"].is_null(), keeps_input, "{request}");
    }

    /// Ids are only unique inside one run, so a child can ask about a call whose
    /// id the parent already cached. The dialog has to stay title only, or it
    /// would name a file the subagent is not touching.
    #[tokio::test]
    async fn a_subagent_permission_ignores_a_colliding_cached_input() {
        let (srv, .., out_rx) = test_server();
        let (answer_tx, _answer_rx) = flume::unbounded();
        run_pump(&srv, None, |sender| {
            sender.try_send(write_tool_start(COLLIDING_TOOL_USE_ID));
            sender
                .send_envelope(Envelope {
                    event: permission_request(COLLIDING_TOOL_USE_ID),
                    subagent: Some(subagent(Some(answer_tx))),
                    run_id: 0,
                })
                .unwrap();
        });

        let request = out_rx
            .try_iter()
            .last()
            .expect("the permission request reaches the client");
        let call = &request["params"]["toolCall"];
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(call["toolCallId"], PARENT_TOOL_USE_ID);
        assert_eq!(call["title"], CHILD_PERMISSION_TITLE);
        for field in ["kind", "locations", "rawInput", "content"] {
            assert!(call[field].is_null(), "{field} must stay unset: {request}");
        }
    }

    #[test_case(Some(ESCALATION_REASON), ESCALATED_PERMISSION_TITLE ; "a_plugin_reason_leads_the_title")]
    #[test_case(None, MAIN_PERMISSION_TITLE ; "no_reason_keeps_the_plain_title")]
    #[tokio::test]
    async fn permission_title_carries_the_escalation_reason(reason: Option<&str>, title: &str) {
        let (srv, .., out_rx) = test_server();
        run_pump(&srv, None, |sender| {
            sender.try_send(AgentEvent::PermissionRequest {
                id: PARENT_TOOL_USE_ID.to_owned(),
                tool: ToolKey::native(PERMISSION_TOOL),
                scopes: vec![PUMP_CWD.to_owned()],
                reason: reason.map(str::to_owned),
                choices: Vec::new(),
            });
        });

        let request = out_rx
            .try_recv()
            .expect("the permission request reaches the client");
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(request["params"]["toolCall"]["title"], title);
    }

    const COMPACTION_WINDOW: u32 = 64_000;
    /// The main model after a switch right before the compaction.
    const SWITCHED_WINDOW: u32 = 400_000;
    const MAIN_USED: u32 = 9_000;
    const AFTER_COMPACTION: u32 = 3_000;
    const LIST_COST: f64 = 0.75;
    const SUBSIDY: &str = "Max plan";

    fn main_turn(cost: f64) -> Box<TurnCompleteEvent> {
        Box::new(TurnCompleteEvent {
            context_size: Some(MAIN_USED),
            subsidised_list_cost: Some(LIST_COST),
            subsidised_by: Some(Arc::from(SUBSIDY)),
            ..*turn_complete(cost)
        })
    }

    fn child(event: AgentEvent) -> Envelope {
        Envelope {
            event,
            subagent: Some(subagent(None)),
            run_id: 0,
        }
    }

    fn with_extension() -> PumpUsage {
        PumpUsage::new(true, SessionLedger::default(), None)
    }

    /// The usage updates the pump sent, in order.
    fn usage_updates(out_rx: &flume::Receiver<Value>) -> Vec<Value> {
        out_rx
            .try_iter()
            .map(|message| message["params"]["update"].clone())
            .filter(|update| update["sessionUpdate"] == "usage_update")
            .collect()
    }

    fn wire(update: &Value) -> WireUsage {
        serde_json::from_value(update["_meta"][META_KEY][usage::USAGE_KEY].clone()).unwrap()
    }

    #[test_case(true ; "with_ledger")]
    #[test_case(false ; "without_ledger")]
    fn usage_tracks_spending_without_moving_the_main_gauge_for_other_turns(shared: bool) {
        let (srv, .., out_rx) = test_server();
        let usage = PumpUsage::new(
            true,
            SessionLedger {
                cost: Some(RECORDED_COST),
                ..SessionLedger::default()
            },
            None,
        )
        .sharing_ledger(shared);
        let mut spent = *turn_complete(SUBAGENT_COST);
        spent.model = CHILD_MODEL.into();
        spent.usage.input = CHILD_INPUT;
        spent.context_window = COMPACTION_WINDOW;
        let mut compaction = *turn_complete(SUBAGENT_COST);
        compaction.context_window = COMPACTION_WINDOW;
        run_pump_with(&srv, usage, |sender| {
            sender.try_send(AgentEvent::TurnComplete(main_turn(TURN_COST)));
            sender
                .send_envelope(child(AgentEvent::TurnComplete(Box::new(spent))))
                .unwrap();
            sender.try_send(AgentEvent::AutoCompacting {
                context_size: MAIN_USED,
                context_window: SWITCHED_WINDOW,
                manual: false,
            });
            sender.try_send(AgentEvent::TurnComplete(Box::new(compaction)));
            sender.try_send(AgentEvent::CompactionDone {
                context_size_before: MAIN_USED,
                context_size_after: AFTER_COMPACTION,
                context_window: SWITCHED_WINDOW,
                summary: String::new(),
            });
        });
        let updates = usage_updates(&out_rx);
        let [main, child, compacting, done] = &updates[..] else {
            panic!("expected main, child, compaction spending and final gauge");
        };
        assert_eq!(
            main["cost"]["amount"].as_f64(),
            Some(RECORDED_COST + TURN_COST)
        );
        let usage = wire(main);
        assert_eq!(usage.list_cost, Some(LIST_COST));
        assert_eq!(usage.subsidised_by.as_deref(), Some(SUBSIDY));
        let turn = usage.turn.unwrap();
        assert_eq!(turn.model, SELECTED_SPEC);
        assert_eq!(turn.usage.cost, Some(TURN_COST));
        assert_eq!(main["used"], MAIN_USED);
        assert_eq!(child["used"], MAIN_USED);
        assert_eq!(child["size"], CONTEXT_WINDOW);
        assert_eq!(
            child["cost"]["amount"].as_f64(),
            Some(RECORDED_COST + TURN_COST + SUBAGENT_COST)
        );
        assert!(wire(child).turn.is_none());
        assert_eq!(compacting["size"], SWITCHED_WINDOW);
        assert!(wire(compacting).turn.is_none());
        assert_eq!(done["used"], AFTER_COMPACTION);
        assert_eq!(done["size"], SWITCHED_WINDOW);
        let ledger = wire(done).ledger;
        assert_eq!(ledger.is_some(), shared);
        if let Some(ledger) = ledger {
            assert_eq!(ledger.total.input, CHILD_INPUT);
            assert_eq!(
                ledger.cost,
                Some(RECORDED_COST + TURN_COST + SUBAGENT_COST * 2.0)
            );
            assert_eq!(
                ledger.by_model[SELECTED_SPEC].cost,
                Some(TURN_COST + SUBAGENT_COST)
            );
            assert_eq!(
                ledger.by_model[SELECTED_SPEC].subsidised_list_cost,
                Some(LIST_COST)
            );
            assert_eq!(ledger.by_model[CHILD_MODEL].cost, Some(SUBAGENT_COST));
        }
    }

    /// A loaded session's gauge comes from its replay, so spending before the
    /// next main turn still reaches the client.
    #[test]
    fn the_replay_gauge_seeds_the_totals_that_follow() {
        let mut usage = with_extension();
        usage.gauge(STORED_CONTEXT, CONTEXT_WINDOW);
        let update = usage
            .turn(&turn_complete(SUBAGENT_COST), false)
            .expect("the seeded gauge carries the child's spending");
        let SessionUpdate::UsageUpdate(update) = update else {
            panic!("expected a usage update");
        };
        assert_eq!(update.used, u64::from(STORED_CONTEXT));
        assert_eq!(update.cost.map(|cost| cost.amount), Some(SUBAGENT_COST));
    }

    fn compaction_started() -> AgentEvent {
        AgentEvent::AutoCompacting {
            context_size: MAIN_USED,
            context_window: CONTEXT_WINDOW,
            manual: false,
        }
    }

    fn compaction_finished() -> AgentEvent {
        AgentEvent::CompactionDone {
            context_size_before: MAIN_USED,
            context_size_after: AFTER_COMPACTION,
            context_window: CONTEXT_WINDOW,
            summary: SUMMARY.into(),
        }
    }

    fn turn_failed() -> AgentEvent {
        AgentEvent::Error {
            message: TURN_ERROR.into(),
            auth: false,
            totals: RunTotals::default(),
        }
    }

    fn compaction_updates(reported: bool, events: Vec<AgentEvent>) -> Vec<Value> {
        let (mut srv, _answer_rx, out_rx) = test_server();
        srv.client_compaction = reported;
        run_pump_with(&srv, with_extension(), |sender| {
            for event in events {
                sender.try_send(event);
            }
        });
        out_rx
            .try_iter()
            .map(|message| message["params"]["update"].clone())
            .filter(|update| update["sessionUpdate"] == "compaction_update")
            .collect()
    }

    /// A hook can skip a manual compaction, which then ends with no
    /// `CompactionDone`.
    #[test_case(vec![compaction_started(), compaction_finished(), done_with(DoneReason::Compact)], "completed", "/summary/0/text", SUMMARY ; "completed")]
    #[test_case(vec![compaction_started(), turn_failed()], "failed", "/error", TURN_ERROR ; "failed")]
    #[test_case(vec![compaction_started(), done_with(DoneReason::Cancelled)], "cancelled", "/status", "cancelled" ; "cancelled")]
    #[test_case(vec![compaction_started(), done_with(DoneReason::Compact)], COMPACTION_SKIPPED, "/status", COMPACTION_SKIPPED ; "skipped")]
    fn a_compaction_reports_how_it_ended(
        events: Vec<AgentEvent>,
        status: &str,
        field: &str,
        value: &str,
    ) {
        let [started, ended] = &compaction_updates(true, events)[..] else {
            panic!("expected a start and an end");
        };
        assert_eq!(started["status"], "in_progress");
        assert_eq!(ended["status"], status);
        assert_eq!(ended.pointer(field), Some(&Value::from(value)));
        assert_eq!(started["compactionId"], ended["compactionId"]);
    }

    /// A manual start says so on the wire, so the client draws no automatic
    /// line for it.
    #[test_case(true ; "manual")]
    #[test_case(false ; "automatic")]
    fn a_compaction_start_says_whether_it_is_manual(manual: bool) {
        let started = AgentEvent::AutoCompacting {
            context_size: MAIN_USED,
            context_window: CONTEXT_WINDOW,
            manual,
        };
        let [update, ..] = &compaction_updates(true, vec![started, turn_failed()])[..] else {
            panic!("expected the start");
        };
        assert_eq!(
            update.pointer(&format!("/_meta/{}/manual", META_KEY)) == Some(&Value::Bool(true)),
            manual
        );
    }

    #[test]
    fn each_compaction_gets_its_own_id() {
        let events = vec![
            compaction_started(),
            compaction_finished(),
            compaction_started(),
            turn_failed(),
        ];
        let [first, _, second, _] = &compaction_updates(true, events)[..] else {
            panic!("expected two compactions");
        };
        assert_ne!(first["compactionId"], second["compactionId"]);
    }

    /// The schema allows the update only to a client that advertised it.
    #[test]
    fn a_client_without_the_capability_gets_no_compaction_updates() {
        let events = vec![compaction_started(), compaction_finished(), done_event()];
        assert!(compaction_updates(false, events).is_empty());
    }

    /// A failed compaction sends no `CompactionDone`, and the next run's
    /// ordinary turn must not be taken for a compaction turn.
    #[test]
    fn a_failed_compaction_does_not_swallow_the_next_turn() {
        let (srv, .., out_rx) = test_server();
        run_pump_with(&srv, with_extension(), |sender| {
            sender.try_send(AgentEvent::AutoCompacting {
                context_size: MAIN_USED,
                context_window: CONTEXT_WINDOW,
                manual: false,
            });
            sender.try_send(AgentEvent::Error {
                message: TURN_ERROR.into(),
                auth: false,
                totals: RunTotals::default(),
            });
            sender.try_send(AgentEvent::TurnComplete(main_turn(TURN_COST)));
        });

        let [update] = &usage_updates(&out_rx)[..] else {
            panic!("expected the next run's turn");
        };
        assert!(wire(update).turn.is_some());
    }

    fn results_submitted() -> AgentEvent {
        AgentEvent::ToolResultsSubmitted {
            message: Box::new(Message::user(String::new())),
        }
    }

    /// Answers with a fixed quota when it has one.
    struct Quota(Option<ProviderUsage>);

    impl Provider for Quota {
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
            Box::pin(async { Ok(StreamResponse::default()) })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
            let quota = self.0.clone();
            Box::pin(async move { Ok(quota) })
        }
    }

    fn quota_answer(
        connect: impl Future<Output = Result<(Arc<dyn Provider>, Model), AgentError>>,
    ) -> Value {
        let (out_tx, out_rx) = flume::unbounded();
        block_on(answer_quota(
            out_tx,
            RequestId::Number(PROMPT_ID),
            QUOTA_PROVIDER.into(),
            connect,
        ));
        let answer = out_rx.try_recv().unwrap();
        assert_eq!(answer["id"], PROMPT_ID);
        assert_eq!(answer["result"]["provider"], QUOTA_PROVIDER);
        answer["result"].clone()
    }

    #[test_case(Some(ProviderUsage::default()), "ready" ; "ready")]
    #[test_case(None, "unsupported" ; "unsupported")]
    fn a_quota_answer_names_its_provider_and_state(quota: Option<ProviderUsage>, state: &str) {
        let model = Model::from_spec(SELECTED_SPEC).unwrap();
        let connect = async { Ok((Arc::new(Quota(quota)) as Arc<dyn Provider>, model)) };
        assert!(quota_answer(connect).get(state).is_some());
    }

    #[test]
    fn a_failed_connection_is_a_quota_error() {
        let connect = async {
            Err(AgentError::Config {
                message: TURN_ERROR.into(),
            })
        };
        assert!(quota_answer(connect)["error"].is_string());
    }

    /// The agent's own boundary: the client cannot infer it, because a
    /// retried stream announces a call that never finishes.
    #[test_case(true, false, true ; "main_under_the_extension")]
    #[test_case(true, true, false ; "a_childs_stays_out")]
    #[test_case(false, false, false ; "without_the_extension")]
    fn the_results_boundary_reaches_the_client(extension: bool, child_turn: bool, sent: bool) {
        let (srv, .., out_rx) = test_server();
        let usage = PumpUsage::new(extension, SessionLedger::default(), None);
        run_pump_with(&srv, usage, |sender| {
            sender.try_send(AgentEvent::TurnComplete(main_turn(TURN_COST)));
            match child_turn {
                true => sender.send_envelope(child(results_submitted())).unwrap(),
                false => sender.try_send(results_submitted()),
            }
        });

        let updates = usage_updates(&out_rx);
        let flagged = updates
            .iter()
            .filter(|update| !update["_meta"].is_null() && wire(update).results_submitted)
            .count();
        assert_eq!(flagged, usize::from(sent));
        assert_eq!(updates.len(), 1 + usize::from(sent));
    }

    #[test]
    fn without_the_extension_usage_is_standard_only() {
        let (srv, .., out_rx) = test_server();
        run_pump(&srv, None, |sender| {
            sender.try_send(AgentEvent::TurnComplete(main_turn(TURN_COST)));
        });

        let [update] = &usage_updates(&out_rx)[..] else {
            panic!("expected one usage update");
        };
        assert!(update["_meta"].is_null());
        assert_eq!(update["cost"]["amount"].as_f64(), Some(TURN_COST));
    }

    /// The test server with live input and save channels, as a session has.
    fn settings_server() -> (
        Server,
        flume::Receiver<SessionInput>,
        flume::Receiver<()>,
        flume::Receiver<Value>,
    ) {
        let (mut srv, _answer_rx, out_rx) = test_server();
        let (input_tx, input_rx) = flume::unbounded();
        let (saved_tx, saved_rx) = flume::bounded(1);
        let session = fixture_session_mut(&mut srv);
        session.handle.input_tx = input_tx;
        session.handle.settings_saved = saved_tx;
        srv.extensions.settings = true;
        (srv, input_rx, saved_rx, out_rx)
    }

    fn sent_prompt(input_rx: &flume::Receiver<SessionInput>) -> AgentInput {
        match input_rx.try_recv().unwrap() {
            SessionInput::Prompt(input) => input,
            SessionInput::Compact(_) => panic!("expected a prompt"),
        }
    }

    fn prompt_with(srv: &Server, settings: Option<&settings::WireSettings>) -> Value {
        text_prompt(srv, PROMPT_TEXT, settings)
    }

    fn text_prompt(srv: &Server, text: &str, settings: Option<&settings::WireSettings>) -> Value {
        let mut params = serde_json::json!({ "sessionId": fixture_session(srv).handle.session_id.as_str(), "prompt": [{"type": "text", "text": text}] });
        if let Some(settings) = settings {
            params["_meta"] = serde_json::to_value(settings.meta()).unwrap();
        }
        serde_json::json!({ "params": params })
    }

    fn set_config(srv: &Server, id: &str, value: Value) -> Value {
        serde_json::json!({ "params": { "sessionId": fixture_session(srv).handle.session_id.as_str(), "configId": id, "value": value,
            "type": if value.is_boolean() { "boolean" } else { "value_id" } } })
    }

    fn intent(srv: &Server) -> SessionSettings {
        (*fixture_session(srv).settings.load_full()).clone()
    }

    /// The prompt's own copy runs it, as a local `AgentInput` does, and the
    /// session's intent and every client follow.
    #[test]
    fn a_prompts_settings_build_its_input() {
        let (srv, input_rx, _saved_rx, out_rx) = settings_server();
        let wire = settings::WireSettings::new(ThinkingConfig::Adaptive, true, true);

        block_on(handle_prompt(
            &srv,
            &prompt_with(&srv, Some(&wire)),
            &RequestId::Number(PROMPT_ID),
        ))
        .unwrap();

        let input = sent_prompt(&input_rx);
        assert_eq!(
            (input.thinking, input.fast, input.workflow),
            (ThinkingConfig::Adaptive, true, true)
        );
        assert_eq!(intent(&srv).thinking, Some(StoredThinking::Adaptive));
        let update = out_rx.try_recv().unwrap();
        assert_eq!(
            update["params"]["update"]["sessionUpdate"],
            "config_option_update"
        );
    }

    #[test]
    fn a_prompt_without_settings_runs_on_the_sessions_intent() {
        let (srv, input_rx, _saved_rx, _out_rx) = settings_server();
        fixture_session(&srv)
            .settings
            .store(Arc::new(SessionSettings {
                fast: true,
                ..SessionSettings::default()
            }));

        block_on(handle_prompt(
            &srv,
            &prompt_with(&srv, None),
            &RequestId::Number(PROMPT_ID),
        ))
        .unwrap();

        assert!(sent_prompt(&input_rx).fast);
    }

    /// Yolo reaches the permission checks of a running turn at once, and an
    /// idle change wakes the driver to save it.
    #[test]
    fn yolo_applies_at_once_and_is_saved() {
        let (mut srv, _input_rx, saved_rx, _out_rx) = settings_server();

        let request = set_config(&srv, settings::YOLO_CONFIG_ID, serde_json::json!(true));
        let response =
            block_on(handle_set_config(&mut srv, &request, Timeouts::default())).unwrap();

        assert!(fixture_session(&srv).handle.permissions.is_yolo());
        assert_eq!(intent(&srv).yolo, Some(true));
        assert!(saved_rx.try_recv().is_ok(), "the driver is woken to save");
        let json = serde_json::to_value(response).unwrap();
        let yolo = json["configOptions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|option| option["id"] == settings::YOLO_CONFIG_ID)
            .unwrap()
            .clone();
        assert_eq!(yolo["currentValue"], true);
    }

    /// A load gives back what the session last chose, as a local load does.
    #[test_case(None, false ; "old_session")]
    #[test_case(Some(StoredMode::Build), true ; "build_with_saved_plan")]
    #[test_case(Some(StoredMode::Plan), true ; "plan_with_saved_path")]
    #[test_case(Some(StoredMode::Plan), false ; "plan_without_saved_path")]
    fn a_load_restores_the_stored_settings(mode: Option<StoredMode>, saved_plan: bool) {
        let tmp = TempDir::new().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut stored: StoredSession = Session::new(SELECTED_SPEC, "/project");
        stored.push_message(Message::user(QUEUED_TEXT.into()));
        stored.meta.thinking = Some(StoredThinking::Adaptive);
        stored.meta.fast = true;
        stored.meta.workflow = true;
        stored.meta.yolo = Some(true);
        stored.meta.mode = mode;
        let path = tmp.path().join(PLAN_FILE);
        if saved_plan {
            stored.meta.plan_path = Some(path.to_string_lossy().into_owned());
            fs::write(&path, PLAN_TEXT).unwrap();
        }
        save_fixture(&mut stored, &storage);
        let (mut srv, ..) = test_server();

        let mut params = acp_params(storage);
        params.background = true;
        let response = block_on(load_session(&mut srv, &load_request(stored.id), &params)).unwrap();

        let resumed = running_session(&srv, &SessionId::from(stored.id.to_string())).unwrap();
        assert_eq!(*resumed.settings.load_full(), SessionSettings::of(&stored));
        assert!(resumed.handle.permissions.is_yolo());
        let planning = mode == Some(StoredMode::Plan);
        let AgentResponse::LoadSessionResponse(response) = response else {
            unreachable!()
        };
        assert_eq!(
            response.modes.unwrap().current_mode_id.to_string(),
            if planning {
                MODE_PLAN
            } else {
                methods::MODE_BUILD
            }
        );
        assert_eq!(resumed.current_mode.plan_path().is_some(), planning);
        let seed = maki_agent::AgentSeed::live(stored.id).unwrap();
        assert_eq!(seed.chat_mode(), resumed.current_mode);
        let kept = resumed.plan_path.lock().unwrap();
        if saved_plan {
            assert_eq!(kept.as_ref(), Some(&path));
            assert_eq!(
                read_plan(kept.as_ref().unwrap()).unwrap().text.as_deref(),
                Some(PLAN_TEXT)
            );
        } else if planning {
            assert!(kept.as_ref().unwrap().starts_with(tmp.path()));
        } else {
            assert!(kept.is_none());
        }
    }

    /// A prompt refused while another runs must leave the settings alone,
    /// and a compaction is admitted like one.
    #[test_case(PROMPT_TEXT ; "a_prompt")]
    #[test_case(COMPACT_PROMPT ; "a_compaction")]
    fn a_rejected_prompt_changes_no_settings(text: &str) {
        let (srv, input_rx, _saved_rx, out_rx) = settings_server();
        fixture_session(&srv).pending.lock().unwrap().prompt = Some(RequestId::Number(PROMPT_ID));
        let wire = settings::WireSettings::new(ThinkingConfig::Adaptive, true, true);

        let result = block_on(handle_prompt(
            &srv,
            &text_prompt(&srv, text, Some(&wire)),
            &RequestId::Number(1),
        ));

        assert!(result.is_err());
        assert_eq!(intent(&srv), SessionSettings::default());
        assert!(out_rx.is_empty());
        assert!(input_rx.is_empty());
    }

    /// The workflow it was queued under applies to that compaction only, so
    /// the session's intent and every client stay as they are.
    #[test]
    fn a_compact_prompt_runs_a_compaction_under_its_carried_workflow() {
        let (srv, input_rx, _saved_rx, out_rx) = settings_server();
        let wire = settings::WireSettings::new(ThinkingConfig::Adaptive, true, true);

        block_on(handle_prompt(
            &srv,
            &text_prompt(&srv, COMPACT_PROMPT, Some(&wire)),
            &RequestId::Number(PROMPT_ID),
        ))
        .unwrap();

        let SessionInput::Compact(compaction) = input_rx.try_recv().unwrap() else {
            panic!("expected a compaction");
        };
        assert_eq!(
            compaction.instructions.as_deref(),
            Some(COMPACT_INSTRUCTIONS)
        );
        assert!(compaction.workflow);
        assert_eq!(intent(&srv), SessionSettings::default());
        assert!(out_rx.is_empty());
        let pending = fixture_session(&srv).pending.lock().unwrap();
        assert_eq!(pending.prompt, Some(RequestId::Number(PROMPT_ID)));
    }

    fn btw_request(session: &str, question: &str) -> Value {
        serde_json::json!({ "params": { "sessionId": session, "question": question } })
    }

    /// A question with nothing in it, or for a session this agent does not
    /// run, is refused at once, with no side task left behind.
    #[test_case(SESSION_UNDER_TEST, "  ", -32602 ; "empty_question")]
    #[test_case("another-session", PROMPT_TEXT, -32002 ; "another_session")]
    fn a_side_question_that_cannot_run_is_refused(session: &str, question: &str, code: i64) {
        let (mut srv, _answer_rx, out_rx) = test_server();
        let session = match session {
            SESSION_UNDER_TEST => fixture_session(&srv).handle.session_id.to_string(),
            other => other.to_owned(),
        };
        let tmp = TempDir::new().unwrap();
        block_on(handle_request(
            &mut srv,
            btw::BTW_METHOD,
            RequestId::Number(PROMPT_ID),
            &btw_request(&session, question),
            &mut acp_params(StateDir::from_path(tmp.path().to_path_buf())),
        ));
        let answer = out_rx.try_recv().unwrap();
        assert_eq!(answer["id"], PROMPT_ID);
        assert_eq!(answer["error"]["code"], code);
        assert!(srv.side_tasks.is_empty());
    }

    /// Answers with two deltas, so the order of what goes out shows.
    struct TwoDeltas;

    impl Provider for TwoDeltas {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            event_tx: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                for text in [PROMPT_TEXT, QUEUED_TEXT] {
                    let _ = event_tx.send(ProviderEvent::TextDelta { text: text.into() });
                }
                Ok(StreamResponse::default())
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[test]
    fn every_chunk_goes_out_before_the_answer() {
        let (out_tx, out_rx) = flume::unbounded();
        let model = Model::from_spec(SELECTED_SPEC).unwrap();
        let connect = async { Ok((Arc::new(TwoDeltas) as Arc<dyn Provider>, model)) };
        block_on(answer_btw(
            out_tx,
            RequestId::Number(PROMPT_ID),
            connect,
            None,
            PROMPT_TEXT.into(),
            SessionRef::from(MakiId::generate()),
        ));
        let sent: Vec<Value> = out_rx.try_iter().collect();
        let [first, second, answer] = &sent[..] else {
            panic!("expected two chunks and the answer: {sent:?}");
        };
        for (chunk, text) in [(first, PROMPT_TEXT), (second, QUEUED_TEXT)] {
            assert_eq!(chunk["method"], btw::CHUNK_METHOD);
            assert_eq!(chunk["params"]["requestId"], PROMPT_ID);
            assert_eq!(chunk["params"]["text"], text);
        }
        assert_eq!(answer["id"], PROMPT_ID);
        assert_eq!(answer["result"], serde_json::json!({}));
    }

    /// A provider that never connects holds only its own task, and the
    /// server's end, which cancels the task, lets the writer's sender go.
    #[test]
    fn a_dropped_side_task_releases_the_output() {
        let (out_tx, out_rx) = flume::unbounded();
        let connect = future::pending::<Result<(Arc<dyn Provider>, Model), AgentError>>();
        let task = spawn(answer_btw(
            out_tx,
            RequestId::Number(PROMPT_ID),
            connect,
            None,
            PROMPT_TEXT.into(),
            SessionRef::from(MakiId::generate()),
        ));
        assert!(!out_rx.is_disconnected());
        task.abort();
        assert!(block_on(task).unwrap_err().is_cancelled());
        assert!(out_rx.is_disconnected());
        assert!(out_rx.is_empty(), "nothing is answered for a dropped task");
    }

    /// A load seeds the ledger from the file, so `/usage` shows the stored
    /// rows before the next turn.
    #[test]
    fn a_load_seeds_the_ledger_from_the_file() {
        let tmp = TempDir::new().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut stored: StoredSession = Session::new(SELECTED_SPEC, "/project");
        stored.push_message(Message::user(QUEUED_TEXT.into()));
        let spent = TokenUsage {
            input: CHILD_INPUT,
            ..TokenUsage::default()
        };
        stored.token_usage = spent;
        stored.add_model_usage(CHILD_MODEL, spent.billed(Some(SUBAGENT_COST)));
        save_fixture(&mut stored, &storage);
        let (mut srv, _answer_rx, out_rx) = test_server();
        srv.extensions = Agreed {
            usage: true,
            usage_ledger: true,
            ..Agreed::default()
        };

        block_on(load_session(
            &mut srv,
            &load_request(stored.id),
            &acp_params(storage),
        ))
        .unwrap();

        let updates = usage_updates(&out_rx);
        let ledger = wire(updates.last().unwrap()).ledger.unwrap();
        assert_eq!(ledger.total.input, CHILD_INPUT);
        assert_eq!(ledger.by_model[CHILD_MODEL].cost, Some(SUBAGENT_COST));
    }

    #[test]
    fn a_quota_for_another_session_is_refused() {
        let (mut srv, _answer_rx, _out_rx) = test_server();
        let request = serde_json::json!({ "params": { "sessionId": "another-session" } });
        let result = handle_quota(
            &mut srv,
            &request,
            &RequestId::Number(PROMPT_ID),
            Timeouts::default(),
        );
        assert!(result.is_err());
        assert!(srv.side_tasks.is_empty());
    }

    /// The terminal login is offered only to a client that can run it and
    /// will say it ran, or the agent would stay on its old credentials.
    #[test_case(true, true, true ; "terminal_and_login")]
    #[test_case(true, false, false ; "terminal_without_login")]
    #[test_case(false, true, false ; "login_without_terminal")]
    fn the_terminal_login_needs_both_sides(terminal: bool, login_agreed: bool, offered: bool) {
        let (mut srv, _answer_rx, out_rx) = test_server();
        let tmp = TempDir::new().unwrap();
        let params = acp_params(StateDir::from_path(tmp.path().to_path_buf()));
        let mut request = serde_json::json!({"params": {"protocolVersion": 1,
            "clientCapabilities": {"auth": {"terminal": terminal}}}});
        if login_agreed {
            request["params"]["_meta"] =
                serde_json::json!({ META_KEY: { login::LOGIN_KEY: login::LOGIN_VERSION } });
        }
        block_on(handle_request(
            &mut srv,
            "initialize",
            RequestId::Number(PROMPT_ID),
            &request,
            &mut params.clone(),
        ));
        let methods = out_rx.try_recv().unwrap()["result"]["authMethods"].clone();
        let listed = methods.as_array().is_some_and(|methods| {
            methods.iter().any(|method| {
                method["id"] == login::METHOD_ID
                    && method["type"] == "terminal"
                    && method["args"][0] == login::LOGIN_FLAG
            })
        });
        assert_eq!(listed, offered);
    }

    const PLAN_FILE: &str = "plan.md";
    const PLAN_TEXT: &str = "# Plan\n- one";
    const EDITED: &str = "# Plan\n- one\n- two";

    #[test_case(true, true ; "state_dir")]
    #[test_case(false, true ; "private_fallback")]
    #[test_case(false, false ; "no_writable_plan")]
    fn queued_planning_keeps_its_write_boundary(state_writable: bool, scratch_writable: bool) {
        let tmp = TempDir::new().unwrap();
        let blocked = tmp.path().join(PLAN_FILE);
        fs::write(&blocked, PLAN_TEXT).unwrap();
        let storage =
            StateDir::from_path(if state_writable { tmp.path() } else { &blocked }.to_path_buf());
        let scratch = if scratch_writable {
            tmp.path()
        } else {
            &blocked
        };
        let plan = PlanPath::default();
        let AgentMode::Plan(path) = plan_mode(&plan, &storage, scratch) else {
            panic!("planning must not fall back to build mode")
        };
        assert_eq!(path.as_os_str().is_empty(), !scratch_writable);
        if scratch_writable {
            assert_eq!(
                plan_mode(&plan, &storage, &blocked).plan_path(),
                Some(path.as_path())
            );
            assert!(path.starts_with(tmp.path()));
        }
    }

    #[test]
    fn a_switch_to_plan_keeps_the_sessions_plan_file() {
        let (mut srv, _answer_rx, _out_rx) = test_server();
        let kept = PathBuf::from(STEER_PLAN);
        *fixture_session(&srv).plan_path.lock().unwrap() = Some(kept.clone());
        for mode in [MODE_PLAN, methods::MODE_BUILD, MODE_PLAN] {
            let request = json!({"params": {"sessionId": fixture_session(&srv).handle.session_id.as_str(), "modeId": mode}});
            block_on(handle_set_mode(&mut srv, &request)).unwrap();
            assert_eq!(
                fixture_session(&srv).current_mode.plan_path(),
                (mode == MODE_PLAN).then_some(kept.as_path())
            );
        }
    }

    fn plan_done(tool: &str, written: &Path, is_error: bool) -> ToolDoneEvent {
        ToolDoneEvent {
            id: "call-plan".to_owned(),
            tool: tool.into(),
            output: Arc::new(maki_agent::ToolOutput::Plain(String::new().into())),
            is_error,
            annotation: None,
            written_path: Some(written.display().to_string()),
            backgrounded: false,
            call: None,
            data: None,
        }
    }

    /// A full write carries its text, an edit only the path, in any mode;
    /// another file, a failed call, or a session without a plan file carries
    /// nothing.
    #[test_case(WRITE_TOOL_NAME, true, false, true, Some(Some(PLAN_TEXT)) ; "full_write")]
    #[test_case("edit", true, false, true, Some(None) ; "edit")]
    #[test_case(WRITE_TOOL_NAME, false, false, true, None ; "another_file")]
    #[test_case(WRITE_TOOL_NAME, true, true, true, None ; "failed")]
    #[test_case(WRITE_TOOL_NAME, true, false, false, None ; "no_plan_file")]
    fn only_a_plan_write_is_marked(
        tool: &str,
        to_plan: bool,
        is_error: bool,
        has_plan: bool,
        expected: Option<Option<&str>>,
    ) {
        let tmp = TempDir::new().unwrap();
        let plan_file = tmp.path().join(PLAN_FILE);
        fs::write(&plan_file, PLAN_TEXT).unwrap();
        let other = tmp.path().join("other.md");
        let run = PlanPath::new(Mutex::new(has_plan.then(|| plan_file.clone())));
        let written = if to_plan { &plan_file } else { &other };
        let marked = plan_written(Some(&run), &plan_done(tool, written, is_error));
        assert_eq!(
            marked.map(|marked| marked.text),
            expected.map(|text| text.map(str::to_owned))
        );
    }

    #[test]
    fn a_plan_past_the_limit_is_named_without_its_text() {
        let tmp = TempDir::new().unwrap();
        let plan_file = tmp.path().join(PLAN_FILE);
        fs::write(&plan_file, "x".repeat(PLAN_TEXT_LIMIT + 1)).unwrap();
        let run = PlanPath::new(Mutex::new(Some(plan_file.clone())));
        let marked = plan_written(Some(&run), &plan_done(WRITE_TOOL_NAME, &plan_file, false));
        assert_eq!(marked.unwrap().text, None);
    }

    /// A server whose session plans in `dir`, and that session's id.
    fn planning_server(dir: &Path) -> (Server, flume::Receiver<Value>, String) {
        let (mut srv, _answer_rx, out_rx) = test_server();
        let session = fixture_session_mut(&mut srv);
        *session.plan_path.lock().unwrap() = Some(dir.join(PLAN_FILE));
        let id = session.handle.session_id.to_string();
        (srv, out_rx, id)
    }

    fn plan_request(
        srv: &mut Server,
        out_rx: &flume::Receiver<Value>,
        method: &str,
        params: Value,
    ) -> Value {
        let tmp = TempDir::new().unwrap();
        let acp = acp_params(StateDir::from_path(tmp.path().to_path_buf()));
        block_on(handle_request(
            srv,
            method,
            RequestId::Number(PROMPT_ID),
            &json!({ "params": params }),
            &mut acp.clone(),
        ));
        out_rx.try_recv().unwrap()
    }

    #[test]
    fn a_plan_is_read_and_written_at_its_revision() {
        let tmp = TempDir::new().unwrap();
        let (mut srv, out_rx, sid) = planning_server(tmp.path());
        let before = plan_request(
            &mut srv,
            &out_rx,
            plan::READ_METHOD,
            json!({ "sessionId": sid }),
        );
        assert_eq!(before["result"]["text"], Value::Null);
        assert_eq!(before["result"]["revision"], Value::Null);

        let written = plan_request(
            &mut srv,
            &out_rx,
            plan::WRITE_METHOD,
            json!({ "sessionId": sid, "text": PLAN_TEXT, "revision": null }),
        );
        assert_eq!(written["result"], json!({}));
        let read = plan_request(
            &mut srv,
            &out_rx,
            plan::READ_METHOD,
            json!({ "sessionId": sid }),
        );
        assert_eq!(read["result"]["text"], PLAN_TEXT);
        assert_eq!(read["result"]["revision"], plan::revision(PLAN_TEXT));

        let stale = plan_request(
            &mut srv,
            &out_rx,
            plan::WRITE_METHOD,
            json!({ "sessionId": sid, "text": EDITED, "revision": null }),
        );
        assert_eq!(stale["error"]["data"], CONFLICT);
        let saved = plan_request(
            &mut srv,
            &out_rx,
            plan::WRITE_METHOD,
            json!({ "sessionId": sid, "text": EDITED, "revision": plan::revision(PLAN_TEXT) }),
        );
        assert_eq!(saved["result"], json!({}));
        assert_eq!(
            fs::read_to_string(tmp.path().join(PLAN_FILE)).unwrap(),
            EDITED
        );
    }

    #[test_case(true ; "no_plan_file")]
    #[test_case(false ; "another_session")]
    fn a_plan_request_needs_the_sessions_own_plan(no_plan: bool) {
        let tmp = TempDir::new().unwrap();
        let (mut srv, out_rx, sid) = planning_server(tmp.path());
        if no_plan {
            *fixture_session(&srv).plan_path.lock().unwrap() = None;
        }
        let named = if no_plan {
            sid
        } else {
            "another-session".to_owned()
        };
        for method in [plan::READ_METHOD, plan::WRITE_METHOD] {
            let answer = plan_request(
                &mut srv,
                &out_rx,
                method,
                json!({ "sessionId": named, "text": EDITED, "revision": null }),
            );
            assert!(answer.get("error").is_some(), "{method}: {answer}");
        }
        assert!(!tmp.path().join(PLAN_FILE).exists());
    }

    /// A login rebuilds what the old credentials connected, as the local
    /// `RefreshProvider` and `RefreshModels` do: the driver, the session's
    /// options, and a fresh listing no older discovery can extend.
    #[test]
    fn a_login_rebuilds_the_session_and_its_listing() {
        let (mut srv, _answer_rx, out_rx) = test_server();
        let (rebuild_tx, rebuild_rx) = flume::bounded(1);
        fixture_session_mut(&mut srv).handle.rebuild_tx = rebuild_tx;
        srv.model_specs.push(DISCOVERED_SPEC.into());
        let tmp = TempDir::new().unwrap();
        let params = acp_params(StateDir::from_path(tmp.path().to_path_buf()));

        block_on(auth_changed(&mut srv, &params));

        assert!(rebuild_rx.try_recv().is_ok(), "the driver rebuilds");
        assert_eq!(srv.discovery, 1);
        assert!(!srv.model_specs.contains(&DISCOVERED_SPEC.to_owned()));
        let sent: Vec<Value> = out_rx.try_iter().collect();
        assert!(
            sent.iter()
                .any(|message| message["params"]["update"]["sessionUpdate"]
                    == "config_option_update")
        );
        block_on(refresh_models(&mut srv, 0, vec![DISCOVERED_SPEC.into()]));
        assert!(
            !srv.model_specs.contains(&DISCOVERED_SPEC.to_owned()),
            "a late batch of the older discovery is ignored"
        );
    }

    /// The client binds the session as it reads the answer, so the list goes
    /// after it.
    #[test]
    fn the_command_list_follows_the_load_answer() {
        let tmp = TempDir::new().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut stored: StoredSession = Session::new(SELECTED_SPEC, "/project");
        stored.push_message(Message::user(QUEUED_TEXT.into()));
        save_fixture(&mut stored, &storage);
        let (mut srv, _answer_rx, out_rx) = test_server();

        block_on(handle_request(
            &mut srv,
            "session/load",
            RequestId::Number(PROMPT_ID),
            &load_request(stored.id),
            &mut acp_params(storage),
        ));

        let sent: Vec<Value> = out_rx.try_iter().collect();
        let answer = sent
            .iter()
            .position(|message| message["id"] == PROMPT_ID)
            .expect("the load is answered");
        let commands = sent
            .iter()
            .position(|message| {
                message["params"]["update"]["sessionUpdate"] == "available_commands_update"
            })
            .expect("the commands are advertised");
        assert!(answer < commands);
        assert_eq!(
            sent[commands]["params"]["update"]["availableCommands"][0]["name"],
            commands::COMPACT
        );
    }

    #[test]
    fn a_change_to_a_session_whose_driver_ended_is_refused() {
        let (mut srv, _input_rx, saved_rx, _out_rx) = settings_server();
        drop(saved_rx);

        let request = set_config(&srv, settings::FAST_CONFIG_ID, serde_json::json!(true));
        let result = block_on(handle_set_config(&mut srv, &request, Timeouts::default()));

        assert!(result.is_err());
        assert!(
            !intent(&srv).fast,
            "nothing that cannot be saved is accepted"
        );
    }

    #[test_case(settings::THINKING_CONFIG_ID, serde_json::json!("not a level") ; "unreadable_thinking")]
    #[test_case(settings::FAST_CONFIG_ID, serde_json::json!("yes") ; "fast_not_a_boolean")]
    #[test_case("colour", serde_json::json!(true) ; "unknown_option")]
    fn a_bad_setting_is_invalid_params(id: &str, value: Value) {
        let (mut srv, ..) = settings_server();
        let request = set_config(&srv, id, value);
        let error =
            block_on(handle_set_config(&mut srv, &request, Timeouts::default())).unwrap_err();
        assert_eq!(error.code, AcpError::invalid_params().code);
    }

    /// A run's `Done` or `Error` carries its ledger, which also bills
    /// compaction and moved work, so it replaces what the run's turns added
    /// up to.
    #[test_case(false ; "a_finished_run")]
    #[test_case(true ; "a_failed_run")]
    #[tokio::test]
    async fn event_pump_settles_a_run_on_its_ledger(failed: bool) {
        let (srv, .., out_rx) = test_server();
        let ledger = RunTotals {
            cost: Some(LEDGER_COST),
            ..RunTotals::default()
        };
        let ending = if failed {
            AgentEvent::Error {
                message: TURN_ERROR.to_owned(),
                auth: false,
                totals: ledger,
            }
        } else {
            AgentEvent::Done {
                usage: ledger.usage,
                cost: ledger.cost,
                list_cost: None,
                context_size: 0,
                context_window: CONTEXT_WINDOW,
                num_turns: 1,
                reason: DoneReason::EndTurn,
            }
        };
        run_pump(&srv, Some(RECORDED_COST), |sender| {
            sender
                .send(AgentEvent::TurnComplete(turn_complete(TURN_COST)))
                .unwrap();
            sender.send(ending).unwrap();
            sender
                .send(AgentEvent::TurnComplete(turn_complete(TURN_COST)))
                .unwrap();
        });

        let last = out_rx.try_iter().last().expect("the next turn reports");
        assert_eq!(
            last["params"]["update"]["cost"]["amount"].as_f64(),
            Some(RECORDED_COST + LEDGER_COST + TURN_COST)
        );
    }

    /// Results ride the next prompt and nothing wakes a turn, so a session
    /// closed with work out stops it rather than waiting on it. The task that
    /// never ends stands in for that work.
    #[tokio::test]
    async fn close_session_returns_at_once_with_background_work_out() {
        let (mut srv, ..) = test_server();
        fixture_session_mut(&mut srv).handle.task = spawn(future::pending());
        let sid = fixture_session(&srv).handle.session_id.to_string();

        let closed = timeout(
            STDIN_DEADLOCK_TIMEOUT,
            close_session(&mut srv, &SessionId::from(sid), SessionEndReason::Shutdown),
        )
        .await
        .is_ok();

        assert!(closed, "{CLOSE_WAITED}");
    }

    #[tokio::test]
    async fn close_session_awaits_the_session_end_hook() {
        let (mut srv, ..) = test_server();
        let ended = fixture_session(&srv).handle.session_id.id();
        let (ended_tx, ended_rx) = flume::bounded(1);
        srv.on_session_end = Some(Arc::new(move |id, reason| {
            let ended_tx = ended_tx.clone();
            Box::pin(async move {
                let _ = ended_tx.send((id, reason));
            })
        }));

        block_on(close_session(
            &mut srv,
            &SessionId::from(ended.to_string()),
            SessionEndReason::Replaced,
        ));

        assert_eq!(
            ended_rx.try_recv().ok(),
            Some((ended, SessionEndReason::Replaced))
        );
        assert!(srv.sessions.is_empty(), "close must take the session");
    }

    /// JSON-RPC ids are `string | number`, and a client is free to echo ours
    /// back in either shape. An answer dropped for its shape leaves the tool
    /// parked forever.
    #[test_case(Value::from(ANSWERED_ID), true ; "a_numeric_id_answers_the_ask")]
    #[test_case(Value::from(ANSWERED_ID.to_string()), true ; "a_string_id_answers_the_same_ask")]
    #[test_case(Value::from(UNANSWERABLE_ID), false ; "an_id_matching_no_ask_answers_nothing")]
    #[tokio::test]
    async fn a_response_is_delivered_whatever_shape_its_id_has(id: Value, delivered: bool) {
        let (srv, answer_rx, ..) = server_with_ask(permission_ask(None));

        handle_incoming_response(
            &srv,
            &serde_json::json!({
                "id": id,
                "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } },
            }),
        );

        let answered = TaggedAnswer::new(PARENT_TOOL_USE_ID, PermissionAnswer::AllowOnce).encode();
        assert_eq!(answer_rx.try_recv().ok(), delivered.then_some(answered));
        assert_eq!(
            pending(&srv)
                .lock()
                .unwrap()
                .asks
                .contains_key(&ANSWERED_ID),
            !delivered,
            "only the ask that was answered is retired"
        );
    }

    #[tokio::test]
    async fn only_the_outstanding_request_id_is_answered() {
        let (srv, answer_rx, ..) = server_with_ask(permission_ask(None));

        handle_incoming_response(&srv, &allow_once(UNKNOWN_ID));
        assert!(answer_rx.is_empty(), "an unknown id is dropped");

        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
        assert_eq!(
            answer_rx.try_recv().ok(),
            Some(TaggedAnswer::new(PARENT_TOOL_USE_ID, PermissionAnswer::AllowOnce).encode())
        );

        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
        assert!(
            answer_rx.is_empty(),
            "a replayed answer cannot land on the next request"
        );
    }

    #[tokio::test]
    async fn elicitation_response_forwards_the_raw_result() {
        let (srv, answer_rx, ..) = server_with_ask(Ask::Elicitation);
        let raw = serde_json::json!({
            "id": ANSWERED_ID,
            "result": { "action": "accept", "content": { "q1": "axum" } },
        });

        handle_incoming_response(&srv, &raw);
        let forwarded = answer_rx.try_recv().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&forwarded).unwrap(),
            raw["result"]
        );
    }

    #[tokio::test]
    async fn discovered_models_are_pushed_to_the_client() {
        let (mut srv, .., out_rx) = test_server();
        srv.model_specs = vec![OFFLINE_SPEC.to_owned()];
        let batch = vec![DISCOVERED_SPEC.to_owned()];

        block_on(refresh_models(&mut srv, 0, batch.clone()));
        let update = out_rx.try_recv().expect("the fuller list is announced");
        let option = &update["params"]["update"]["configOptions"][0];
        assert_eq!(option["id"], methods::MODEL_CONFIG_ID);
        let selectable: Vec<&str> = option["options"]
            .as_array()
            .expect("the option is a select")
            .iter()
            .filter_map(|o| o["value"].as_str())
            .collect();
        assert!(
            selectable.contains(&OFFLINE_SPEC) && selectable.contains(&DISCOVERED_SPEC),
            "a batch is merged into the offline list, not swapped for it: {selectable:?}"
        );
        let ids: Vec<&str> = update["params"]["update"]["configOptions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|option| option["id"].as_str())
            .collect();
        assert!(
            ids.contains(&settings::YOLO_CONFIG_ID),
            "an update carries the whole set, so a client replacing its menu keeps the settings"
        );

        block_on(refresh_models(&mut srv, 0, batch));
        assert!(out_rx.is_empty(), "a batch adding nothing is not announced");
    }

    /// Resuming must bill what the session actually paid. If the per-model
    /// breakdown came back empty or lost its recorded costs, ACP would re-price
    /// the restored total against today's table and disagree with the TUI.
    #[test]
    fn load_history_prices_a_resumed_session_at_what_it_paid() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut session: StoredSession = Session::new(RETIRED_SPEC, "relative/project");
        session.token_usage = TokenUsage {
            input: 1_000_000,
            output: 200_000,
            ..Default::default()
        };
        session.add_model_usage(
            RETIRED_MODEL_ID,
            StoredTokenUsage {
                input: 1_000_000,
                output: 200_000,
                cost: Some(RECORDED_COST),
                ..Default::default()
            },
        );
        save_fixture(&mut session, &dir);

        let restored = claim_history(&dir, session.id).unwrap().0;
        assert_eq!(restored.cwd, None);
        let mut by_model = restored.session.usage_by_model().clone();
        assert_eq!(
            by_model[RETIRED_MODEL_ID].cost,
            Some(RECORDED_COST),
            "the per-model breakdown survives the file"
        );

        // Mirrors `load_session`: the recorded spec no longer parses, so the
        // selected model stands in, and that must not change the bill.
        let recorded_model = Model::from_spec(&restored.session.model)
            .unwrap_or_else(|_| Model::from_spec(SELECTED_SPEC).expect("a shipped model"));
        assert_eq!(
            settle_session(
                &restored.session.token_usage,
                &mut by_model,
                &recorded_model,
                false
            ),
            Some(RECORDED_COST)
        );
    }

    /// A session file that is there but cannot be read is no missing
    /// session: the answer carries the read error, so a client never
    /// replaces the conversation with a new one.
    #[test]
    fn load_unreadable_session_is_its_read_error() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut session: StoredSession = Session::new(SELECTED_SPEC, "/project");
        save_fixture(&mut session, &dir);
        let sessions = tmp.path().join(maki_storage::sessions::SESSIONS_DIR);
        for file in std::fs::read_dir(&sessions).unwrap() {
            let path = file.unwrap().path();
            if path.file_stem() == Some(session.id.to_string().as_ref()) {
                std::fs::write(path, CORRUPT_SESSION).unwrap();
            }
        }
        let err = claim_history(&dir, session.id).unwrap_err();
        assert_eq!(err.code, AcpError::internal_error().code);
        assert!(err.data.is_some(), "the answer carries the read error");
    }

    #[test]
    fn converts_injected_mcp_servers() {
        let raw = serde_json::json!({
            "params": {
                "sessionId": MakiId::generate().to_string(),
                "cwd": "/project",
                "mcpServers": [
                    {
                        "type": "http",
                        "name": "kan.dev/mcp",
                        "url": "http://127.0.0.1:41012",
                        "headers": [{ "name": "Authorization", "value": "Bearer abc" }]
                    },
                    {
                        "name": "local",
                        "command": "/usr/bin/mcp",
                        "args": ["--stdio"],
                        "env": [{ "name": "TOKEN", "value": "t" }]
                    },
                    {
                        "type": "sse",
                        "name": "legacy",
                        "url": "http://127.0.0.1:41013",
                        "headers": []
                    }
                ]
            }
        });

        let req: LoadSessionRequest = parse_params(&raw).unwrap();
        let servers = injected_servers(&req.mcp_servers);
        assert_eq!(servers.len(), 2, "sse is dropped, not converted");

        let (name, RawTransport::Http(http)) = &servers[0] else {
            panic!("expected http transport");
        };
        assert_eq!(name, "kan-dev-mcp", "wire names are coerced to valid ones");
        assert_eq!(http.url, "http://127.0.0.1:41012");
        assert_eq!(
            http.headers.get("Authorization").map(String::as_str),
            Some("Bearer abc")
        );

        let (name, RawTransport::Stdio(stdio)) = &servers[1] else {
            panic!("expected stdio transport");
        };
        assert_eq!(name, "local");
        assert_eq!(stdio.command, ["/usr/bin/mcp", "--stdio"]);
        assert_eq!(
            stdio.environment.get("TOKEN").map(String::as_str),
            Some("t")
        );
    }

    #[test]
    fn tool_data_survives_the_server_and_client_lifecycle() {
        const RAW: &str = "old\r\nnew\u{7}\u{1b}[31m";
        const TOOL: &str = "srv.probe";
        let mut data = ToolData::new(
            TOOL.into(),
            DEFAULT_DATA_VERSION,
            Value::Null,
            String::new(),
        );
        data.state = Value::Null;
        data.annotations = vec![RAW.into(), TURN_ERROR.into()];
        data.mark_rewritten();
        data.children.push(ToolChild {
            index: 0,
            status: ChildStatus::Done,
            data: ToolData::new(
                "bash".into(),
                DEFAULT_DATA_VERSION,
                json!({ "command": RAW }),
                RAW.into(),
            ),
            output: Some(Arc::new(ToolOutput::Plain(RAW.into()))),
            usage: Some(RAW.into()),
        });
        let events = vec![
            AgentEvent::ToolPending {
                id: STORED_CALL_ID.into(),
                name: "srv__probe".into(),
            },
            AgentEvent::ToolStart(Box::new(ToolStartEvent {
                id: STORED_CALL_ID.into(),
                tool: TOOL.into(),
                data_version: DEFAULT_DATA_VERSION,
                summary: RAW.into(),
                annotation: Some(RAW.into()),
                input: None,
                raw_input: Some(json!({ "command": RAW })),
                output: None,
            })),
            AgentEvent::ToolMovable {
                id: STORED_CALL_ID.into(),
                movable: true,
            },
            AgentEvent::ToolParts {
                id: STORED_CALL_ID.into(),
                parts: vec![ToolPart {
                    id: "part".into(),
                    name: "output".into(),
                    tool: "bash".into(),
                    status: PartStatus::Working,
                    body: Some(BufferSnapshot::plain_text(RAW.into())),
                }],
            },
            AgentEvent::ToolProgress(Box::new(ToolProgressEvent {
                id: STORED_CALL_ID.into(),
                path: vec![0],
                append: RAW.into(),
                state: Some(Value::Null),
                child: Some(Box::new(data.children[0].clone())),
            })),
            AgentEvent::ToolDone(Box::new(ToolDoneEvent {
                id: STORED_CALL_ID.into(),
                tool: TOOL.into(),
                output: Arc::new(ToolOutput::Plain(RAW.into())),
                is_error: true,
                annotation: Some(TURN_ERROR.into()),
                written_path: Some(STEER_PLAN.into()),
                backgrounded: true,
                call: None,
                data: Some(Arc::new(data)),
            })),
        ];
        let expected = serde_json::to_value(&events).unwrap();
        let (mut srv, _, out) = test_server();
        srv.extensions.tool_data = true;
        run_pump_with(&srv, with_extension(), |sender| {
            for event in events {
                sender.try_send(event);
            }
        });
        let mut client = DisplayMapper::new(&srv.extensions);
        let received: Vec<_> = out
            .try_iter()
            .flat_map(|mut message| {
                tool_data::sanitize_keeping_raw(&mut message);
                let notification: SessionNotification =
                    serde_json::from_value(message["params"].take()).unwrap();
                client.map(notification.update)
            })
            .collect();
        assert_eq!(serde_json::to_value(received).unwrap(), expected);
    }

    mod links {
        use std::pin::Pin;
        use std::task::{Context, Poll};
        use std::time::Duration;

        use agent_client_protocol_schema::v1::{AGENT_METHOD_NAMES, Implementation};
        use futures::future::BoxFuture;
        use maki_agent::prompt::ResolvedPrompts;
        use maki_providers::test_support::isolated_home;
        use test_case::test_case;
        use tokio::io::{DuplexStream, duplex, split};
        #[cfg(unix)]
        use tokio::net::UnixStream;

        use maki_wire::client::{AcpClient, ClientError, ClientEvent};

        use super::*;

        /// A failure guard only: every case ends on its own long before it.
        const CASE_TIMEOUT: Duration = Duration::from_secs(60);
        const BYTE_STREAM_CAPACITY: usize = 64 * 1024;
        const CASE_STALLED: &str = "the link case did not finish";
        const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
        const LINK_CLOSED_EARLY: &str = "the link closed before the reply";
        const SERVER_FAILED: &str = "the server must end cleanly";
        const LINK_LEFT_OPEN: &str = "the client must see its link close";
        const CLIENT_NAME: &str = "link-test";
        const CLIENT_VERSION: &str = "0";
        const RAW_ID: i64 = 41;
        const FOLLOWING_ID: i64 = 42;
        const UNKNOWN_METHOD: &str = "no/such_method";
        const MALFORMED_LINE: &str = "{not json";
        const STORED_TEXT: &str = "keep";
        #[cfg(unix)]
        const WRITE_FAILURE: &str = "the peer stopped reading";
        #[cfg(unix)]
        const WRITE_FAILURE_KIND: io::ErrorKind = io::ErrorKind::BrokenPipe;

        #[derive(Debug)]
        enum Kind {
            Memory,
            #[cfg(unix)]
            Stream,
        }

        type Serving = Pin<Box<dyn Future<Output = io::Result<()>>>>;
        type Ended = Receiver<(MakiId, SessionEndReason)>;

        /// Isolated from this machine's config and sessions. The home goes
        /// first, before anything reads the environment.
        struct Fixture {
            _home: TempDir,
            storage: TempDir,
            project: TempDir,
        }

        impl Fixture {
            fn new() -> Self {
                Self {
                    _home: isolated_home(),
                    storage: TempDir::new().unwrap(),
                    project: TempDir::new().unwrap(),
                }
            }

            fn storage(&self) -> StateDir {
                StateDir::from_path(self.storage.path().to_path_buf())
            }

            fn cwd(&self) -> PathBuf {
                self.project.path().to_path_buf()
            }

            fn params(&self) -> (AcpParams, Ended) {
                let (ended_tx, ended_rx) = flume::unbounded();
                let mut params = acp_params(self.storage());
                params.on_session_end = Some(Arc::new(move |id, reason| {
                    let ended_tx = ended_tx.clone();
                    Box::pin(async move {
                        let _ = ended_tx.send((id, reason));
                    })
                }));
                (params, ended_rx)
            }
        }

        /// The link a case talks over, the server behind it, and the pumps
        /// the link needs, which must outlive the case.
        fn connect(kind: Kind, params: AcpParams) -> (Link, Serving, Vec<Task<()>>) {
            match kind {
                Kind::Memory => {
                    let (link, server) = memory_link();
                    let serving = async move {
                        serve_channels(params, server.input, server.output).await;
                        Ok(())
                    };
                    (link, Box::pin(serving), Vec::new())
                }
                #[cfg(unix)]
                Kind::Stream => {
                    let (ours, theirs) = duplex(BYTE_STREAM_CAPACITY);
                    let (link, pumps) = stream_link(theirs);
                    let (reader, writer) = split(ours);
                    let serving = serve_stream(params, BufReader::new(reader), writer);
                    (link, Box::pin(serving), pumps)
                }
            }
        }

        /// The client's half of a byte stream, framed as `Link::spawn` frames
        /// a child's pipes. [`END_OF_INPUT`] closes the server's input, and the
        /// reader runs until the server closes its output.
        #[cfg(unix)]
        fn stream_link(stream: DuplexStream) -> (Link, Vec<Task<()>>) {
            let (tx, out_rx) = flume::unbounded::<String>();
            let (in_tx, rx) = flume::unbounded();
            let (stream, mut sink) = split(stream);
            let writer = spawn(async move {
                while let Ok(line) = out_rx.recv_async().await {
                    if line == END_OF_INPUT {
                        break;
                    }
                    if sink
                        .write_all(format!("{line}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                let _ = sink.shutdown().await;
            });
            let reader = spawn(async move {
                let _ = read_lines(BufReader::new(stream), in_tx).await;
            });
            (Link::new(tx, rx), vec![writer, reader])
        }

        async fn stalled<T>() -> T {
            tokio::time::sleep(CASE_TIMEOUT).await;
            panic!("{CASE_STALLED}");
        }

        /// Runs `case` beside the server on a fresh link of `kind`, and gives
        /// back how the server ended and what the case found.
        fn over<T>(
            kind: Kind,
            params: AcpParams,
            case: impl AsyncFnOnce(Link) -> T,
        ) -> (io::Result<()>, T) {
            block_on(async move {
                let (link, serving, _pumps) = connect(kind, params);
                future::or(future::zip(serving, case(link)), stalled()).await
            })
        }

        struct ClientRun<T> {
            served: io::Result<()>,
            closed: bool,
            found: T,
        }

        impl<T> ClientRun<T> {
            fn found(self) -> T {
                assert!(self.served.is_ok(), "{SERVER_FAILED}: {:?}", self.served);
                assert!(self.closed, "{LINK_LEFT_OPEN}");
                self.found
            }
        }

        /// Runs `case` through a real client, then shuts it down and reads
        /// its events to the end, so the server has sent its last line.
        fn with_client<T>(
            kind: Kind,
            params: AcpParams,
            case: impl AsyncFnOnce(&AcpClient) -> T,
        ) -> ClientRun<T> {
            let (served, (closed, found)) = over(kind, params, async |link| {
                let (client, events) = AcpClient::new(link);
                let found = case(&client).await;
                client.shutdown(SHUTDOWN_GRACE).await;
                let mut closed = false;
                while let Ok(event) = events.recv_async().await {
                    closed |= matches!(event, ClientEvent::Closed);
                }
                (closed, found)
            });
            ClientRun {
                served,
                closed,
                found,
            }
        }

        /// Writes `line` to the link and reads raw replies up to the one for
        /// `id`, since the client drops a reply no request waits on.
        async fn raw_reply(link: &Link, line: &str, id: &Value) -> Value {
            link.tx.send(line.to_owned()).unwrap();
            loop {
                let line = link.rx.recv_async().await.expect(LINK_CLOSED_EARLY);
                let message: Value = serde_json::from_str(line.trim()).unwrap();
                if message.get("id") == Some(id) {
                    return message;
                }
            }
        }

        async fn end_raw(link: &Link) {
            link.tx.send(END_OF_INPUT.to_owned()).unwrap();
            while link.rx.recv_async().await.is_ok() {}
        }

        async fn request(client: &AcpClient, operation: SessionOperation) -> Value {
            client.session_operation(operation).await.unwrap()
        }

        fn raw_request(id: i64, method: &str, params: Value) -> String {
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
        }

        #[cfg_attr(unix, test_case(Kind::Stream ; "byte_stream"))]
        #[test_case(Kind::Memory ; "in_memory")]
        fn initialize_answers_the_capabilities_and_extensions(kind: Kind) {
            let fixture = Fixture::new();
            let (params, _ended) = fixture.params();
            let answer = with_client(kind, params, async |client| {
                let info = Implementation::new(CLIENT_NAME, CLIENT_VERSION);
                client.initialize(info, false).await
            })
            .found()
            .unwrap();

            let expected = methods::initialize_response(false);
            assert_eq!(answer.agent_capabilities, expected.agent_capabilities);
            let mut expected_meta = extensions::request_meta();
            expected_meta
                .get_mut(META_KEY)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(plugins::KEY);
            assert_eq!(answer.meta, Some(expected_meta));
        }

        #[test]
        fn native_load_restores_task_transcripts_without_replaying_the_main_chat() {
            let fixture = Fixture::new();
            let storage = fixture.storage();
            let mut stored = StoredSession::new(SELECTED_SPEC, &fixture.cwd().to_string_lossy());
            stored.push_message(Message::user(PROMPT_TEXT.into()));
            stored.set_subagents(vec![StoredSubagent {
                tool_use_id: PARENT_TOOL_USE_ID.into(),
                parent_call: None,
                name: SUBAGENT_NAME.into(),
                model: None,
                thinking: None,
                fast: false,
                plugin: None,
                cost: None,
            }]);
            stored.set_subagent_messages(
                PARENT_TOOL_USE_ID.into(),
                vec![Message::user(QUEUED_TEXT.into())],
            );
            let id = stored.id;
            stored
                .save(&SessionClaim::acquire(id, &storage).unwrap(), &storage)
                .unwrap();
            let (mut params, _) = fixture.params();
            params.model.provider = "no-such-provider".into();
            let (served, ()) = over(Kind::Memory, params, async |link| {
                let (client, events) = AcpClient::new(link);
                client
                    .initialize(Implementation::new("test", "1"), false)
                    .await
                    .unwrap();
                let loaded = client
                    .load_session(&events, id.to_string().into(), fixture.cwd())
                    .await
                    .unwrap();
                let snapshot = loaded
                    .early
                    .into_iter()
                    .find_map(|event| match event {
                        ClientEvent::Restored(snapshot) => Some(snapshot),
                        _ => None,
                    })
                    .unwrap();
                let sid = client.bound_session().unwrap();
                let read: StoredSession = serde_json::from_value(
                    request(
                        &client,
                        SessionOperation::Snapshot {
                            session_id: sid.clone(),
                        },
                    )
                    .await,
                )
                .unwrap();
                assert_eq!(read.messages()[0].first_text_content(), Some(PROMPT_TEXT));
                let before = std::env::current_dir().unwrap();
                let next = fixture.cwd().join("next");
                std::fs::create_dir(&next).unwrap();
                let changed = request(
                    &client,
                    SessionOperation::ChangeDirectory {
                        session_id: sid.clone(),
                        path: "next".into(),
                    },
                )
                .await;
                assert_eq!(changed.as_str(), Some(next.to_str().unwrap()));
                assert_eq!(std::env::current_dir().unwrap(), before);
                let read: StoredSession = serde_json::from_value(
                    request(
                        &client,
                        SessionOperation::Snapshot {
                            session_id: sid.clone(),
                        },
                    )
                    .await,
                )
                .unwrap();
                assert_eq!(read.cwd, next.to_str().unwrap());
                assert!(
                    client
                        .session_operation(SessionOperation::ChangeDirectory {
                            session_id: sid,
                            path: "missing".into(),
                        })
                        .await
                        .is_err()
                );
                assert_eq!(snapshot.subagents()[0].name, SUBAGENT_NAME);
                assert_eq!(
                    snapshot.subagent_messages()[PARENT_TOOL_USE_ID][0].first_text_content(),
                    Some(QUEUED_TEXT)
                );
                assert!(
                    !loaded
                        .replay
                        .iter()
                        .any(|update| matches!(update, SessionUpdate::UserMessageChunk(_)))
                );
                client.shutdown(SHUTDOWN_GRACE).await;
                while events.recv_async().await.is_ok() {}
            });
            assert!(served.is_ok());
        }

        struct PausedPrompts(Receiver<()>);

        impl SessionPlugins for PausedPrompts {
            fn prompts(&self) -> BoxFuture<'static, ResolvedPrompts> {
                let ready = self.0.clone();
                Box::pin(async move {
                    ready.recv_async().await.unwrap();
                    ResolvedPrompts::default()
                })
            }
            fn observe(&self, _: MakiId, _: &Envelope) {}
            fn fire(&self, _: MakiId, _: &str, _: Value) -> Result<(), String> {
                Ok(())
            }
            fn stop_task(&self, _: &str) {}
        }

        #[test]
        fn a_plan_write_waits_for_an_enqueued_run() {
            let fixture = Fixture::new();
            let (mut params, _) = fixture.params();
            let (release, ready) = flume::bounded(1);
            params.plugins = Some(Arc::new(PausedPrompts(ready)));
            params.model.provider = "no-such-provider".into();
            let (served, ()) = over(Kind::Memory, params, async |link| {
                let (client, events) = AcpClient::new(link);
                let sid = client
                    .new_session(fixture.cwd())
                    .await
                    .unwrap()
                    .value
                    .session_id;
                client
                    .configure(maki_wire::configuration::Key::Mode, json!(MODE_PLAN))
                    .await
                    .unwrap();
                let plan = client.plan_read(sid.clone()).await.unwrap();
                let input = AgentInput::from_defaults(
                    PROMPT_TEXT.into(),
                    AgentMode::Plan(PathBuf::from(&plan.path)),
                    Vec::new(),
                    SessionDefaults::default(),
                    InputSource::Acp,
                );
                let receipt: QueueReceipt = serde_json::from_value(
                    request(
                        &client,
                        SessionOperation::Enqueue {
                            session_id: sid.clone(),
                            id: None,
                            input: Box::new(SessionInput::Prompt(input)),
                        },
                    )
                    .await,
                )
                .unwrap();
                loop {
                    if let ClientEvent::QueueUpdate(update) = events.recv_async().await.unwrap()
                        && update.receipt.id == receipt.id
                        && update.receipt.status == QueueStatus::Running
                    {
                        break;
                    }
                }
                let error = client
                    .plan_write(sid.clone(), EDITED.into(), plan.revision)
                    .await
                    .unwrap_err();
                let ClientError::Rpc(error) = error else {
                    panic!("{error}")
                };
                assert_eq!(error.data, Some(json!(BUSY)));
                assert_eq!(client.plan_read(sid.clone()).await.unwrap().text, None);
                release.send(()).unwrap();
                loop {
                    if let ClientEvent::QueueUpdate(update) = events.recv_async().await.unwrap()
                        && update.receipt.id == receipt.id
                        && update.receipt.status == QueueStatus::Error
                    {
                        break;
                    }
                }
                client
                    .plan_write(sid.clone(), EDITED.into(), None)
                    .await
                    .unwrap();
                assert_eq!(
                    client.plan_read(sid).await.unwrap().text.as_deref(),
                    Some(EDITED)
                );
                client.shutdown(SHUTDOWN_GRACE).await;
                while events.recv_async().await.is_ok() {}
            });
            assert!(served.is_ok());
        }

        #[test]
        fn accepted_queue_reports_a_durable_failure_without_a_live_prompt() {
            let fixture = Fixture::new();
            let (mut params, _ended) = fixture.params();
            params.model.provider = "no-such-provider".into();
            let storage = fixture.storage();
            let (served, ()) = over(Kind::Memory, params, async |link| {
                let (client, events) = AcpClient::new(link);
                let session_id = client
                    .new_session(fixture.cwd())
                    .await
                    .unwrap()
                    .value
                    .session_id;
                let input = SessionInput::Prompt(AgentInput::from_defaults(
                    STORED_TEXT.into(),
                    AgentMode::Build,
                    Vec::new(),
                    SessionDefaults::default(),
                    InputSource::Acp,
                ));
                let receipt: QueueReceipt = serde_json::from_value(
                    request(
                        &client,
                        SessionOperation::Enqueue {
                            id: None,
                            session_id: session_id.clone(),
                            input: Box::new(input),
                        },
                    )
                    .await,
                )
                .unwrap();
                loop {
                    if let ClientEvent::QueueUpdate(update) = events.recv_async().await.unwrap()
                        && update.receipt.id == receipt.id
                        && update.receipt.status == QueueStatus::Error
                    {
                        assert!(update.receipt.error.is_some());
                        break;
                    }
                }
                let saved =
                    StoredSession::read_only(parse_session_id(&session_id).unwrap(), &storage)
                        .unwrap();
                assert_eq!(saved.meta.accepted_work[0].status, QueueStatus::Error);
                assert!(saved.meta.accepted_work[0].input.is_some());
                client.shutdown(SHUTDOWN_GRACE).await;
                while events.recv_async().await.is_ok() {}
            });
            assert!(served.is_ok());
        }

        #[test]
        fn session_operations_keep_live_and_stored_sessions_independent() {
            let fixture = Fixture::new();
            let storage = fixture.storage();
            let (params, _ended) = fixture.params();
            with_client(Kind::Memory, params, async |client| {
                let live = client
                    .new_session(fixture.cwd())
                    .await
                    .unwrap()
                    .value
                    .session_id;
                let document = json!({ "messages": [
                    Message::user(STORED_TEXT.into()), Message { role: Role::Assistant, content: vec![MsgBlock::Text { text: "answer".into() }], ..Default::default() },
                    Message::user("second prompt".into()),
                ] });
                let imported = request(client, SessionOperation::Import {
                        parent: live.clone(),
                        transcript: Box::new(serde_json::from_value(document).unwrap()),
                    }).await;
                let imported: SessionId =
                    serde_json::from_value(imported["sessionId"].clone()).unwrap();
                let renamed = "a renamed session";
                for session_id in [live.clone(), imported.clone()] {
                    request(client, SessionOperation::SetTitle {
                            session_id: session_id.clone(),
                            title: renamed.into(),
                        }).await;
                    let read = request(client, SessionOperation::Read { session_id }).await;
                    assert_eq!(read["title"], renamed);
                }
                let exported = request(client, SessionOperation::Snapshot {
                        session_id: imported.clone(),
                    }).await;
                let exported = serde_json::to_value(SessionExport::stored(serde_json::from_value(exported).unwrap())).unwrap();
                assert_eq!(
                    exported["messages"][0]["content"],
                    json!([{
                        "type": "text", "text": STORED_TEXT,
                    }])
                );
                let forked = request(client, SessionOperation::Fork {
                        turn: None,
                        session_id: imported.clone(),
                    }).await;
                let forked: SessionId =
                    serde_json::from_value(forked["sessionId"].clone()).unwrap();
                let fork_export = request(client, SessionOperation::Snapshot {
                        session_id: forked.clone(),
                    }).await;
                let fork_export = serde_json::to_value(SessionExport::stored(serde_json::from_value(fork_export).unwrap())).unwrap();
                assert_eq!(fork_export["messages"], exported["messages"]);
                assert_eq!(
                    fork_export["title"],
                    format!("{FORK_TITLE_PREFIX}{renamed}")
                );
                let cut = request(client, SessionOperation::Fork {
                        session_id: imported.clone(),
                        turn: Some((2, "second prompt".into(), None)),
                    }).await;
                let cut_id: SessionId = serde_json::from_value(cut["sessionId"].clone()).unwrap();
                let cut = request(client, SessionOperation::Snapshot { session_id: cut_id }).await;
                let cut: StoredSession = serde_json::from_value(cut).unwrap();
                assert_eq!(cut.messages().len(), 2);
                assert_eq!(cut.meta.input_draft.as_deref(), Some("second prompt"));
                assert_eq!(cut.meta.input_draft_writers, None);
                request(client, SessionOperation::Delete {
                        session_id: imported.clone(),
                    }).await;
                assert!(
                    client
                        .session_operation(SessionOperation::Read {
                            session_id: imported
                        })
                        .await
                        .is_err()
                );
                request(client, SessionOperation::Delete { session_id: live }).await;
                request(client, SessionOperation::SetTitle {
                        session_id: forked.clone(),
                        title: renamed.into(),
                    }).await;
                let saved =
                    StoredSession::read_only(parse_session_id(&forked).unwrap(), &storage).unwrap();
                assert_eq!(saved.title, renamed);
                assert_eq!(saved.messages().len(), 3);
            })
            .found();
        }

        #[test]
        fn host_session_changes_respect_another_process_claim() {
            let fixture = Fixture::new();
            let storage = fixture.storage();
            let mut stored = StoredSession::new(SELECTED_SPEC, &fixture.cwd().to_string_lossy());
            stored.push_message(Message::user(STORED_TEXT.into()));
            let claim = SessionClaim::acquire(stored.id, &storage).unwrap();
            stored.save(&claim, &storage).unwrap();
            let session_id = SessionId::new(stored.id.to_string());
            let (params, _ended) = fixture.params();
            with_client(Kind::Memory, params, async |client| {
                assert!(
                    client
                        .session_operation(SessionOperation::SetTitle {
                            session_id: session_id.clone(),
                            title: STORED_TEXT.into(),
                        })
                        .await
                        .is_err()
                );
                assert!(
                    client
                        .session_operation(SessionOperation::Delete { session_id })
                        .await
                        .is_err()
                );
            })
            .found();
            assert_eq!(
                StoredSession::read_only(stored.id, &storage)
                    .unwrap()
                    .messages(),
                stored.messages()
            );
        }

        #[test]
        fn sessions_keep_their_settings_and_survive_loading_a_sibling() {
            let fixture = Fixture::new();
            let (params, ended) = fixture.params();
            let mut stored: StoredSession =
                Session::new(SELECTED_SPEC, &fixture.cwd().to_string_lossy());
            stored.push_message(Message::user(STORED_TEXT.into()));
            let claim = SessionClaim::acquire(stored.id, &fixture.storage()).unwrap();
            stored.save(&claim, &fixture.storage()).unwrap();
            drop(claim);
            let (served, second) = over(Kind::Memory, params, async |link| {
                let load = raw_request(
                    RAW_ID,
                    "session/load",
                    json!({ "sessionId": stored.id.to_string(), "cwd": fixture.cwd(), "mcpServers": [] }),
                );
                let loaded = raw_reply(&link, &load, &json!(RAW_ID)).await;
                assert!(loaded.get("error").is_none(), "{loaded:?}");
                let new = raw_request(
                    RAW_ID,
                    "session/new",
                    json!({ "cwd": fixture.cwd(), "mcpServers": [] }),
                );
                let opened = raw_reply(&link, &new, &json!(RAW_ID)).await;
                let second = opened["result"]["sessionId"].as_str().unwrap().to_owned();
                for (session, workflow) in [(stored.id.to_string(), true), (second.clone(), false)]
                {
                    let request = raw_request(
                        RAW_ID,
                        "session/set_config_option",
                        json!({ "sessionId": session, "configId": settings::WORKFLOW_CONFIG_ID, "value": workflow, "type": "boolean" }),
                    );
                    let reply = raw_reply(&link, &request, &json!(RAW_ID)).await;
                    assert!(reply.get("error").is_none(), "{reply:?}");
                }
                let loaded = raw_reply(&link, &load, &json!(RAW_ID)).await;
                let options = loaded["result"]["configOptions"].as_array().unwrap();
                assert!(
                    options
                        .iter()
                        .any(|option| option["id"] == settings::WORKFLOW_CONFIG_ID
                            && option["currentValue"] == true)
                );
                let request = raw_request(
                    RAW_ID,
                    "session/set_config_option",
                    json!({ "sessionId": second, "configId": settings::WORKFLOW_CONFIG_ID, "value": true, "type": "boolean" }),
                );
                let reply = raw_reply(&link, &request, &json!(RAW_ID)).await;
                assert!(reply.get("error").is_none(), "{reply:?}");
                end_raw(&link).await;
                second.parse::<SessionRef>().unwrap().id()
            });
            assert!(served.is_ok(), "{SERVER_FAILED}: {served:?}");
            let ended: Vec<_> = ended.try_iter().collect();
            assert!(ended.contains(&(stored.id, SessionEndReason::Replaced)));
            assert!(!ended.contains(&(second, SessionEndReason::Replaced)));
            for id in [stored.id, second] {
                assert!(ended.contains(&(id, SessionEndReason::Shutdown)));
            }
        }

        #[cfg_attr(unix, test_case(Kind::Stream ; "byte_stream"))]
        #[test_case(Kind::Memory ; "in_memory")]
        fn session_new_opens_a_session(kind: Kind) {
            let fixture = Fixture::new();
            let (params, ended) = fixture.params();
            let cwd = fixture.cwd();
            let opened = with_client(kind, params, async move |client| {
                client.new_session(cwd).await
            })
            .found()
            .unwrap();

            let session: SessionRef = opened.value.session_id.0.parse().unwrap();
            assert_eq!(
                ended.try_recv(),
                Ok((session.id(), SessionEndReason::Shutdown))
            );
        }

        #[cfg_attr(unix, test_case(Kind::Stream ; "byte_stream"))]
        #[test_case(Kind::Memory ; "in_memory")]
        fn session_list_lists_a_stored_session(kind: Kind) {
            let fixture = Fixture::new();
            let storage = fixture.storage();
            let cwd = fixture.cwd();
            let mut ids = Vec::new();
            for path in [&cwd, fixture.storage.path()] {
                let mut stored: StoredSession =
                    Session::new(SELECTED_SPEC, &path.to_string_lossy());
                stored.push_message(Message::user(STORED_TEXT.into()));
                stored
                    .save(
                        &SessionClaim::acquire(stored.id, &storage).unwrap(),
                        &storage,
                    )
                    .unwrap();
                ids.push(stored.id.to_string());
            }
            let (params, _ended) = fixture.params();
            let (served, ()) = over(kind, params, async |link| {
                let request = raw_request(
                    RAW_ID,
                    AGENT_METHOD_NAMES.session_list,
                    json!({ "cwd": cwd }),
                );
                let reply = raw_reply(&link, &request, &json!(RAW_ID)).await;
                let listed: ListSessionsResponse =
                    serde_json::from_value(reply["result"].clone()).unwrap();
                assert_eq!(listed.sessions.len(), 1);
                let here = &listed.sessions[0];
                assert_eq!(here.session_id.0.as_ref(), ids[0]);
                assert_eq!(here.title.as_deref(), Some("New session"));
                assert!(
                    here.updated_at
                        .as_ref()
                        .unwrap()
                        .parse::<jiff::Timestamp>()
                        .is_ok()
                );
                let request = raw_request(FOLLOWING_ID, AGENT_METHOD_NAMES.session_list, json!({}));
                let all = raw_reply(&link, &request, &json!(FOLLOWING_ID)).await;
                let all: ListSessionsResponse =
                    serde_json::from_value(all["result"].clone()).unwrap();
                let all: Vec<_> = all
                    .sessions
                    .iter()
                    .map(|session| session.session_id.to_string())
                    .collect();
                assert!(ids.iter().all(|id| all.contains(id)));
                end_raw(&link).await;
            });
            assert!(served.is_ok(), "{SERVER_FAILED}: {served:?}");
        }

        #[cfg_attr(unix, test_case(Kind::Stream ; "byte_stream"))]
        #[test_case(Kind::Memory ; "in_memory")]
        fn an_unknown_method_is_not_found(kind: Kind) {
            let fixture = Fixture::new();
            let (params, _ended) = fixture.params();
            let (served, reply) = over(kind, params, async |link| {
                let request = raw_request(RAW_ID, UNKNOWN_METHOD, json!({}));
                let reply = raw_reply(&link, &request, &json!(RAW_ID)).await;
                end_raw(&link).await;
                reply
            });

            assert!(served.is_ok(), "{SERVER_FAILED}: {served:?}");
            assert_eq!(
                reply["error"]["code"],
                json!(AcpError::method_not_found().code)
            );
        }

        #[cfg_attr(unix, test_case(Kind::Stream ; "byte_stream"))]
        #[test_case(Kind::Memory ; "in_memory")]
        fn a_malformed_line_is_a_parse_error_and_the_link_goes_on(kind: Kind) {
            let fixture = Fixture::new();
            let (params, _ended) = fixture.params();
            let (served, (parse, following)) = over(kind, params, async |link| {
                let parse = raw_reply(&link, MALFORMED_LINE, &Value::Null).await;
                let request = raw_request(
                    FOLLOWING_ID,
                    AGENT_METHOD_NAMES.initialize,
                    json!({ "protocolVersion": 1 }),
                );
                let following = raw_reply(&link, &request, &json!(FOLLOWING_ID)).await;
                end_raw(&link).await;
                (parse, following)
            });

            assert!(served.is_ok(), "{SERVER_FAILED}: {served:?}");
            assert_eq!(parse["error"]["code"], json!(AcpError::parse_error().code));
            assert!(following.get("result").is_some(), "{following}");
        }

        #[cfg_attr(unix, test_case(Kind::Stream ; "byte_stream"))]
        #[test_case(Kind::Memory ; "in_memory")]
        fn client_shutdown_ends_the_server(kind: Kind) {
            let fixture = Fixture::new();
            let (params, _ended) = fixture.params();

            let run = with_client(kind, params, async |_| {});

            assert!(run.served.is_ok(), "{SERVER_FAILED}: {:?}", run.served);
            assert!(run.closed, "{LINK_LEFT_OPEN}");
        }

        #[test_case(false ; "input_closes_with_output_full")]
        #[test_case(true ; "output_disconnects_with_input_open")]
        fn a_stalled_memory_peer_does_not_prevent_session_cleanup(disconnect: bool) {
            let fixture = Fixture::new();
            let (params, ended) = fixture.params();
            let (served, ()) = over(Kind::Memory, params, async |link| {
                let request = raw_request(
                    RAW_ID,
                    AGENT_METHOD_NAMES.session_new,
                    json!({ "cwd": fixture.cwd(), "mcpServers": [] }),
                );
                let reply = raw_reply(&link, &request, &json!(RAW_ID)).await;
                assert!(reply.get("result").is_some(), "{reply}");
                for id in 0..EVENT_CAPACITY * 2 + 2 {
                    link.tx
                        .send(raw_request(id as i64, UNKNOWN_METHOD, json!({})))
                        .unwrap();
                }
                while link.rx.len() < EVENT_CAPACITY {
                    future::yield_now().await;
                }
                let mut output = Some(link.rx);
                if disconnect {
                    drop(output.take());
                } else {
                    link.tx.send(END_OF_INPUT.to_owned()).unwrap();
                }
                assert_eq!(
                    ended.recv_async().await.map(|(_, reason)| reason),
                    Ok(SessionEndReason::Shutdown)
                );
                // Keep the opposite half open until cleanup has completed.
                drop((output, link.tx));
            });
            assert!(served.is_ok(), "{SERVER_FAILED}: {served:?}");
        }

        struct StalledWriter;

        impl AsyncWrite for StalledWriter {
            fn poll_write(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
                _: &[u8],
            ) -> Poll<io::Result<usize>> {
                Poll::Pending
            }

            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Pending
            }

            fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        #[test]
        fn input_eof_releases_a_stalled_byte_writer() {
            block_on(async {
                let (output, rx) = flume::bounded(1);
                output.send(MALFORMED_LINE.into()).unwrap();
                let (input, _rx) = flume::unbounded();
                let (end, ended) = CancelToken::new();
                let mut writing = Box::pin(write_lines(
                    StalledWriter,
                    rx,
                    input.downgrade(),
                    ended,
                    CancelToken::none(),
                ));
                assert!(future::poll_once(&mut writing).await.is_none());
                end.cancel();
                writing.await.unwrap();
                assert!(output.is_disconnected());
            });
        }

        /// A peer that stopped reading: every write fails.
        #[cfg(unix)]
        struct FailingWriter;

        #[cfg(unix)]
        impl AsyncWrite for FailingWriter {
            fn poll_write(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
                _: &[u8],
            ) -> Poll<io::Result<usize>> {
                Poll::Ready(Err(io::Error::new(WRITE_FAILURE_KIND, WRITE_FAILURE)))
            }

            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }

            fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        /// The peer stays open and silent after its one request, so only the
        /// failed write can end the server.
        #[cfg(unix)]
        #[test]
        fn a_failed_write_ends_the_server_with_its_error() {
            let fixture = Fixture::new();
            let (params, ended) = fixture.params();
            let _entered = test_runtime().enter();
            let (ours, mut peer) = UnixStream::pair().unwrap();
            let request = raw_request(
                RAW_ID,
                AGENT_METHOD_NAMES.session_new,
                json!({ "cwd": fixture.cwd(), "mcpServers": [] }),
            );

            let error = block_on(future::or(
                async {
                    peer.write_all(format!("{request}\n").as_bytes())
                        .await
                        .unwrap();
                    serve_stream(params, BufReader::new(ours), FailingWriter).await
                },
                stalled(),
            ))
            .unwrap_err();

            assert_eq!(error.kind(), WRITE_FAILURE_KIND);
            assert_eq!(error.to_string(), WRITE_FAILURE);
            assert_eq!(
                ended.try_recv().map(|(_, reason)| reason),
                Ok(SessionEndReason::Shutdown)
            );
            drop(peer);
        }
    }
}

#[cfg(test)]
mod iso_tests {
    use super::iso_from_epoch;
    use test_case::test_case;

    #[test_case(0, None ; "unknown_sentinel_is_no_timestamp")]
    #[test_case(1_700_000_000, Some("2023-11-14T22:13:20Z") ; "epoch_seconds_to_utc")]
    #[test_case(u64::MAX, None ; "out_of_range_drops_the_timestamp")]
    fn iso_from_epoch_converts(secs: u64, expected: Option<&str>) {
        assert_eq!(iso_from_epoch(secs).as_deref(), expected);
    }
}
