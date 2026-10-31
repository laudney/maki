//! An ACP client over a link of JSON lines. One reader task owns the
//! incoming side: it resolves request waiters, turns agent requests and
//! session updates into [`ClientEvent`]s, and answers every agent request it
//! does not implement, so the agent never waits on this client by accident.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use agent_client_protocol_schema::ProtocolVersion;
use agent_client_protocol_schema::v1::{
    AGENT_METHOD_NAMES, AuthCapabilities, CLIENT_METHOD_NAMES, CancelNotification,
    ClientCapabilities, ClientSessionCapabilities, CompactionCapabilities, ContentBlock,
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAction, ElicitationScope,
    Error as AcpError, Implementation, InitializeRequest, InitializeResponse, JsonRpcMessage,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, Meta,
    NewSessionRequest, NewSessionResponse, Notification, PermissionOptionId, PromptRequest,
    PromptResponse, Request, RequestId, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, Response, SelectedPermissionOutcome, SessionConfigOption,
    SessionConfigOptionValue, SessionId, SessionInfo, SessionModeState, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    SetSessionModeRequest,
};
use flume::{Receiver, Sender};
use futures_lite::future;
use maki_storage::id::MakiId;
use maki_storage::sessions::{QueueReceipt, QueueStatus};
use maki_types::btw::SideAnswer;
use maki_types::session::StoredSession;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::process::Stdio;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio_util::task::AbortOnDropHandle;
use tracing::{debug, warn};

use crate::btw::{BTW_METHOD, BtwChunk, BtwRequest, CHUNK_METHOD};
use crate::configuration::{Key, wire_value};
use crate::elicitation::{self, extension_questions};
use crate::plan::{self, PlanRead, PlanText, PlanWrite};
use crate::plugins::{self, PluginReply, PluginRequest};
use crate::session::{
    METHOD as SESSION_METHOD, QUEUE_UPDATE_METHOD, QueueUpdate, SessionOperation,
};
use crate::usage::{QUOTA_METHOD, QuotaAnswer, QuotaRequest};
use crate::{extensions, login, sanitize, tool_data};

/// Bounded so a UI that stops draining (suspend, `$EDITOR`) pauses the
/// reader and lets the pipe push back, instead of buffering without end.
pub const EVENT_CAPACITY: usize = 256;
/// Queued last by [`AcpClient::shutdown`]. The client never sends an empty
/// line, so a writer that reads one has written everything before it, and
/// closes the peer's input.
pub const END_OF_INPUT: &str = "";
const FIRST_REQUEST_ID: i64 = 1;
const PUMP_STDIN: &str = "stdin";
const PUMP_STDOUT: &str = "stdout";

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("the agent connection is closed")]
    Closed,
    #[error("the agent rejected the request: {0}")]
    Rpc(AcpError),
    #[error("invalid JSON-RPC payload: {0}")]
    Decode(#[from] serde_json::Error),
    /// Nothing was sent, so no answer follows.
    #[error("could not encode the request: {0}")]
    Encode(serde_json::Error),
    #[error("no session is open on this connection")]
    NoSession,
    #[error("no pending permission request has id {0}")]
    UnknownAsk(u64),
    #[error("the agent did not offer permission option {0}")]
    UnofferedOption(PermissionOptionId),
}

#[derive(Debug)]
pub enum ClientEvent {
    QueueUpdate(QueueUpdate),
    Restored(Box<StoredSession>),
    RestoreError(serde_json::Error),
    Task(crate::session::TaskUpdate),
    /// With the sequence number of the line it came on, see [`Sequenced`].
    Update(u64, SessionUpdate),
    Permission(PermissionAsk),
    /// A question the agent asks under the question extension; answer it
    /// with [`AcpClient::reply_question`].
    Question(QuestionAsk),
    /// A line the reader could not use. The connection stays up.
    Malformed(String),
    /// Queued right after the reader settles a `session/load` answer, so
    /// every update the agent replayed before it is ahead of this marker.
    ReplayEnd,
    /// The answer to a routed request, see [`AcpClient::set_config_routed`],
    /// in its place on the wire among the updates.
    Answer {
        seq: u64,
        tag: u64,
        result: Result<Value, AcpError>,
    },
    /// Sent once, after every pending request failed with [`ClientError::Closed`].
    Closed,
}

/// `questions` is the question tool's input as the model wrote it. `id` is
/// local to this client, as a [`PermissionAsk`]'s is.
#[derive(Debug)]
pub struct QuestionAsk {
    pub id: u64,
    pub questions: Value,
}

/// `id` is local to this client. The JSON-RPC id stays inside, so a reply
/// can only ever answer the request it was shown for.
#[derive(Debug)]
pub struct PermissionAsk {
    pub id: u64,
    pub request: RequestPermissionRequest,
}

/// A response with the sequence number of the line it came on. Every
/// incoming line is numbered in wire order, so a caller holding a response
/// and updates can tell which the agent sent last: responses and updates
/// reach it on different channels.
#[derive(Debug)]
pub struct Sequenced<T> {
    pub seq: u64,
    pub value: T,
}

/// What the agent last confirmed of the bound session's config options and
/// mode: its `session/new` or `session/load` answer, a later update, or the
/// answer to a change the client asked for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Confirmed {
    pub config_options: Vec<SessionConfigOption>,
    pub mode: Option<String>,
}

impl Confirmed {
    fn opened(
        config_options: Option<Vec<SessionConfigOption>>,
        modes: Option<SessionModeState>,
    ) -> Self {
        Self {
            config_options: config_options.unwrap_or_default(),
            mode: modes.map(|modes| modes.current_mode_id.to_string()),
        }
    }
}

#[derive(Debug)]
pub struct Loaded {
    pub response: Sequenced<LoadSessionResponse>,
    pub replay: Vec<SessionUpdate>,
    pub early: Vec<ClientEvent>,
    pub unreadable: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionReply {
    Selected(PermissionOptionId),
    Cancelled,
}

/// Whole JSON lines in each direction. A child process, a socket or a test
/// fills the same pair.
pub struct Link {
    pub tx: Sender<String>,
    pub rx: Receiver<String>,
    pumps: Option<Pumps>,
}

/// What carries lines to and from the peer. Dropping it stops both pumps
/// and kills a child process.
pub(crate) struct Pumps {
    child: Option<Child>,
    /// The writer, then the reader.
    tasks: [AbortOnDropHandle<()>; 2],
}

impl Pumps {
    pub(crate) fn new(child: Option<Child>, tasks: [AbortOnDropHandle<()>; 2]) -> Self {
        Self { child, tasks }
    }

    /// The writer drains what was queued before [`END_OF_INPUT`], then
    /// closes the peer's input, which it reads as the end of the connection
    /// and closes its sessions, saving them. Whatever is still running after
    /// `grace` is dropped, which kills a child.
    async fn shutdown(self, grace: Duration) {
        let [writer, reader] = self.tasks;
        let child = self.child;
        let finish = async move {
            let _ = writer.await;
            if let Some(mut child) = child {
                let _ = child.wait().await;
            }
        };
        future::or(finish, async {
            tokio::time::sleep(grace).await;
        })
        .await;
        drop(reader);
    }
}

impl Link {
    pub fn new(tx: Sender<String>, rx: Receiver<String>) -> Self {
        Self {
            tx,
            rx,
            pumps: None,
        }
    }

    pub(crate) fn with_pumps(tx: Sender<String>, rx: Receiver<String>, pumps: Pumps) -> Self {
        Self {
            tx,
            rx,
            pumps: Some(pumps),
        }
    }

    /// Runs `command` with piped stdin and stdout. Its stderr is inherited.
    pub fn spawn(mut command: Command) -> io::Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or(io::ErrorKind::BrokenPipe)?;
        let stdout = child.stdout.take().ok_or(io::ErrorKind::BrokenPipe)?;
        let (out_tx, out_rx) = flume::unbounded::<String>();
        let (in_tx, in_rx) = flume::bounded::<String>(EVENT_CAPACITY);
        // Dropped when the writer stops, so a child that closed its stdin but
        // keeps stdout open still closes the link and fails pending requests.
        let (writer_alive, writer_gone) = flume::bounded::<()>(0);
        let writer = tokio::spawn(logged(PUMP_STDIN, async move {
            let _writer_alive = writer_alive;
            while let Ok(line) = out_rx.recv_async().await {
                if line == END_OF_INPUT {
                    break;
                }
                stdin.write_all(line.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await?;
            }
            Ok(())
        }));
        let reader = tokio::spawn(logged(PUMP_STDOUT, async move {
            let mut lines = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                let read = async { Some(lines.read_line(&mut line).await) };
                let gone = async {
                    let _ = writer_gone.recv_async().await;
                    None
                };
                // The signal goes first: a child that keeps writing must not
                // hide that nothing reaches it any more.
                match futures_lite::future::or(gone, read).await {
                    None | Some(Ok(0)) => return Ok(()),
                    Some(read) => read?,
                };
                if in_tx.send_async(line).await.is_err() {
                    return Ok(());
                }
            }
        }));
        Ok(Self::with_pumps(
            out_tx,
            in_rx,
            Pumps::new(
                Some(child),
                [
                    AbortOnDropHandle::new(writer),
                    AbortOnDropHandle::new(reader),
                ],
            ),
        ))
    }
}

pub(crate) async fn logged(pump: &'static str, work: impl Future<Output = io::Result<()>>) {
    if let Err(error) = work.await {
        warn!(pump, %error, "agent link pump stopped");
    }
}

type RpcReply = (u64, Result<Value, AcpError>);
struct Waiter {
    reply: Sender<RpcReply>,
    binds: Binds,
    _settled: Option<Settled>,
}

/// Held by the waiter of a request someone may wait on without the call,
/// and dropped with it when the agent answers or the link closes.
type Settled = Sender<()>;

/// What an open request is waited on as, see [`AcpClient::end_turns`] and
/// [`AcpClient::settle_settings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Turn,
    Setting,
}

enum Binds {
    Nothing,
    Session,
    /// The caller reads the replay from the event channel up to the marker.
    /// A refused load gives `previous` back before the next line, so an
    /// update the agent sends for it right after is kept.
    Replay {
        target: SessionId,
        previous: Option<SessionId>,
    },
    /// The answer goes on the event channel, tagged by the caller. Config
    /// replies also confirm options for the binding of {generation}.
    Event {
        tag: u64,
        generation: u64,
        config: bool,
    },
    /// A config option change, whose answer is confirmed state for the
    /// binding of {generation}.
    Config {
        generation: u64,
    },
    /// A mode change to {mode}, confirmed by any success for the binding
    /// of {generation}.
    Mode {
        mode: String,
        generation: u64,
    },
    /// The request's `_maki/btw/chunk`s go to this sink until it is
    /// answered. Unbounded, so the reader never waits on it.
    Chunks(Sender<SideAnswer>),
}

#[allow(
    clippy::large_enum_variant,
    reason = "moved once per event; boxing would allocate per update"
)]
enum Loading {
    Answered(Result<Sequenced<LoadSessionResponse>, ClientError>),
    Event(Option<ClientEvent>),
}

struct Ask {
    rpc_id: RequestId,
    options: Vec<PermissionOptionId>,
}

#[derive(Default)]
struct Shared {
    waiters: HashMap<RequestId, Waiter>,
    queue_sinks: HashMap<MakiId, Settled>,
    asks: HashMap<u64, Ask>,
    questions: HashMap<u64, RequestId>,
    next_ask: u64,
    session: Option<SessionId>,
    /// Bumped whenever `session` is bound, so an answer to a change asked
    /// for under an earlier binding never confirms state for this one.
    generation: u64,
    confirmed: Confirmed,
    /// The prompts and setting changes not answered yet, which may outlive
    /// the call that sent them.
    open: HashMap<RequestId, (Open, Receiver<()>)>,
    closed: bool,
}

impl Shared {
    /// Records {id} as open until the agent answers it, and returns what
    /// its waiter holds until then.
    fn watch(&mut self, id: &RequestId, open: Open) -> Settled {
        let (settled, answered) = flume::bounded(1);
        self.open.insert(id.clone(), (open, answered));
        settled
    }

    fn forget_queue(&mut self, id: MakiId) {
        self.queue_sinks.remove(&id);
        self.open.remove(&RequestId::from(format!("queue/{id}")));
    }

    fn bind(&mut self, session: Option<SessionId>) {
        self.session = session;
        for (id, _) in self.queue_sinks.drain() {
            self.open.remove(&RequestId::from(format!("queue/{id}")));
        }
        self.generation += 1;
    }
}

pub struct AcpClient {
    out: Sender<String>,
    shared: Arc<Mutex<Shared>>,
    next_id: AtomicI64,
    _reader: AbortOnDropHandle<()>,
    pumps: Mutex<Option<Pumps>>,
}

impl AcpClient {
    pub fn new(link: Link) -> (Self, Receiver<ClientEvent>) {
        let (events_tx, events_rx) = flume::bounded(EVENT_CAPACITY);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let reader = tokio::spawn(read_loop(
            link.rx,
            link.tx.clone(),
            Arc::clone(&shared),
            events_tx,
        ));
        let client = Self {
            out: link.tx,
            shared,
            next_id: AtomicI64::new(FIRST_REQUEST_ID),
            _reader: AbortOnDropHandle::new(reader),
            pumps: Mutex::new(link.pumps),
        };
        (client, events_rx)
    }

    /// `terminal_login` when this client can run the agent's own invocation
    /// in its terminal, which only a spawned agent allows.
    pub async fn initialize(
        &self,
        client_info: Implementation,
        terminal_login: bool,
    ) -> Result<InitializeResponse, ClientError> {
        // The agent may then report its compactions.
        let capabilities = ClientCapabilities::new()
            .session(ClientSessionCapabilities::new().compaction(CompactionCapabilities::new()))
            .auth(AuthCapabilities::new().terminal(terminal_login));
        let request = InitializeRequest::new(ProtocolVersion::V1)
            .client_info(client_info)
            .client_capabilities(capabilities)
            .meta(extensions::request_meta());
        self.request(AGENT_METHOD_NAMES.initialize, request).await
    }

    pub async fn plugins(&self, request: &PluginRequest) -> Result<PluginReply, ClientError> {
        self.request(plugins::METHOD, request).await
    }

    /// Updates for any session but this one are dropped from here on.
    /// The reader binds the session as it reads the answer, before the next
    /// line, so an update the agent sends right after it is never dropped.
    pub async fn new_session(
        &self,
        cwd: PathBuf,
    ) -> Result<Sequenced<NewSessionResponse>, ClientError> {
        self.call(
            AGENT_METHOD_NAMES.session_new,
            NewSessionRequest::new(cwd),
            Binds::Session,
        )
        .await
    }

    /// The agent replays the session's history before it answers, so the
    /// session is bound before the request goes out and the replay is read
    /// from `events` while the answer is pending: a full event channel
    /// would otherwise stop the reader short of the answer. A failed load
    /// restores the session bound before.
    pub async fn load_session(
        &self,
        events: &Receiver<ClientEvent>,
        session: SessionId,
        cwd: PathBuf,
    ) -> Result<Loaded, ClientError> {
        let previous = {
            let mut shared = self.lock();
            let previous = shared.session.clone();
            shared.bind(Some(session.clone()));
            previous
        };
        let loaded = self
            .read_load(events, session.clone(), previous.clone(), cwd)
            .await;
        // An answer that refused the load was rolled back by the reader.
        // These two never reached the agent, so nothing races this.
        if let Err(ClientError::Closed | ClientError::Encode(_)) = &loaded {
            let mut shared = self.lock();
            if shared.session.as_ref() == Some(&session) {
                shared.bind(previous);
            }
        }
        loaded
    }

    async fn read_load(
        &self,
        events: &Receiver<ClientEvent>,
        session: SessionId,
        previous: Option<SessionId>,
        cwd: PathBuf,
    ) -> Result<Loaded, ClientError> {
        let mut request = pin!(self.call(
            AGENT_METHOD_NAMES.session_load,
            LoadSessionRequest::new(session.clone(), cwd),
            Binds::Replay {
                target: session,
                previous,
            },
        ));
        let mut replay = Vec::new();
        let mut early = Vec::new();
        let mut unreadable = 0;
        let mut ended = false;
        let mut answer = None;
        while answer.is_none() || !ended {
            let next = match answer {
                None => {
                    future::or(async { Loading::Answered(request.as_mut().await) }, async {
                        Loading::Event(events.recv_async().await.ok())
                    })
                    .await
                }
                Some(_) => Loading::Event(events.recv_async().await.ok()),
            };
            match next {
                // No answer came off the wire, so no marker follows.
                Loading::Answered(Err(error @ (ClientError::Closed | ClientError::Encode(_)))) => {
                    return Err(error);
                }
                Loading::Answered(result) => answer = Some(result),
                Loading::Event(None | Some(ClientEvent::Closed)) => {
                    return Err(ClientError::Closed);
                }
                Loading::Event(Some(ClientEvent::ReplayEnd)) => ended = true,
                Loading::Event(Some(ClientEvent::Update(seq, update))) => {
                    if early.is_empty() {
                        replay.push(update);
                    } else {
                        early.push(ClientEvent::Update(seq, update));
                    }
                }
                Loading::Event(Some(ClientEvent::RestoreError(error))) => {
                    return Err(ClientError::Decode(error));
                }
                Loading::Event(Some(ClientEvent::Malformed(reason))) => {
                    warn!(%reason, "unreadable line in a session replay");
                    unreadable += 1;
                }
                // A load opens the connection's session, before anything
                // could have sent a routed request.
                Loading::Event(Some(
                    event @ (ClientEvent::QueueUpdate(_)
                    | ClientEvent::Task(_)
                    | ClientEvent::Restored(_)),
                )) => early.push(event),
                Loading::Event(Some(ClientEvent::Answer { .. })) => {}
                Loading::Event(Some(ClientEvent::Permission(ask))) => {
                    if early
                        .iter()
                        .any(|event| matches!(event, ClientEvent::QueueUpdate(_)))
                    {
                        early.push(ClientEvent::Permission(ask));
                    } else {
                        let _ = self.reply_permission(ask.id, PermissionReply::Cancelled);
                    }
                }
                Loading::Event(Some(ClientEvent::Question(ask))) => {
                    if early
                        .iter()
                        .any(|event| matches!(event, ClientEvent::QueueUpdate(_)))
                    {
                        early.push(ClientEvent::Question(ask));
                    } else {
                        let _ = self.reply_question(ask.id, None);
                    }
                }
            }
        }
        let response = answer.unwrap_or(Err(ClientError::Closed))?;
        Ok(Loaded {
            response,
            replay,
            early,
            unreadable,
        })
    }

    pub async fn list_sessions(
        &self,
        cwd: PathBuf,
        cursor: Option<String>,
    ) -> Result<ListSessionsResponse, ClientError> {
        self.request(
            AGENT_METHOD_NAMES.session_list,
            ListSessionsRequest::new().cwd(cwd).cursor(cursor),
        )
        .await
    }

    /// Every page of the agent's stored sessions. A cursor seen before ends
    /// the walk, so an agent that repeats one cannot hold the connection
    /// forever.
    pub async fn list_all_sessions(&self, cwd: PathBuf) -> Result<Vec<SessionInfo>, ClientError> {
        let mut sessions = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor = None;
        loop {
            let listed = self.list_sessions(cwd.clone(), cursor).await?;
            sessions.extend(listed.sessions);
            match listed.next_cursor {
                Some(next) if seen.insert(next.clone()) => cursor = Some(next),
                _ => return Ok(sessions),
            }
        }
    }

    /// Resolves when the agent answers, which is what ends the turn. The
    /// answer queues behind the turn's updates, so the caller must keep
    /// draining the event receiver while it waits.
    /// `meta` is what the prompt carries beside its content, such as its
    /// settings under the settings extension.
    pub async fn prompt(
        &self,
        prompt: Vec<ContentBlock>,
        meta: Option<Meta>,
    ) -> Result<PromptResponse, ClientError> {
        let session = self.session()?;
        Ok(self
            .call(
                AGENT_METHOD_NAMES.session_prompt,
                PromptRequest::new(session, prompt).meta(meta),
                Binds::Nothing,
            )
            .await?
            .value)
    }

    /// The bound session and the generation of its binding, read together,
    /// so a request is never stamped with a binding other than the one it
    /// names.
    fn binding(&self) -> Result<(SessionId, u64), ClientError> {
        let shared = self.lock();
        let session = shared.session.clone().ok_or(ClientError::NoSession)?;
        Ok((session, shared.generation))
    }

    /// The requests of kind {open} still waiting for an answer, which are
    /// then no longer recorded.
    fn take_open(&self, open: Open) -> Vec<Receiver<()>> {
        self.lock()
            .open
            .extract_if(|_, (kind, _)| *kind == open)
            .map(|(_, (_, answered))| answered)
            .collect()
    }

    /// Cancels every prompt the agent has not answered yet, which a caller
    /// may have stopped waiting on, and waits up to {grace} for the answers;
    /// whether they all came. The agent sends a turn's updates before its
    /// answer, so once they came every update of those turns has reached
    /// the event channel, which someone must read meanwhile: the reader
    /// waits on a full one.
    pub async fn end_turns(&self, grace: Duration) -> bool {
        let turns = self.take_open(Open::Turn);
        if !turns.is_empty() {
            let _ = self.cancel();
        }
        all_answered(turns, grace).await
    }

    /// Waits up to {grace} for the answer to every setting or mode change
    /// still open, which a caller may have stopped waiting on; whether they
    /// all came, so [`Self::confirmed`] holds what the agent settled on.
    pub async fn settle_settings(&self, grace: Duration) -> bool {
        all_answered(self.take_open(Open::Setting), grace).await
    }

    /// Answers every request of the agent still open as refused: permission
    /// permission asks cancelled and questions dismissed. This
    /// includes the ones a caller already took from the event channel and
    /// will not answer, so no turn waits on a client that moved on.
    pub fn retire_requests(&self) {
        let (asks, questions): (Vec<u64>, Vec<u64>) = {
            let shared = self.lock();
            (
                shared.asks.keys().copied().collect(),
                shared.questions.keys().copied().collect(),
            )
        };
        for ask in asks {
            let _ = self.reply_permission(ask, PermissionReply::Cancelled);
        }
        for ask in questions {
            let _ = self.reply_question(ask, None);
        }
    }

    pub fn confirmed(&self) -> Confirmed {
        self.lock().confirmed.clone()
    }

    /// Sends a config option and returns at once: the answer arrives on the
    /// event channel as [`ClientEvent::Answer`] with `tag`, so a caller that
    /// waits on events handles it in any state, and it is never lost when
    /// the caller moves on.
    pub fn set_config_routed(
        &self,
        config_id: &str,
        value: SessionConfigOptionValue,
        tag: u64,
    ) -> Result<(), ClientError> {
        let session = self.bound_session()?;
        self.route(
            AGENT_METHOD_NAMES.session_set_config_option,
            SetSessionConfigOptionRequest::new(session, config_id.to_owned(), value),
            tag,
            true,
        )
    }

    pub fn session_operation_routed(
        &self,
        operation: &SessionOperation,
        tag: u64,
    ) -> Result<(), ClientError> {
        let queued = match operation {
            SessionOperation::Enqueue { id: Some(id), .. } => Some(*id),
            _ => None,
        };
        if let Some(id) = queued {
            let settled = self
                .lock()
                .watch(&RequestId::from(format!("queue/{id}")), Open::Turn);
            self.lock().queue_sinks.insert(id, settled);
        }
        self.route(SESSION_METHOD, operation, tag, false)
            .inspect_err(|_| {
                if let Some(id) = queued {
                    self.lock().forget_queue(id);
                }
            })
    }

    pub fn prompt_routed(
        &self,
        blocks: Vec<ContentBlock>,
        meta: Option<Meta>,
        tag: u64,
    ) -> Result<(), ClientError> {
        let mut request = PromptRequest::new(self.bound_session()?, blocks);
        request.meta = meta;
        self.route(AGENT_METHOD_NAMES.session_prompt, request, tag, false)
    }

    fn route(
        &self,
        method: &str,
        params: impl Serialize,
        tag: u64,
        config: bool,
    ) -> Result<(), ClientError> {
        let (_, generation) = self.binding()?;
        let id = self.next_request_id();
        self.send_call(
            id,
            method,
            params,
            Binds::Event {
                tag,
                generation,
                config,
            },
        )
        .map(drop)
    }

    pub fn forget_queue(&self, id: MakiId) {
        self.lock().forget_queue(id);
    }

    /// Closes the link gracefully, after everything already sent, see
    /// `Pumps::shutdown`.
    pub async fn shutdown(&self, grace: Duration) {
        let _ = self.out.send(END_OF_INPUT.to_owned());
        let pumps = self.pumps.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(pumps) = pumps {
            pumps.shutdown(grace).await;
        }
    }

    pub async fn configure(&self, key: Key, value: Value) -> Result<Sequenced<Value>, ClientError> {
        let (session, generation) = self.binding()?;
        match key {
            Key::Mode => {
                let mode = value.as_str().unwrap_or_default().to_owned();
                let request = SetSessionModeRequest::new(session, mode.clone());
                self.call(
                    AGENT_METHOD_NAMES.session_set_mode,
                    request,
                    Binds::Mode { mode, generation },
                )
                .await
            }
            Key::Option(id) => {
                let request =
                    SetSessionConfigOptionRequest::new(session, id.clone(), wire_value(&value));
                self.call(
                    AGENT_METHOD_NAMES.session_set_config_option,
                    request,
                    Binds::Config { generation },
                )
                .await
            }
        }
    }

    pub fn cancel(&self) -> Result<(), ClientError> {
        let notification = Notification {
            method: AGENT_METHOD_NAMES.session_cancel.into(),
            params: Some(CancelNotification::new(self.session()?)),
        };
        self.send(notification)
    }

    /// An option the agent did not offer is refused and the ask stays open,
    /// so a bad reply never answers for the user.
    pub fn reply_permission(&self, ask_id: u64, reply: PermissionReply) -> Result<(), ClientError> {
        let ask = {
            let mut shared = self.lock();
            let ask = shared
                .asks
                .get(&ask_id)
                .ok_or(ClientError::UnknownAsk(ask_id))?;
            if let PermissionReply::Selected(option) = &reply
                && !ask.options.contains(option)
            {
                return Err(ClientError::UnofferedOption(option.clone()));
            }
            shared
                .asks
                .remove(&ask_id)
                .ok_or(ClientError::UnknownAsk(ask_id))?
        };
        let outcome = match reply {
            PermissionReply::Selected(option) => {
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option))
            }
            PermissionReply::Cancelled => RequestPermissionOutcome::Cancelled,
        };
        self.send(Response::<_, AcpError>::new(
            ask.rpc_id,
            Ok(RequestPermissionResponse::new(outcome)),
        ))
    }

    /// Dropped before the answer, as a timed-out wait drops it, the call
    /// forgets its waiter.
    pub async fn session_operation(
        &self,
        operation: SessionOperation,
    ) -> Result<Value, ClientError> {
        self.request(SESSION_METHOD, operation).await
    }

    pub async fn plan_read(&self, session: SessionId) -> Result<PlanText, ClientError> {
        self.request(
            plan::READ_METHOD,
            PlanRead {
                session_id: session,
            },
        )
        .await
    }

    /// `revision` is the one the text was read at; the agent refuses the
    /// write when the plan changed since, or while a prompt runs.
    pub async fn plan_write(
        &self,
        session: SessionId,
        text: String,
        revision: Option<String>,
    ) -> Result<(), ClientError> {
        let write = PlanWrite {
            session_id: session,
            text,
            revision,
        };
        self.request::<_, Value>(plan::WRITE_METHOD, write).await?;
        Ok(())
    }

    /// For a call nothing binds on its answer: a waiter left behind by a
    /// dropped call would stay until the link closes.
    async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, ClientError> {
        let id = self.next_request_id();
        let _forget = Forget {
            shared: &self.shared,
            id: id.clone(),
        };
        Ok(self
            .call_as(id, method, params, Binds::Nothing)
            .await?
            .value)
    }

    /// `None` dismisses the form.
    pub fn reply_question(
        &self,
        ask: u64,
        answers: Option<Vec<Vec<String>>>,
    ) -> Result<(), ClientError> {
        let rpc_id = self
            .lock()
            .questions
            .remove(&ask)
            .ok_or(ClientError::UnknownAsk(ask))?;
        let response = match answers {
            Some(answers) => elicitation::submitted(answers),
            None => elicitation::dismissed(),
        };
        self.send(Response::<_, AcpError>::new(rpc_id, Ok(response)))
    }

    async fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
        binds: Binds,
    ) -> Result<Sequenced<R>, ClientError> {
        self.call_as(self.next_request_id(), method, params, binds)
            .await
    }

    fn next_request_id(&self) -> RequestId {
        RequestId::Number(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    async fn call_as<P: Serialize, R: DeserializeOwned>(
        &self,
        id: RequestId,
        method: &str,
        params: P,
        binds: Binds,
    ) -> Result<Sequenced<R>, ClientError> {
        let rx = self.send_call(id, method, params, binds)?;
        match rx.recv_async().await {
            Ok((seq, Ok(result))) => Ok(Sequenced {
                seq,
                value: serde_json::from_value(result)?,
            }),
            Ok((_, Err(error))) => Err(ClientError::Rpc(error)),
            Err(_) => Err(ClientError::Closed),
        }
    }

    fn send_call(
        &self,
        id: RequestId,
        method: &str,
        params: impl Serialize,
        binds: Binds,
    ) -> Result<Receiver<RpcReply>, ClientError> {
        let (tx, rx) = flume::bounded(1);
        {
            let mut shared = self.lock();
            if shared.closed {
                return Err(ClientError::Closed);
            }
            let open = match (&binds, method) {
                (_, method) if method == AGENT_METHOD_NAMES.session_prompt => Some(Open::Turn),
                (Binds::Event { .. } | Binds::Config { .. } | Binds::Mode { .. }, _) => {
                    Some(Open::Setting)
                }
                _ => None,
            };
            let settled = open.map(|open| shared.watch(&id, open));
            shared.waiters.insert(
                id.clone(),
                Waiter {
                    reply: tx,
                    binds,
                    _settled: settled,
                },
            );
        }
        self.send(Request {
            id: id.clone(),
            method: method.into(),
            params: Some(params),
        })
        .inspect_err(|_| {
            let mut shared = self.lock();
            shared.waiters.remove(&id);
            shared.open.remove(&id);
        })?;
        Ok(rx)
    }

    /// `/btw` for `session`, which the caller reads with
    /// [`Self::bound_session`] when it decides to ask, so a later `/new`
    /// cannot redirect the question. `chunks` takes the answer as it
    /// streams, every delta before this returns, and must be unbounded.
    pub async fn btw(
        &self,
        session: SessionId,
        question: String,
        chunks: Sender<SideAnswer>,
    ) -> Result<(), ClientError> {
        let request = BtwRequest {
            session_id: session,
            question,
        };
        let id = self.next_request_id();
        // Dropped before the answer, the call takes its sink with it.
        let _forget = Forget {
            shared: &self.shared,
            id: id.clone(),
        };
        let _: Sequenced<Value> = self
            .call_as(id, BTW_METHOD, request, Binds::Chunks(chunks))
            .await?;
        Ok(())
    }

    fn send(&self, message: impl Serialize) -> Result<(), ClientError> {
        if self.lock().closed {
            return Err(ClientError::Closed);
        }
        let line =
            serde_json::to_string(&JsonRpcMessage::wrap(message)).map_err(ClientError::Encode)?;
        self.out.send(line).map_err(|_| ClientError::Closed)
    }

    /// The agent's provider quota for `session`, read as `btw` reads it.
    pub async fn usage_quota(&self, session: SessionId) -> Result<QuotaAnswer, ClientError> {
        self.request(
            QUOTA_METHOD,
            QuotaRequest {
                session_id: session,
            },
        )
        .await
    }

    /// The terminal login ran, so the agent rebuilds what its old
    /// credentials connected.
    pub fn auth_changed(&self) -> Result<(), ClientError> {
        self.send(Notification {
            method: Arc::from(login::CHANGED_METHOD),
            params: Some(Value::Object(serde_json::Map::new())),
        })
    }

    pub fn bound_session(&self) -> Result<SessionId, ClientError> {
        self.session()
    }

    fn session(&self) -> Result<SessionId, ClientError> {
        self.lock().session.clone().ok_or(ClientError::NoSession)
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }
}

async fn all_answered(answered: Vec<Receiver<()>>, grace: Duration) -> bool {
    if answered.is_empty() {
        return true;
    }
    future::or(
        async {
            for answer in answered {
                let _ = answer.recv_async().await;
            }
            true
        },
        async {
            tokio::time::sleep(grace).await;
            false
        },
    )
    .await
}

async fn read_loop(
    rx: Receiver<String>,
    out: Sender<String>,
    shared: Arc<Mutex<Shared>>,
    events: Sender<ClientEvent>,
) {
    let mut seq = 0;
    while let Ok(line) = rx.recv_async().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        seq += 1;
        if let Some(event) = handle_line(seq, line, &out, &shared) {
            let _ = events.send_async(event).await;
        }
    }
    {
        let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.closed = true;
        shared.waiters.clear();
        shared.queue_sinks.clear();
        shared.open.clear();
        shared.asks.clear();
        shared.questions.clear();
    }
    let _ = events.send_async(ClientEvent::Closed).await;
}

fn handle_line(
    seq: u64,
    line: &str,
    out: &Sender<String>,
    shared: &Mutex<Shared>,
) -> Option<ClientEvent> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(e) => return Some(ClientEvent::Malformed(format!("not JSON: {e}"))),
    };
    let id = message
        .get("id")
        .and_then(|id| serde_json::from_value::<RequestId>(id.clone()).ok());
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match (method, id) {
        (Some(method), Some(id)) => handle_request(&method, id, message, out, shared),
        (Some(method), None) => handle_notification(seq, &method, message, shared),
        (None, Some(id)) => resolve(seq, id, message, shared),
        (None, None) => Some(ClientEvent::Malformed(format!(
            "neither a request, a notification nor a response: {line}"
        ))),
    }
}

fn resolve(seq: u64, id: RequestId, message: Value, shared: &Mutex<Shared>) -> Option<ClientEvent> {
    let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Waiter {
        reply: waiter,
        binds,
        _settled,
    }) = shared.waiters.remove(&id)
    else {
        warn!(%id, "response for no pending request");
        return None;
    };
    let result = match serde_json::from_value::<Response<Value>>(message) {
        Ok(Response::Result { result, .. }) => Ok(result),
        Ok(Response::Error { error, .. }) => Err(error),
        Err(e) => Err(AcpError::internal_error().data(Value::String(e.to_string()))),
    };
    shared.open.remove(&id);
    let marker = match binds {
        Binds::Nothing | Binds::Chunks(_) => None,
        Binds::Session => {
            if let Ok(result) = &result
                && let Ok(session) = serde_json::from_value::<NewSessionResponse>(result.clone())
            {
                shared.bind(Some(session.session_id));
                shared.confirmed = Confirmed::opened(session.config_options, session.modes);
            }
            None
        }
        Binds::Event {
            tag,
            generation,
            config,
            ..
        } => {
            if let Ok(value) = &result
                && let Ok(receipt) = serde_json::from_value::<QueueReceipt>(value.clone())
                && !matches!(receipt.status, QueueStatus::Queued | QueueStatus::Running)
            {
                shared.forget_queue(receipt.id);
            }
            if config {
                confirm_options(&mut shared, generation, &result);
            }
            drop(shared);
            return Some(ClientEvent::Answer { seq, tag, result });
        }
        Binds::Config { generation, .. } => {
            confirm_options(&mut shared, generation, &result);
            None
        }
        Binds::Mode {
            mode, generation, ..
        } => {
            if result.is_ok() && generation == shared.generation {
                shared.confirmed.mode = Some(mode);
            }
            None
        }
        Binds::Replay { target, previous } => {
            let loaded = result.as_ref().ok().and_then(|result| {
                serde_json::from_value::<LoadSessionResponse>(result.clone()).ok()
            });
            match loaded {
                Some(loaded) => {
                    shared.confirmed = Confirmed::opened(loaded.config_options, loaded.modes);
                }
                None if shared.session.as_ref() == Some(&target) => shared.bind(previous),
                None => {}
            }
            Some(ClientEvent::ReplayEnd)
        }
    };
    drop(shared);
    let _ = waiter.send((seq, result));
    marker
}

/// A config option change the agent answered under the binding of
/// {generation} gives the full set it holds; one answered for an earlier
/// binding says nothing about this one.
fn confirm_options(shared: &mut Shared, generation: u64, result: &Result<Value, AcpError>) {
    if generation == shared.generation
        && let Ok(result) = result
        && let Ok(answer) = serde_json::from_value::<SetSessionConfigOptionResponse>(result.clone())
    {
        shared.confirmed.config_options = answer.config_options;
    }
}

fn handle_request(
    method: &str,
    id: RequestId,
    message: Value,
    out: &Sender<String>,
    shared: &Mutex<Shared>,
) -> Option<ClientEvent> {
    if method == CLIENT_METHOD_NAMES.elicitation_create {
        return question(id, message, out, shared);
    }
    if method != CLIENT_METHOD_NAMES.session_request_permission {
        debug!(
            method,
            "refusing an agent request this client does not implement"
        );
        reply(
            out,
            Response::<Value>::new(id, Err(AcpError::method_not_found())),
        );
        return None;
    }
    let request = match params::<RequestPermissionRequest>(message) {
        Ok(request) => request,
        Err(e) => {
            reply(
                out,
                Response::<Value>::new(id, Err(AcpError::invalid_params())),
            );
            return Some(ClientEvent::Malformed(format!("permission request: {e}")));
        }
    };
    let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
    if shared.session.as_ref() != Some(&request.session_id) {
        drop(shared);
        let cancelled = RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled);
        reply(out, Response::<_, AcpError>::new(id, Ok(cancelled)));
        return None;
    }
    let ask_id = shared.next_ask;
    shared.next_ask += 1;
    shared.asks.insert(
        ask_id,
        Ask {
            rpc_id: id,
            options: request
                .options
                .iter()
                .map(|o| o.option_id.clone())
                .collect(),
        },
    );
    Some(ClientEvent::Permission(PermissionAsk {
        id: ask_id,
        request,
    }))
}

/// This client draws only the questions of the extension, so it advertises
/// no standard form and declines any other elicitation. A question for
/// another session is dismissed.
fn question(
    id: RequestId,
    message: Value,
    out: &Sender<String>,
    shared: &Mutex<Shared>,
) -> Option<ClientEvent> {
    let request = match params::<CreateElicitationRequest>(message) {
        Ok(request) => request,
        Err(e) => {
            reply(
                out,
                Response::<Value>::new(id, Err(AcpError::invalid_params())),
            );
            return Some(ClientEvent::Malformed(format!("elicitation request: {e}")));
        }
    };
    let Some(mut questions) = extension_questions(&request) else {
        let declined = CreateElicitationResponse::new(ElicitationAction::Decline);
        reply(out, Response::<_, AcpError>::new(id, Ok(declined)));
        return None;
    };
    let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
    let bound = match request.scope() {
        ElicitationScope::Session(scope) => shared.session.as_ref() == Some(&scope.session_id),
        _ => false,
    };
    if !bound {
        drop(shared);
        reply(
            out,
            Response::<_, AcpError>::new(id, Ok(elicitation::dismissed())),
        );
        return None;
    }
    let ask = shared.next_ask;
    shared.next_ask += 1;
    shared.questions.insert(ask, id);
    // The form draws every one of these strings.
    sanitize::sanitize_value(&mut questions);
    Some(ClientEvent::Question(QuestionAsk { id: ask, questions }))
}

/// A poll for another session, or one that cannot be read, takes nothing.
struct Forget<'a> {
    shared: &'a Mutex<Shared>,
    id: RequestId,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.waiters.remove(&self.id);
    }
}

fn handle_notification(
    seq: u64,
    method: &str,
    mut message: Value,
    shared: &Mutex<Shared>,
) -> Option<ClientEvent> {
    if method == crate::session::RESTORE_METHOD {
        return match params::<Box<StoredSession>>(message) {
            Ok(session)
                if shared
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .session
                    .as_ref()
                    .is_some_and(|id| id.to_string() == session.id.to_string()) =>
            {
                Some(ClientEvent::Restored(session))
            }
            Ok(_) => None,
            Err(error) => Some(ClientEvent::RestoreError(error)),
        };
    }
    if method == crate::session::TASK_UPDATE_METHOD {
        return match params::<crate::session::TaskUpdate>(message) {
            Ok(update)
                if shared
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .session
                    .as_ref()
                    == Some(&update.session_id) =>
            {
                Some(ClientEvent::Task(update))
            }
            Ok(_) => None,
            Err(error) => Some(ClientEvent::Malformed(format!("task update: {error}"))),
        };
    }
    if method == QUEUE_UPDATE_METHOD {
        return match params::<QueueUpdate>(message) {
            Ok(mut update) => {
                if let Some(error) = &mut update.receipt.error {
                    *error = sanitize::sanitize_display(error).into_owned();
                }
                let mut shared = shared.lock().unwrap_or_else(|error| error.into_inner());
                if shared.session.as_ref() != Some(&update.session_id) {
                    return None;
                }
                if matches!(
                    update.receipt.status,
                    QueueStatus::Queued | QueueStatus::Running
                ) {
                    if !shared.queue_sinks.contains_key(&update.receipt.id) {
                        let (settled, answered) = flume::bounded(1);
                        shared.open.insert(
                            RequestId::from(format!("queue/{}", update.receipt.id)),
                            (Open::Turn, answered),
                        );
                        shared.queue_sinks.insert(update.receipt.id, settled);
                    }
                } else {
                    shared.forget_queue(update.receipt.id);
                }
                Some(ClientEvent::QueueUpdate(update))
            }
            Err(error) => Some(ClientEvent::Malformed(format!("queue update: {error}"))),
        };
    }
    if method == CHUNK_METHOD {
        route_chunk(message, shared);
        return None;
    }
    if method != CLIENT_METHOD_NAMES.session_update {
        debug!(
            method,
            "ignoring an agent notification this client does not use"
        );
        return None;
    }
    tool_data::sanitize_keeping_raw(&mut message);
    let notification = match params::<SessionNotification>(message) {
        Ok(notification) => notification,
        Err(e) => return Some(ClientEvent::Malformed(format!("session update: {e}"))),
    };
    let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
    if shared.session.as_ref() != Some(&notification.session_id) {
        return None;
    }
    match &notification.update {
        SessionUpdate::ConfigOptionUpdate(update) => {
            shared.confirmed.config_options = update.config_options.clone();
        }
        SessionUpdate::CurrentModeUpdate(update) => {
            shared.confirmed.mode = Some(update.current_mode_id.to_string());
        }
        _ => {}
    }
    Some(ClientEvent::Update(seq, notification.update))
}

/// To the sink of the request it names, cleaned like any remote text the
/// screen draws. A chunk for no such request is one whose call is gone.
fn route_chunk(mut message: Value, shared: &Mutex<Shared>) {
    sanitize::sanitize_value(&mut message);
    let chunk = match params::<BtwChunk>(message) {
        Ok(chunk) => chunk,
        Err(error) => {
            warn!(%error, "ignoring an unreadable btw chunk");
            return;
        }
    };
    let shared = shared.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(Waiter {
        binds: Binds::Chunks(sink),
        ..
    }) = shared.waiters.get(&chunk.request_id)
    {
        let _ = sink.send(chunk.delta);
    }
}

fn params<T: DeserializeOwned>(mut message: Value) -> Result<T, serde_json::Error> {
    serde_json::from_value(
        message
            .get_mut("params")
            .map(Value::take)
            .unwrap_or_default(),
    )
}

fn reply(out: &Sender<String>, message: impl Serialize) {
    if let Ok(line) = serde_json::to_string(&JsonRpcMessage::wrap(message)) {
        let _ = out.send(line);
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use agent_client_protocol_schema::v1::{
        ContentChunk, ErrorCode, PromptResponse, StopReason, TextContent,
    };
    use futures_lite::future;
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::methods;

    const SESSION: &str = "session-1";
    const OTHER_SESSION: &str = "session-2";
    const CWD: &str = "/agent/workspace";
    const TEXT: &str = "hello";
    const DIRTY: &str = "\u{1b}[31mhello\u{7}";
    const OTHER_TEXT: &str = "not for this session";
    const ALLOW: &str = "allow-7";
    const REJECT: &str = "reject-7";
    const UNOFFERED: &str = "approve-everything";
    const AGENT_REQUEST_ID: &str = "agent-ask-1";
    const UNSUPPORTED_METHOD: &str = "fs/read_text_file";
    const CLIENT_NAME: &str = "maki-test";
    const CLIENT_VERSION: &str = "0";
    const ECHO_LINE: &str = "{\"echo\":true}";
    const NOT_JSON: &str = "not json";
    const OVER_CAPACITY: usize = EVENT_CAPACITY + 2;
    /// Each child stops reading but keeps stdout open, so only the writer's
    /// failure can tell the client the link is gone.
    const QUIET_CHILD: &str = "exec 0<&-; echo ready; exec sleep 600";
    const CHATTY_CHILD: &str = "exec 0<&-; exec yes ready";

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

        fn respond(&self, request: &Value, result: impl Serialize) {
            self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}));
        }
    }

    fn connect() -> (AcpClient, Receiver<ClientEvent>, Peer) {
        let (client_tx, from_client) = flume::unbounded();
        let (to_client, client_rx) = flume::unbounded();
        let (client, events) = AcpClient::new(Link::new(client_tx, client_rx));
        (
            client,
            events,
            Peer {
                from_client,
                to_client,
            },
        )
    }

    fn update(session: &str, text: &str) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": session,
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}
        }})
    }

    fn permission_request(session: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": AGENT_REQUEST_ID, "method": "session/request_permission", "params": {
            "sessionId": session,
            "toolCall": {"toolCallId": "call-1", "title": "bash"},
            "options": [
                {"optionId": ALLOW, "name": "Allow", "kind": "allow_once"},
                {"optionId": REJECT, "name": "Reject", "kind": "reject_once"},
            ]
        }})
    }

    /// What `maki acp` sends for a question under the extension, or the
    /// standard form alone without it.
    fn question_request(session: &str, extension: bool) -> Value {
        let input = json!({ "questions": [{ "question": "Pick?", "header": "Pick", "options": [{ "label": ALLOW }] }] });
        let request = elicitation::form_request(session, None, &input, extension).unwrap();
        json!({"jsonrpc": "2.0", "id": AGENT_REQUEST_ID, "method": "elicitation/create", "params": request})
    }

    fn text_update(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        ))))
    }

    const PAGE_TWO: &str = "page-2";

    fn listed(ids: &[&str], next: Option<&str>) -> Value {
        let sessions: Vec<_> = ids
            .iter()
            .map(|id| json!({"sessionId": id, "cwd": CWD}))
            .collect();
        json!({"sessions": sessions, "nextCursor": next})
    }

    fn ids(sessions: &[SessionInfo]) -> Vec<String> {
        sessions
            .iter()
            .map(|info| info.session_id.to_string())
            .collect()
    }

    /// The agent orders nothing, so every page is read.
    #[test]
    fn list_all_sessions_follows_the_cursor_to_a_later_page() {
        let _runtime = crate::test_runtime().enter();
        let (client, _events, peer) = connect();
        let (sessions, ()) =
            crate::test_block_on(future::zip(client.list_all_sessions(CWD.into()), async {
                let first = peer.recv().await;
                assert_eq!(first["params"]["cursor"], Value::Null);
                peer.respond(&first, listed(&[SESSION], Some(PAGE_TWO)));
                let second = peer.recv().await;
                assert_eq!(second["params"]["cursor"], PAGE_TWO);
                peer.respond(&second, listed(&[OTHER_SESSION], None));
            }));
        assert_eq!(ids(&sessions.unwrap()), [SESSION, OTHER_SESSION]);
    }

    #[test]
    fn list_all_sessions_stops_at_a_repeated_cursor() {
        let _runtime = crate::test_runtime().enter();
        let (client, _events, peer) = connect();
        let (sessions, ()) =
            crate::test_block_on(future::zip(client.list_all_sessions(CWD.into()), async {
                for _ in 0..2 {
                    let request = peer.recv().await;
                    peer.respond(&request, listed(&[SESSION], Some(PAGE_TWO)));
                }
            }));
        assert_eq!(ids(&sessions.unwrap()), [SESSION, SESSION]);
    }

    /// Opens `SESSION` the way `maki acp` answers it.
    async fn open(client: &AcpClient, peer: &Peer) {
        let info = Implementation::new(CLIENT_NAME, CLIENT_VERSION);
        let (init, ()) = future::zip(client.initialize(info, false), async {
            let request = peer.recv().await;
            assert_eq!(request["method"], "initialize");
            let asked = &request["params"]["_meta"][extensions::META_KEY][tool_data::TOOL_DATA_KEY];
            assert_eq!(asked, &json!(tool_data::TOOL_DATA_VERSION));
            let capabilities = &request["params"]["clientCapabilities"];
            for service in ["/fs/readTextFile", "/fs/writeTextFile", "/terminal"] {
                assert_eq!(
                    capabilities.pointer(service),
                    Some(&json!(false)),
                    "{service}"
                );
            }
            peer.respond(&request, methods::initialize_response(false));
        })
        .await;
        init.unwrap();
        let (session, ()) = future::zip(client.new_session(CWD.into()), async {
            let request = peer.recv().await;
            assert_eq!(request["method"], "session/new");
            assert_eq!(request["params"]["cwd"], CWD);
            peer.respond(&request, methods::new_session_response(SESSION));
        })
        .await;
        assert_eq!(session.unwrap().value.session_id.to_string(), SESSION);
    }

    async fn opened() -> (AcpClient, Receiver<ClientEvent>, Peer) {
        let (client, events, peer) = connect();
        open(&client, &peer).await;
        (client, events, peer)
    }

    async fn next_event(events: &Receiver<ClientEvent>) -> ClientEvent {
        events.recv_async().await.unwrap()
    }

    const QUESTION: &str = "why sqlite?";
    const ANSWER: &str = "it is embedded";

    fn chunk(request: &Value, text: &str) -> Value {
        json!({"jsonrpc": "2.0", "method": CHUNK_METHOD,
            "params": {"requestId": request["id"], "text": text}})
    }

    fn answers(sink: &Receiver<SideAnswer>) -> Vec<String> {
        sink.try_iter()
            .map(|answer| match answer {
                SideAnswer::Text(text) | SideAnswer::Thinking(text) => text,
            })
            .collect()
    }

    /// Chunks come before the answer on the wire, and every one is in the
    /// sink by the time the call returns; a chunk naming no call is dropped.
    #[test]
    fn a_side_answer_streams_into_its_sink_before_the_call_returns() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, _events, peer) = opened().await;
            let (sink, chunks) = flume::unbounded();
            let session = client.bound_session().unwrap();
            let (result, ()) = future::zip(client.btw(session, QUESTION.into(), sink), async {
                let request = peer.recv().await;
                assert_eq!(request["method"], BTW_METHOD);
                assert_eq!(request["params"]["sessionId"], SESSION);
                assert_eq!(request["params"]["question"], QUESTION);
                peer.send(chunk(&json!({"id": AGENT_REQUEST_ID}), OTHER_TEXT));
                peer.send(chunk(&request, ANSWER));
                peer.send(chunk(&request, DIRTY));
                peer.send(json!({"jsonrpc": "2.0", "method": CHUNK_METHOD,
                    "params": {"requestId": request["id"], "thinking": DIRTY}}));
                peer.respond(&request, json!({}));
            })
            .await;
            result.unwrap();
            assert_eq!(answers(&chunks), [ANSWER, TEXT, TEXT]);
        });
    }

    #[test]
    fn a_failed_side_answer_returns_its_error() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, _events, peer) = opened().await;
            let session = client.bound_session().unwrap();
            let (result, ()) = future::zip(
                client.btw(session, QUESTION.into(), flume::unbounded().0),
                async {
                    let request = peer.recv().await;
                    peer.send(json!({"jsonrpc": "2.0", "id": request["id"],
                        "error": {"code": -32603, "message": "provider is down"}}));
                },
            )
            .await;
            assert!(matches!(result, Err(ClientError::Rpc(_))));
        });
    }

    #[test]
    fn a_quota_request_names_its_session_and_reads_the_answer() {
        let _runtime = crate::test_runtime().enter();
        const PROVIDER: &str = "Anthropic";
        crate::test_block_on(async {
            let (client, _events, peer) = opened().await;
            let session = client.bound_session().unwrap();
            let (answer, ()) = future::zip(client.usage_quota(session), async {
                let request = peer.recv().await;
                assert_eq!(request["method"], QUOTA_METHOD);
                assert_eq!(request["params"]["sessionId"], SESSION);
                peer.respond(&request, json!({"provider": PROVIDER, "unsupported": {}}));
            })
            .await;
            let answer = answer.unwrap();
            assert_eq!(answer.provider, PROVIDER);
            assert!(matches!(
                answer.state,
                crate::usage::QuotaState::Unsupported {}
            ));
        });
    }

    /// The modal closed while the agent still streams: nothing waits on
    /// the call's sink any more, and the link goes on.
    #[test]
    fn a_dropped_side_question_forgets_its_sink() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, _events, peer) = opened().await;
            let (sink, chunks) = flume::unbounded();
            {
                let session = client.bound_session().unwrap();
                let mut call = pin!(client.btw(session, QUESTION.into(), sink));
                assert!(future::poll_once(call.as_mut()).await.is_none());
            }
            let request = peer.recv().await;
            peer.send(chunk(&request, ANSWER));
            let (result, ()) = future::zip(client.prompt(vec![TEXT.into()], None), async {
                let prompt = peer.recv().await;
                peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
            })
            .await;
            result.unwrap();
            assert!(chunks.is_disconnected(), "the sink went with the call");
        });
    }

    #[test]
    fn prompt_streams_updates_then_resolves() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = opened().await;
            let (response, ()) = future::zip(client.prompt(vec![TEXT.into()], None), async {
                let request = peer.recv().await;
                assert_eq!(request["method"], "session/prompt");
                assert_eq!(request["params"]["sessionId"], SESSION);
                peer.send(update(SESSION, TEXT));
                peer.respond(&request, PromptResponse::new(StopReason::EndTurn));
            })
            .await;
            assert_eq!(response.unwrap().stop_reason, StopReason::EndTurn);
            let ClientEvent::Update(_, got) = next_event(&events).await else {
                panic!("expected the session update");
            };
            assert_eq!(got, text_update(TEXT));
        });
    }

    #[test]
    fn bad_lines_and_other_sessions_do_not_stop_the_reader() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (_client, events, peer) = opened().await;
            peer.to_client.send(NOT_JSON.into()).unwrap();
            peer.send(update(OTHER_SESSION, OTHER_TEXT));
            peer.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SESSION}}));
            peer.send(update(SESSION, DIRTY));
            assert!(matches!(
                next_event(&events).await,
                ClientEvent::Malformed(_)
            ));
            assert!(matches!(
                next_event(&events).await,
                ClientEvent::Malformed(_)
            ));
            let ClientEvent::Update(_, got) = next_event(&events).await else {
                panic!("the reader must keep going after bad input");
            };
            assert_eq!(got, text_update(TEXT));
        });
    }

    /// The agent may send an update right behind its `session/new` answer.
    /// The reader binds the session as it reads the answer, so that update is
    /// not dropped as belonging to no session.
    #[test]
    fn update_right_after_the_new_session_answer_is_kept() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = connect();
            let (session, ()) = future::zip(client.new_session(CWD.into()), async {
                let request = peer.recv().await;
                peer.respond(&request, methods::new_session_response(SESSION));
                peer.send(update(SESSION, TEXT));
            })
            .await;
            assert!(session.is_ok());
            let ClientEvent::Update(_, got) = next_event(&events).await else {
                panic!("the update behind the answer must reach the caller");
            };
            assert_eq!(got, text_update(TEXT));
        });
    }

    /// Updates and responses reach a caller on different channels, so the
    /// sequence numbers are what keep their wire order.
    #[test]
    fn sequence_numbers_follow_the_wire() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = opened().await;
            let (response, ()) = future::zip(client.configure(Key::Mode, json!(TEXT)), async {
                let request = peer.recv().await;
                peer.send(update(SESSION, TEXT));
                peer.respond(&request, json!({}));
                peer.send(update(SESSION, TEXT));
            })
            .await;
            let response = response.unwrap();
            let ClientEvent::Update(before, _) = next_event(&events).await else {
                panic!("expected the update before the response");
            };
            let ClientEvent::Update(after, _) = next_event(&events).await else {
                panic!("expected the update after the response");
            };
            assert!(before < response.seq && response.seq < after);
        });
    }

    fn load<'a>(
        client: &'a AcpClient,
        events: &'a Receiver<ClientEvent>,
        session: &str,
    ) -> impl Future<Output = Result<Loaded, ClientError>> + use<'a> {
        client.load_session(events, SessionId::from(session.to_owned()), CWD.into())
    }

    /// The replay comes before the answer, so the session is bound before
    /// the request; the update after the answer stays for the live loop.
    #[test_case(2 ; "short_replay")]
    #[test_case(OVER_CAPACITY ; "full_event_channel")]
    fn load_keeps_the_replay_in_order_and_leaves_what_follows(count: usize) {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = connect();
            let (loaded, ()) = future::zip(load(&client, &events, SESSION), async {
                let request = peer.recv().await;
                assert_eq!(request["method"], "session/load");
                assert_eq!(request["params"]["sessionId"], SESSION);
                assert_eq!(request["params"]["cwd"], CWD);
                for index in 0..count {
                    peer.send(update(SESSION, &index.to_string()));
                }
                peer.respond(&request, methods::load_session_response());
                peer.send(update(SESSION, TEXT));
            })
            .await;
            let loaded = loaded.unwrap();
            assert_eq!(
                loaded.replay,
                (0..count)
                    .map(|index| text_update(&index.to_string()))
                    .collect::<Vec<_>>()
            );
            assert_eq!(loaded.unreadable, 0);
            let ClientEvent::Update(seq, got) = next_event(&events).await else {
                panic!("expected the update after the answer");
            };
            assert!(seq > loaded.response.seq);
            assert_eq!(got, text_update(TEXT));
        });
    }

    #[test]
    fn a_corrupt_native_restore_fails_the_load_instead_of_opening_an_empty_chat() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = connect();
            let mut snapshot = serde_json::to_value(StoredSession::new("test/model", CWD)).unwrap();
            let id = snapshot["id"].as_str().unwrap().to_owned();
            snapshot["messages"] = json!([{ "role": "not_a_role", "content": [] }]);
            let (loaded, ()) = future::zip(load(&client, &events, &id), async {
                let request = peer.recv().await;
                peer.send(json!({"jsonrpc":"2.0", "method":crate::session::RESTORE_METHOD, "params":snapshot}));
                peer.respond(&request, methods::load_session_response());
            }).await;
            assert!(matches!(loaded, Err(ClientError::Decode(_))));
        });
    }

    /// A refused load gives the open session back before the reader reads
    /// on, so an update the agent sends for it right behind the refusal is
    /// kept, and the refused one's updates are dropped again.
    #[test]
    fn a_failed_load_restores_the_session_bound_before() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = opened().await;
            let (loaded, ()) = future::zip(load(&client, &events, OTHER_SESSION), async {
                let request = peer.recv().await;
                peer.send(update(OTHER_SESSION, OTHER_TEXT));
                peer.send(json!({"jsonrpc": "2.0", "id": request["id"],
                    "error": {"code": -32002, "message": "not found"}}));
                peer.send(update(OTHER_SESSION, OTHER_TEXT));
                peer.send(update(SESSION, TEXT));
            })
            .await;
            assert!(matches!(loaded, Err(ClientError::Rpc(_))));
            let ClientEvent::Update(_, got) = next_event(&events).await else {
                panic!("expected the bound session's update");
            };
            assert_eq!(got, text_update(TEXT));
        });
    }

    /// The request never leaves, so no answer or marker follows and the
    /// load must not wait for one.
    #[cfg(unix)]
    #[test]
    fn a_load_that_cannot_be_encoded_fails_at_once_and_keeps_the_session() {
        let _runtime = crate::test_runtime().enter();
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        crate::test_block_on(async {
            let (client, events, peer) = opened().await;
            let cwd = PathBuf::from(OsString::from_vec(vec![0xff]));
            let loaded = client
                .load_session(&events, SessionId::from(OTHER_SESSION), cwd)
                .await;
            assert!(matches!(loaded, Err(ClientError::Encode(_))));
            peer.send(update(SESSION, TEXT));
            let ClientEvent::Update(_, got) = next_event(&events).await else {
                panic!("expected the bound session's update");
            };
            assert_eq!(got, text_update(TEXT));
        });
    }

    #[test]
    fn a_permission_request_during_a_load_is_cancelled() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = connect();
            let (loaded, ()) = future::zip(load(&client, &events, SESSION), async {
                let request = peer.recv().await;
                peer.send(permission_request(SESSION));
                let reply = peer.recv().await;
                assert_eq!(reply["id"], AGENT_REQUEST_ID);
                assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
                peer.respond(&request, methods::load_session_response());
            })
            .await;
            assert!(loaded.unwrap().replay.is_empty());
        });
    }

    const PICKED_MODEL: &str = "zai/glm-4.6";
    #[test]
    fn resumed_queue_updates_keep_their_messages_and_asks_until_the_ui_attaches() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = connect();
            let id = MakiId::generate();
            let notification = |status| {
                json!({ "jsonrpc": "2.0", "method": QUEUE_UPDATE_METHOD,
                "params": QueueUpdate { session_id: SessionId::from(SESSION), receipt: QueueReceipt {
                    id, status, error: None, input: None,
                } } })
            };
            let (loaded, ()) = future::zip(load(&client, &events, SESSION), async {
                let request = peer.recv().await;
                peer.send(notification(QueueStatus::Running));
                peer.send(update(SESSION, TEXT));
                peer.send(permission_request(SESSION));
                peer.respond(&request, methods::load_session_response());
            })
            .await;
            let loaded = loaded.unwrap();
            assert!(loaded.replay.is_empty());
            assert!(matches!(
                loaded.early.as_slice(),
                [
                    ClientEvent::QueueUpdate(_),
                    ClientEvent::Update(..),
                    ClientEvent::Permission(_)
                ]
            ));
            let ClientEvent::Permission(ask) = &loaded.early[2] else {
                unreachable!()
            };
            client
                .reply_permission(ask.id, PermissionReply::Cancelled)
                .unwrap();
            peer.recv().await;
            let (ended, ()) = future::zip(client.end_turns(TURN_GRACE), async {
                assert_eq!(peer.recv().await["method"], "session/cancel");
                peer.send(notification(QueueStatus::Cancelled));
            })
            .await;
            assert!(ended);
        });
    }

    #[test]
    fn a_finished_queued_run_is_not_cancelled_again_on_reload() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = opened().await;
            let id = MakiId::generate();
            for status in [QueueStatus::Running, QueueStatus::Done] {
                peer.send(json!({ "jsonrpc": "2.0", "method": QUEUE_UPDATE_METHOD,
                    "params": QueueUpdate { session_id: SESSION.into(), receipt: QueueReceipt {
                        id, status, error: None, input: None,
                    } } }));
                assert!(matches!(
                    next_event(&events).await,
                    ClientEvent::QueueUpdate(_)
                ));
            }
            assert!(client.end_turns(TURN_GRACE).await);
            assert!(peer.from_client.is_empty());
        });
    }

    const TURN_GRACE: Duration = Duration::from_secs(5);

    /// An ask the caller took from the channel and never answered is
    /// still answered, so the agent's turn does not wait on it.
    #[test]
    fn retiring_answers_an_ask_the_caller_took() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = opened().await;
            peer.send(permission_request(SESSION));
            assert!(matches!(
                next_event(&events).await,
                ClientEvent::Permission(_)
            ));

            client.retire_requests();

            let reply = peer.recv().await;
            assert_eq!(reply["id"], AGENT_REQUEST_ID);
            assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
        });
    }

    /// A prompt whose caller is gone is cancelled and waited for, and the
    /// updates of its turn are on the channel by the time it ends.
    #[test]
    fn ending_turns_cancels_and_waits_for_a_dropped_prompt() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = opened().await;
            let client = Arc::new(client);
            let prompting = Arc::clone(&client);
            let turn = tokio::spawn(async move { prompting.prompt(Vec::new(), None).await });
            let prompt = peer.recv().await;
            assert_eq!(prompt["method"], "session/prompt");
            turn.abort();
            let _ = turn.await;

            let (ended, ()) = future::zip(client.end_turns(TURN_GRACE), async {
                let cancel = peer.recv().await;
                assert_eq!(cancel["method"], "session/cancel");
                peer.send(update(SESSION, TEXT));
                peer.respond(&prompt, json!({"stopReason": "cancelled"}));
            })
            .await;

            assert!(ended);
            let ClientEvent::Update(_, got) = events.try_recv().unwrap() else {
                panic!("the turn's update must be queued before it ends");
            };
            assert_eq!(got, text_update(TEXT));
            assert!(client.end_turns(TURN_GRACE).await, "nothing left to end");
        });
    }

    /// The confirmed options follow the session's answer, an update, and
    /// the answer to a change; the mode follows a confirmed switch.
    #[test]
    fn the_confirmed_state_follows_the_agent() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = opened().await;
            assert_eq!(
                client.confirmed().mode.as_deref(),
                Some(methods::MODE_BUILD)
            );
            let options = |current: &str| {
                vec![methods::model_config_option(
                    current,
                    &[PICKED_MODEL.to_owned()],
                )]
            };
            peer.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": SESSION,
                "update": {"sessionUpdate": "config_option_update", "configOptions": options(TEXT)}
            }}));
            let _ = next_event(&events).await;
            assert_eq!(client.confirmed().config_options, options(TEXT));

            let (set, ()) = future::zip(
                client.configure(
                    Key::Option(methods::MODEL_CONFIG_ID.into()),
                    json!(PICKED_MODEL),
                ),
                async {
                    let request = peer.recv().await;
                    peer.respond(&request, json!({"configOptions": options(PICKED_MODEL)}));
                },
            )
            .await;
            set.unwrap();
            let (mode, ()) = future::zip(
                client.configure(Key::Mode, json!(methods::MODE_PLAN)),
                async {
                    let request = peer.recv().await;
                    peer.respond(&request, json!({}));
                },
            )
            .await;
            mode.unwrap();

            let confirmed = client.confirmed();
            assert_eq!(confirmed.config_options, options(PICKED_MODEL));
            assert_eq!(confirmed.mode.as_deref(), Some(methods::MODE_PLAN));
        });
    }

    /// A setting answered after `/new` bound another session was asked of
    /// the old one, so it confirms nothing about the new one.
    #[test]
    fn an_answer_for_an_earlier_session_confirms_nothing() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, _events, peer) = opened().await;
            let picked = vec![methods::model_config_option(PICKED_MODEL, &[])];
            let settings = future::zip(
                client.configure(
                    Key::Option(methods::MODEL_CONFIG_ID.into()),
                    json!(PICKED_MODEL),
                ),
                client.configure(Key::Mode, json!(methods::MODE_PLAN)),
            );
            let ((set, mode), ()) = future::zip(settings, async {
                let first = peer.recv().await;
                let second = peer.recv().await;
                let (created, ()) = future::zip(client.new_session(CWD.into()), async {
                    let request = peer.recv().await;
                    peer.respond(&request, methods::new_session_response(OTHER_SESSION));
                })
                .await;
                created.unwrap();
                for request in [first, second] {
                    match request["method"].as_str() {
                        Some("session/set_mode") => peer.respond(&request, json!({})),
                        _ => peer.respond(&request, json!({"configOptions": picked})),
                    }
                }
            })
            .await;
            set.unwrap();
            mode.unwrap();

            let confirmed = client.confirmed();
            assert!(confirmed.config_options.is_empty(), "{confirmed:?}");
            assert_eq!(confirmed.mode.as_deref(), Some(methods::MODE_BUILD));
        });
    }

    /// A change whose caller is gone is still waited for, and what the
    /// agent answers is then confirmed.
    #[test]
    fn settling_waits_for_a_dropped_setting() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, _events, peer) = opened().await;
            let client = Arc::new(client);
            let setting = Arc::clone(&client);
            let change = tokio::spawn(async move {
                setting
                    .configure(
                        Key::Option(methods::MODEL_CONFIG_ID.into()),
                        json!(PICKED_MODEL),
                    )
                    .await
            });
            let request = peer.recv().await;
            change.abort();
            let _ = change.await;
            let picked = vec![methods::model_config_option(PICKED_MODEL, &[])];

            let (settled, ()) = future::zip(client.settle_settings(TURN_GRACE), async {
                peer.respond(&request, json!({"configOptions": picked}));
            })
            .await;

            assert!(settled);
            assert_eq!(client.confirmed().config_options, picked);
        });
    }

    #[test]
    fn a_closed_link_fails_the_load() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = connect();
            let Peer {
                from_client,
                to_client,
            } = peer;
            let (loaded, ()) = future::zip(load(&client, &events, SESSION), async {
                from_client.recv_async().await.unwrap();
                drop(to_client);
            })
            .await;
            assert!(matches!(loaded, Err(ClientError::Closed)));
        });
    }

    #[test_case(Some(vec![vec![ALLOW.to_owned()]]), "accept" ; "submitted")]
    #[test_case(None, "cancel" ; "dismissed")]
    fn a_question_is_answered_once(answers: Option<Vec<Vec<String>>>, action: &str) {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = opened().await;
            peer.send(question_request(SESSION, true));
            let ClientEvent::Question(ask) = next_event(&events).await else {
                panic!("expected a question ask");
            };
            assert_eq!(ask.questions[0]["header"], "Pick");

            client.reply_question(ask.id, answers.clone()).unwrap();
            let answer = peer.recv().await;
            assert_eq!(answer["id"], AGENT_REQUEST_ID);
            assert_eq!(answer["result"]["action"], action);
            let raw = answer["result"].to_string();
            let input =
                question_request(SESSION, true)["params"]["_meta"]["maki"]["question"].clone();
            let expected = match answers {
                Some(_) => format!("Pick: {ALLOW}"),
                None => elicitation::DISMISSED.to_owned(),
            };
            assert_eq!(elicitation::format_response(&input, &raw), expected);

            let again = client.reply_question(ask.id, None);
            assert!(matches!(again, Err(ClientError::UnknownAsk(_))));
        });
    }

    /// A wait that timed out drops the call, which must not leave its waiter
    /// for the link's lifetime.
    #[test]
    fn a_dropped_plan_call_forgets_its_waiter() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, _events, peer) = opened().await;
            let mut read = Box::pin(client.plan_read(SessionId::from(SESSION)));
            assert!(future::poll_once(read.as_mut()).await.is_none());
            assert_eq!(peer.recv().await["method"], plan::READ_METHOD);
            drop(read);
            assert!(client.lock().waiters.is_empty());
        });
    }

    #[test]
    fn question_text_is_sanitized() {
        let _runtime = crate::test_runtime().enter();
        const DIRTY: &str = "\u{1b}[31mhello\u{7}";
        const CLEAN: &str = "hello";
        let field = |text: &str| json!({ "question": text, "header": text, "options": [{ "label": text, "description": text }] });
        let input = json!({ "questions": [field(DIRTY)] });
        let request = elicitation::form_request(SESSION, None, &input, true).unwrap();
        crate::test_block_on(async {
            let (_client, events, peer) = opened().await;
            peer.send(json!({"jsonrpc": "2.0", "id": AGENT_REQUEST_ID, "method": "elicitation/create", "params": request}));
            let ClientEvent::Question(ask) = next_event(&events).await else {
                panic!("expected a question ask");
            };
            assert_eq!(ask.questions, json!([field(CLEAN)]));
        });
    }

    /// A plain form is declined, and a question for another session is
    /// dismissed, both without an event.
    #[test_case(SESSION, false, "decline" ; "plain_form")]
    #[test_case("another-session", true, "cancel" ; "another_session")]
    fn a_question_this_client_cannot_draw_is_refused(session: &str, extension: bool, action: &str) {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (_client, events, peer) = opened().await;
            peer.send(question_request(session, extension));
            let answer = peer.recv().await;
            assert_eq!(answer["id"], AGENT_REQUEST_ID);
            assert_eq!(answer["result"]["action"], action);
            assert!(events.is_empty());
        });
    }

    #[test]
    fn list_sessions_sends_the_cwd_and_cursor() {
        let _runtime = crate::test_runtime().enter();
        const CURSOR: &str = "page-2";
        crate::test_block_on(async {
            let (client, _events, peer) = connect();
            let (listed, ()) = future::zip(
                client.list_sessions(CWD.into(), Some(CURSOR.to_owned())),
                async {
                    let request = peer.recv().await;
                    assert_eq!(request["method"], "session/list");
                    assert_eq!(request["params"]["cwd"], CWD);
                    assert_eq!(request["params"]["cursor"], CURSOR);
                    peer.respond(&request, json!({"sessions": []}));
                },
            )
            .await;
            assert!(listed.unwrap().sessions.is_empty());
        });
    }

    #[test]
    fn unsupported_agent_request_is_refused() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (_client, _events, peer) = connect();
            peer.send(json!({"jsonrpc": "2.0", "id": AGENT_REQUEST_ID, "method": UNSUPPORTED_METHOD, "params": {}}));
            let reply = peer.recv().await;
            assert_eq!(reply["id"], AGENT_REQUEST_ID);
            assert_eq!(
                reply["error"]["code"],
                json!(i32::from(ErrorCode::MethodNotFound))
            );
        });
    }

    #[test_case(PermissionReply::Selected(ALLOW.into()), json!({"outcome": "selected", "optionId": ALLOW}) ; "selected")]
    #[test_case(PermissionReply::Cancelled, json!({"outcome": "cancelled"}) ; "cancelled")]
    fn permission_reply_answers_the_agent_once(reply: PermissionReply, outcome: Value) {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = opened().await;
            peer.send(permission_request(SESSION));
            let ClientEvent::Permission(ask) = next_event(&events).await else {
                panic!("expected a permission ask");
            };
            assert_eq!(ask.request.options.len(), 2);

            peer.send(permission_request(OTHER_SESSION));
            let foreign = peer.recv().await;
            assert_eq!(foreign["id"], AGENT_REQUEST_ID);
            assert_eq!(
                foreign["result"]["outcome"],
                json!({"outcome": "cancelled"})
            );
            assert!(events.is_empty());
            let unoffered =
                client.reply_permission(ask.id, PermissionReply::Selected(UNOFFERED.into()));
            assert!(matches!(unoffered, Err(ClientError::UnofferedOption(_))));

            client.reply_permission(ask.id, reply).unwrap();
            let answer = peer.recv().await;
            assert_eq!(answer["id"], AGENT_REQUEST_ID);
            assert_eq!(answer["result"]["outcome"], outcome);

            let again = client.reply_permission(ask.id, PermissionReply::Cancelled);
            assert!(matches!(again, Err(ClientError::UnknownAsk(_))));
        });
    }

    #[test]
    fn cancel_notifies_the_agent() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, _events, peer) = connect();
            assert!(matches!(client.cancel(), Err(ClientError::NoSession)));
            open(&client, &peer).await;
            client.cancel().unwrap();
            let notification = peer.recv().await;
            assert_eq!(notification["method"], "session/cancel");
            assert_eq!(notification["params"]["sessionId"], SESSION);
            assert!(notification.get("id").is_none());
        });
    }

    #[test]
    fn closed_link_fails_the_pending_prompt_and_reports_once() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = opened().await;
            let Peer {
                from_client,
                to_client,
            } = peer;
            let (response, ()) = future::zip(client.prompt(vec![TEXT.into()], None), async {
                from_client.recv_async().await.unwrap();
                drop(to_client);
            })
            .await;
            assert!(matches!(response, Err(ClientError::Closed)));
            assert!(matches!(next_event(&events).await, ClientEvent::Closed));
            assert!(events.recv_async().await.is_err());
            let after = client.prompt(vec![TEXT.into()], None).await;
            assert!(matches!(after, Err(ClientError::Closed)));
            assert!(matches!(client.cancel(), Err(ClientError::Closed)));
        });
    }

    #[test]
    fn closed_outgoing_channel_fails_the_request() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, _events, peer) = connect();
            drop(peer.from_client);
            let info = Implementation::new(CLIENT_NAME, CLIENT_VERSION);
            assert!(matches!(
                client.initialize(info, false).await,
                Err(ClientError::Closed)
            ));
        });
    }

    /// The response queues behind every update of the turn. With the event
    /// buffer full and nobody draining, the reader cannot get past the
    /// updates, so the prompt must stay pending until the caller drains.
    #[test]
    fn prompt_completes_only_while_the_caller_drains() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = opened().await;
            let mut prompt = pin!(client.prompt(vec![TEXT.into()], None));
            assert!(future::poll_once(prompt.as_mut()).await.is_none());
            let request = peer.recv().await;
            for _ in 0..OVER_CAPACITY {
                peer.send(update(SESSION, TEXT));
            }
            peer.respond(&request, PromptResponse::new(StopReason::EndTurn));

            while !events.is_full() {
                future::yield_now().await;
            }
            assert!(future::poll_once(prompt.as_mut()).await.is_none());

            let drain = async {
                for _ in 0..OVER_CAPACITY {
                    next_event(&events).await;
                }
            };
            let (response, ()) = future::zip(prompt, drain).await;
            assert_eq!(response.unwrap().stop_reason, StopReason::EndTurn);
        });
    }

    #[cfg(unix)]
    #[test_case(QUIET_CHILD ; "quiet_stdout")]
    #[test_case(CHATTY_CHILD ; "stdout_keeps_writing")]
    fn write_failure_fails_the_request_while_stdout_stays_open(script: &str) {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            let (client, events) = AcpClient::new(Link::spawn(command).unwrap());
            assert!(matches!(
                next_event(&events).await,
                ClientEvent::Malformed(_)
            ));
            let info = Implementation::new(CLIENT_NAME, CLIENT_VERSION);
            let drain_until_closed =
                async { while !matches!(next_event(&events).await, ClientEvent::Closed) {} };
            let (response, ()) =
                future::zip(client.initialize(info, false), drain_until_closed).await;
            assert!(matches!(response, Err(ClientError::Closed)));
        });
    }

    #[cfg(unix)]
    #[test]
    fn spawned_link_pipes_lines_through_the_child() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let link = Link::spawn(Command::new("cat")).unwrap();
            link.tx.send(ECHO_LINE.into()).unwrap();
            assert_eq!(link.rx.recv_async().await.unwrap().trim(), ECHO_LINE);
        });
    }

    const ROUTED_TAG: u64 = 41;
    /// Long enough that a child which ignores its closed input is still
    /// running when it ends, short enough for a test.
    const GRACE: Duration = Duration::from_millis(200);
    const IGNORES_EOF: &str = "exec 0<&-; exec sleep 600";

    /// A routed answer arrives among the updates, so the caller handles it in
    /// whatever it is waiting on.
    #[test]
    fn a_routed_answer_arrives_on_the_event_channel() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let (client, events, peer) = opened().await;
            client
                .set_config_routed("fast", SessionConfigOptionValue::boolean(true), ROUTED_TAG)
                .unwrap();
            let request = peer.recv().await;
            assert_eq!(request["method"], "session/set_config_option");
            assert_eq!(request["params"]["value"], true);
            peer.send(update(SESSION, TEXT));
            peer.respond(&request, json!({"configOptions": []}));
            assert!(matches!(next_event(&events).await, ClientEvent::Update(..)));
            let ClientEvent::Answer { tag, result, .. } = next_event(&events).await else {
                panic!("expected the routed answer");
            };
            assert_eq!(tag, ROUTED_TAG);
            assert!(result.is_ok());
        });
    }

    /// A child that reads its input to the end exits by itself, and the
    /// shutdown does not wait out the grace for it.
    #[test]
    fn shutdown_lets_a_child_that_reads_to_the_end_exit() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let link = Link::spawn(Command::new("cat")).unwrap();
            let (client, _events) = AcpClient::new(link);
            let started = std::time::Instant::now();
            client.shutdown(Duration::from_secs(30)).await;
            assert!(started.elapsed() < Duration::from_secs(30));
        });
    }

    #[test]
    fn shutdown_kills_a_child_that_ignores_its_closed_input() {
        let _runtime = crate::test_runtime().enter();
        crate::test_block_on(async {
            let mut command = Command::new("sh");
            command.args(["-c", IGNORES_EOF]);
            let link = Link::spawn(command).unwrap();
            let (client, _events) = AcpClient::new(link);
            let started = std::time::Instant::now();
            client.shutdown(GRACE).await;
            assert!(started.elapsed() >= GRACE);
        });
    }

    /// Everything queued before the end marker reaches the child before its
    /// input closes, so the agent hears the last settings before it exits.
    #[test]
    fn shutdown_drains_what_was_queued_before_it() {
        let _runtime = crate::test_runtime().enter();
        let tmp = tempfile::TempDir::new().unwrap();
        let received = tmp.path().join("received");
        let mut command = Command::new("sh");
        command.args(["-c", &format!("cat > {}", received.display())]);
        let link = Link::spawn(command).unwrap();
        link.tx.send(ECHO_LINE.into()).unwrap();
        link.tx.send(END_OF_INPUT.into()).unwrap();
        crate::test_block_on(link.pumps.unwrap().shutdown(Duration::from_secs(30)));
        assert_eq!(
            std::fs::read_to_string(received).unwrap(),
            format!("{ECHO_LINE}\n")
        );
    }
}
