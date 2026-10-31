//! A tab's ACP driver: requests, events, queues and session replacement.

use agent_client_protocol_schema::v1::SessionId;
use maki_types::session::StoredSession;
use std::collections::HashMap;
use std::mem;
use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use crate::EventHandle;
use crate::components::permission_prompt::AnsweredRequest;
use agent_client_protocol_schema::v1::{
    ContentBlock, Error as AcpError, ImageContent, Meta, NewSessionResponse, PermissionOptionKind,
    PromptResponse, SessionUpdate, StopReason, TextContent,
};
use futures_lite::future;
use maki_config::{ToolKey, is_valid_wire_name};
use maki_lua_ui::QuestionFormAnswer;
use maki_storage::sessions::{QueueReceipt, QueueStatus};
use maki_types::cancel::CancelToken;
use maki_types::event::{AgentEvent, Envelope, EventSender, PermissionChoice};
use maki_types::input::{AgentInput, AgentMode};
use maki_types::input::{Compaction as HostCompaction, SessionInput};
use maki_types::message::ContentBlock as MessageBlock;
use maki_types::message::{ImageSource, Message};
use maki_wire::client::{
    AcpClient, ClientError, ClientEvent, PermissionAsk, PermissionReply, QuestionAsk, Sequenced,
};
use maki_wire::commands;
use maki_wire::configuration::{Change, Key, wire_value};
use maki_wire::display::DisplayMapper;
use maki_wire::plan;
use maki_wire::session::SessionOperation;
use maki_wire::settings::WireSettings;
use serde_json::Value;
use tracing::warn;

use super::capabilities::UNAVAILABLE_REMOTE;
use super::remote_models::ModelPick;
use super::remote_plan::PlanWrites;
use super::remote_settings::{RemoteSettings, SESSION_REPLACED, Setting};
use super::shared_queue::{self, Compaction, QueueItem, QueueReceiver, QueueRun};
use super::{SessionBinding, UsageSlot};
use maki_types::cancel::{LiveRun, RunCancels};

pub(crate) const MODEL_NOT_OFFERED: &str = "the agent no longer offers the picked model";
pub(crate) const MODEL_NOT_SET: &str = "The remote agent did not switch to the picked model";
pub(crate) const MODE_NOT_SET: &str = "The remote agent did not switch to the requested mode";
pub(crate) const MALFORMED_UPDATE: &str = "The remote agent sent an update this client cannot read";
pub(crate) const NEW_SESSION_FAILED: &str = "The remote agent did not open a new session";
pub(crate) use maki_wire::configuration::NOT_SET as SETTING_NOT_SET;
const ASK_ID_PREFIX: &str = "remote-ask-";
/// For an ask whose call this client cannot name: the title is shown as the
/// scope either way.
const REMOTE_TOOL: &str = "remote";
pub const BUILD_MODE: &str = "build";
pub const PLAN_MODE: &str = "plan";

/// The mode id used by `session/set_mode`.
fn mode_id(mode: &AgentMode) -> &'static str {
    match mode {
        AgentMode::Build => BUILD_MODE,
        AgentMode::Plan(_) => PLAN_MODE,
    }
}

pub(crate) struct RewindRequest {
    pub session: SessionId,
    pub index: usize,
    pub prompt: String,
    pub reply: flume::Sender<Result<StoredSession, String>>,
}

const REWIND_GRACE: Duration = Duration::from_secs(3);

/// The new input side of a tab whose session `/new` replaces. The loop
/// keeps the connection, so it takes a new queue instead of being respawned,
/// and nothing typed for the new session can reach the old one.
pub(super) struct Rebind {
    pub(super) queue: Arc<QueueReceiver>,
    pub(super) run_id: u64,
    /// The new session's usage. A snapshot the old one published but the
    /// App never drew stays in the old slot, which nobody reads.
    pub(super) usage: UsageSlot,
    /// Bound once the new session is installed, see [`SessionBinding`].
    pub(super) generation: u64,
}

pub(super) struct RemoteAgentLoop {
    client: Arc<AcpClient>,
    events: flume::Receiver<ClientEvent>,
    /// See [`RemoteAgent::early`](super::RemoteAgent::early).
    early: Vec<ClientEvent>,
    mapper: DisplayMapper,
    agent_tx: flume::Sender<Envelope>,
    answer_rx: flume::Receiver<AnsweredRequest>,
    queue: Arc<QueueReceiver>,
    cancels: Arc<RunCancels>,
    asks: HashMap<String, PermissionAsk>,
    /// The client's own Lua host, which draws the agent's questions.
    lua: EventHandle,
    /// Under the plan extension, see [`PlanWrites`].
    plan_writes: Option<PlanWrites>,
    /// The question forms on screen, by the ask they answer; dropping the
    /// sender closes the form.
    forms: Vec<(u64, flume::Sender<()>)>,
    next_ask: u64,
    run_id: u64,
    /// The agent advertised `build` and `plan`, so each prompt carries its
    /// mode to it. Without them the agent's modes are its own business.
    plan_modes: bool,
    /// The run was cancelled while its mode switch settles. An ask then has
    /// no run on screen to answer it, so it is cancelled at once.
    refuse_asks: bool,
    /// The session's spending, read by the App whatever run is current.
    usage: UsageSlot,
    /// Where a new session opens.
    cwd: PathBuf,
    rebind_rx: flume::Receiver<Rebind>,
    rewind_rx: flume::Receiver<RewindRequest>,
    /// A rebind taken during a run, a switch or another replacement, which
    /// runs as soon as that ends. A later one replaces it: the App's queue
    /// is the last one's.
    rebound: Option<Rebind>,
    configuration: RemoteSettings,
    /// Thinking, fast and workflow ride on each prompt under it.
    settings_extension: bool,
    host_queue: bool,
    active: Option<LiveRun>,
    prompt_tag: Option<u64>,
    outcome: Option<Result<StopReason, String>>,
    cancel_sent: bool,
    next_tag: u64,
    /// A yolo change was refused while a prompt waited for it.
    yolo_refused: bool,
    /// The session's latest command list offers `/compact`.
    compact_offered: bool,
    binding: Arc<SessionBinding>,
}

/// A settled request, with its answer when one came, even after a cancel.
enum Settled<T> {
    Done(T),
    Failed(String),
    Cancelled(Option<T>),
    Closed,
    /// `/new` came: the request is abandoned, and the session with it.
    Rebound,
}

#[derive(PartialEq, Eq)]
enum Wait {
    Idle,
    Request,
    Replacement,
    Yolo,
}

#[allow(clippy::large_enum_variant, reason = "moved once per event")]
enum Wake<T> {
    Stop,
    Reply(T),
    Event(Option<ClientEvent>),
    Answer(AnsweredRequest),
    Cancel,
    Rebind(Rebind),
    Rewind(RewindRequest),
    Work,
    Settings,
    Continue,
}

/// Whether the loop keeps serving after a step.
enum Flow {
    Continue,
    Closed,
}

/// However the loop ends, by a lost link, a teardown or its task being
/// dropped, no caller is left waiting on a setting.
impl Drop for RemoteAgentLoop {
    fn drop(&mut self) {
        self.configuration.lock().close();
    }
}

impl RemoteAgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        remote: &mut super::RemoteAgent,
        agent_tx: flume::Sender<Envelope>,
        answer_rx: flume::Receiver<AnsweredRequest>,
        queue: Arc<QueueReceiver>,
        cancels: Arc<RunCancels>,
        usage: UsageSlot,
        cwd: PathBuf,
        rebind_rx: flume::Receiver<Rebind>,
        rewind_rx: flume::Receiver<RewindRequest>,
        binding: Arc<SessionBinding>,
        lua: EventHandle,
        plan_writes: Option<PlanWrites>,
    ) -> Self {
        let configuration = remote.settings.clone();
        configuration
            .lock()
            .open(remote.mode.clone(), remote.session_seq);
        Self {
            client: Arc::clone(&remote.client),
            events: remote.events.clone(),
            early: mem::take(&mut remote.early),
            mapper: DisplayMapper::new(&remote.extensions),
            agent_tx,
            answer_rx,
            queue,
            cancels,
            asks: HashMap::new(),
            lua,
            plan_writes,
            forms: Vec::new(),
            next_ask: 0,
            run_id: 0,
            plan_modes: remote.plan_modes,
            refuse_asks: false,
            usage,
            cwd,
            rebind_rx,
            rewind_rx,
            rebound: None,
            configuration,
            settings_extension: remote.extensions.settings,
            host_queue: remote.extensions.sessions,
            active: None,
            prompt_tag: None,
            outcome: None,
            cancel_sent: false,
            next_tag: 0,
            yolo_refused: false,
            compact_offered: false,
            binding,
        }
    }

    /// Selects inputs in each wait's order. Replacement buffers events until
    /// its reply binds the new session; a request keeps waiting after Esc.
    async fn wake<T>(
        &mut self,
        wait: Wait,
        mut request: Pin<&mut impl Future<Output = T>>,
        token: Option<&CancelToken>,
    ) -> Wake<T> {
        let wake = {
            let mut rewind = pin!(future::fuse(self.rewind_rx.recv_async()));
            let mut teardown = pin!(self.cancels.teardown().cancelled());
            let mut rebind = pin!(future::fuse(self.rebind_rx.recv_async()));
            let mut queued = pin!(future::fuse(self.queue.recv_notify()));
            let mut event = pin!(self.events.recv_async());
            let mut answer = pin!(future::fuse(self.answer_rx.recv_async()));
            let woken = self.configuration.woken();
            let mut settings = pin!(woken.recv_async());
            let mut cancelled = pin!(async {
                match token {
                    Some(token) => token.cancelled().await,
                    None => future::pending().await,
                }
            });
            future::poll_fn(|cx| {
                if wait == Wait::Idle
                    && let Poll::Ready(Ok(request)) = rewind.as_mut().poll(cx)
                {
                    return Poll::Ready(Wake::Rewind(request));
                }
                if teardown.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Wake::Stop);
                }
                if let Poll::Ready(reply) = request.as_mut().poll(cx) {
                    return Poll::Ready(Wake::Reply(reply));
                }
                if wait == Wait::Idle {
                    if let Poll::Ready(Ok(rebind)) = rebind.as_mut().poll(cx) {
                        return Poll::Ready(Wake::Rebind(rebind));
                    }
                    match queued.as_mut().poll(cx) {
                        Poll::Ready(Ok(())) => return Poll::Ready(Wake::Work),
                        Poll::Ready(Err(_)) => match self.rebind_rx.try_recv() {
                            Ok(rebind) => return Poll::Ready(Wake::Rebind(rebind)),
                            Err(_) if self.active.is_none() => return Poll::Ready(Wake::Stop),
                            Err(_) => {}
                        },
                        Poll::Pending => {}
                    }
                }
                if let Poll::Ready(event) = event.as_mut().poll(cx) {
                    return Poll::Ready(Wake::Event(event.ok()));
                }
                match answer.as_mut().poll(cx) {
                    Poll::Ready(Ok(answer)) => return Poll::Ready(Wake::Answer(answer)),
                    Poll::Ready(Err(_)) if wait == Wait::Idle && self.active.is_none() => {
                        return Poll::Ready(Wake::Stop);
                    }
                    _ => {}
                }
                if wait == Wait::Idle && settings.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Wake::Settings);
                }
                if cancelled.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Wake::Cancel);
                }
                if wait != Wait::Idle
                    && let Poll::Ready(Ok(rebind)) = rebind.as_mut().poll(cx)
                {
                    return Poll::Ready(Wake::Rebind(rebind));
                }
                if matches!(wait, Wait::Request | Wait::Yolo)
                    && settings.as_mut().poll(cx).is_ready()
                {
                    return Poll::Ready(Wake::Settings);
                }
                Poll::Pending
            })
            .await
        };
        match wake {
            Wake::Answer(answer) => {
                self.handle_answer(&answer);
                Wake::Continue
            }
            Wake::Settings => {
                if wait != Wait::Yolo {
                    self.sync_settings();
                }
                Wake::Continue
            }
            wake => wake,
        }
    }

    /// A waiting rebind always goes before the queue: the App sends it
    /// before it drops the old queue, so a closed queue with a rebind waiting
    /// is a replacement, not a shutdown.
    pub(super) async fn run(mut self) {
        for event in mem::take(&mut self.early) {
            if let Flow::Closed = self.handle_event(Some(event)) {
                self.flush_settings();
                return;
            }
        }
        loop {
            let token = self
                .active
                .as_ref()
                .filter(|_| !self.cancel_sent)
                .map(|active| active.token().clone());
            let idle = self
                .wake(Wait::Idle, pin!(future::pending::<()>()), token.as_ref())
                .await;
            let mut flow = match idle {
                Wake::Cancel => {
                    self.cancel_sent = true;
                    self.refuse_asks = true;
                    if self.client.cancel().is_err() {
                        return;
                    }
                    Flow::Continue
                }
                Wake::Work => self.drain_queue().await,
                Wake::Event(event) => self.handle_event(event),
                Wake::Rebind(rebind) => self.replace(rebind).await,
                Wake::Rewind(request) => self.rewind(request).await,
                Wake::Continue => Flow::Continue,
                Wake::Stop => {
                    self.flush_settings();
                    return;
                }
                Wake::Reply(_) | Wake::Answer(_) | Wake::Settings => continue,
            };
            if matches!(flow, Flow::Continue) && self.rebound.is_none() {
                flow = self.drain_queue().await;
            }
            while let (Flow::Continue, Some(rebind)) = (&flow, self.rebound.take()) {
                flow = self.replace(rebind).await;
            }
            if let Flow::Closed = flow {
                self.flush_settings();
                return;
            }
        }
    }

    async fn rewind(&mut self, request: RewindRequest) -> Flow {
        let mut closed = false;
        let result = async {
            if self.client.bound_session().ok().as_ref() != Some(&request.session) {
                return Err(SESSION_REPLACED.into());
            }
            self.refuse_asks = true;
            self.retire_asks();
            let client = Arc::clone(&self.client);
            let ended = self
                .settle(
                    pin!(async { Ok(client.end_turns(REWIND_GRACE).await) }),
                    &CancelToken::none(),
                )
                .await;
            if matches!(ended, Settled::Closed) {
                closed = true;
            }
            if !matches!(ended, Settled::Done(true)) {
                return Err("The agent did not stop; rewind was not applied".into());
            }
            self.refuse_asks = true;
            let operation = SessionOperation::Rewind {
                session_id: request.session,
                turn_index: request.index,
                prompt: request.prompt,
            };
            match self
                .settle(
                    pin!(client.session_operation(operation)),
                    &CancelToken::none(),
                )
                .await
            {
                Settled::Done(value) => {
                    serde_json::from_value(value).map_err(|error| error.to_string())
                }
                Settled::Failed(error) => Err(error),
                Settled::Closed => {
                    closed = true;
                    Err(super::capabilities::CONNECTION_LOST.into())
                }
                _ => Err(SESSION_REPLACED.into()),
            }
        }
        .await;
        self.refuse_asks = false;
        let _ = request.reply.send(result);
        if closed { self.close() } else { Flow::Continue }
    }

    async fn drain_queue(&mut self) -> Flow {
        if self.host_queue {
            return self.submit_queue().await;
        }
        if self.prompt_tag.is_some() {
            return Flow::Continue;
        }
        let mut last_run_id = None;
        loop {
            if self.rebound.is_none() {
                self.rebound = self.rebind_rx.try_recv().ok();
            }
            if self.rebound.is_some() {
                // The queue is the old session's now.
                return Flow::Continue;
            }
            let Some(mut run) = self.queue.pop_run() else {
                break;
            };
            let Some(run_id) = run.drop_cancelled(self.cancels.min_run_id()) else {
                continue;
            };
            last_run_id = Some(run_id);
            self.run_id = run_id;
            if let Flow::Closed = self.process_run(run).await {
                return Flow::Closed;
            }
            if self.prompt_tag.is_some() {
                return Flow::Continue;
            }
        }
        if let Some(run_id) = last_run_id {
            let sender = EventSender::new(self.agent_tx.clone(), run_id);
            self.queue
                .publish_if_empty(|| drop(sender.send(AgentEvent::QueueDrained)));
        }
        Flow::Continue
    }

    async fn submit_queue(&mut self) -> Flow {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.token().is_cancelled())
        {
            return Flow::Continue;
        }
        for id in self.queue.cancel_hosted(self.cancels.min_run_id()) {
            let Ok(session_id) = self.client.bound_session() else {
                return self.close();
            };
            let _ = self.client.session_operation_routed(
                &SessionOperation::CancelQueued { session_id, id },
                self.next_tag,
            );
            self.next_tag += 1;
        }
        loop {
            if self.rebound.is_none() {
                self.rebound = self.rebind_rx.try_recv().ok();
            }
            if self.rebound.is_some() {
                return Flow::Continue;
            }
            let tag = self.next_tag;
            let Some((id, item)) = self.queue.submit(tag) else {
                break;
            };
            self.next_tag += 1;
            if item.run_id() < self.cancels.min_run_id() {
                self.queue.reject(tag);
                continue;
            }
            self.run_id = item.run_id();
            let token = self
                .active
                .get_or_insert_with(|| {
                    self.cancel_sent = false;
                    self.cancels.start(self.run_id)
                })
                .token()
                .clone();
            let input = match item {
                QueueItem::Message(queued) => {
                    if let Some(flow) = self.preflight(&queued.input.mode, &token).await {
                        self.queue.reject(tag);
                        self.finish_submissions();
                        return flow;
                    }
                    SessionInput::Prompt(queued.input)
                }
                QueueItem::Compact(compact) => {
                    self.mapper.start_compaction();
                    SessionInput::Compact(HostCompaction {
                        instructions: compact.instructions,
                        workflow: compact.workflow,
                    })
                }
            };
            if self.queue.cancelled(id) {
                self.queue.reject(tag);
                self.submission_ended(Ok(StopReason::Cancelled));
                self.finish_submissions();
                continue;
            }
            let Ok(session_id) = self.client.bound_session() else {
                return self.close();
            };
            let operation = SessionOperation::Enqueue {
                session_id,
                id: Some(id),
                input: Box::new(input),
            };
            match self
                .queue
                .submit_if_live(id, || self.client.session_operation_routed(&operation, tag))
            {
                Some(Ok(())) => {}
                Some(Err(error)) => {
                    self.queue.acknowledge(tag, false);
                    self.emit(AgentEvent::Notice {
                        text: error.to_string(),
                    });
                    return self.close();
                }
                None => {
                    self.queue.reject(tag);
                    self.submission_ended(Ok(StopReason::Cancelled));
                    self.finish_submissions();
                }
            }
        }
        Flow::Continue
    }

    fn queue_receipt(&mut self, receipt: QueueReceipt) {
        if let Some(raw) = &receipt.input
            && matches!(
                receipt.status,
                QueueStatus::Queued
                    | QueueStatus::Running
                    | QueueStatus::Interrupted
                    | QueueStatus::Error
            )
            && let Ok(input) = serde_json::from_value::<SessionInput>(raw.clone())
        {
            self.queue.restore_hosted(receipt.id, input, self.run_id);
            if self.active.is_none() {
                self.active = Some(self.cancels.start(self.run_id));
            }
        }
        let Some((run_id, consumed)) = self.queue.receipt(&receipt) else {
            return;
        };
        self.run_id = self.run_id.max(run_id);
        if let Some(QueueItem::Message(queued)) = consumed
            && !queued.displayed
        {
            self.emit(AgentEvent::QueueItemConsumed {
                text: queued.text,
                images: queued.input.images,
            });
        }
        let outcome = match receipt.status {
            QueueStatus::Done => Ok(StopReason::EndTurn),
            QueueStatus::Cancelled => Ok(StopReason::Cancelled),
            QueueStatus::Error | QueueStatus::Interrupted => Err(receipt
                .error
                .unwrap_or_else(|| "queued work was interrupted".into())),
            QueueStatus::Queued | QueueStatus::Running => return,
        };
        self.submission_ended(outcome);
        self.finish_submissions();
    }

    fn submission_ended(&mut self, outcome: Result<StopReason, String>) {
        if !self.outcome.as_ref().is_some_and(Result::is_err) {
            self.outcome = Some(outcome);
        }
    }

    fn finish_submissions(&mut self) {
        if self.queue.has_hosted() {
            return;
        }
        if let Some(outcome) = self.outcome.take() {
            match outcome {
                Ok(reason) => self.emit_done(reason),
                Err(message) => self.emit(AgentEvent::Error {
                    message,
                    auth: false,

                    totals: maki_types::event::RunTotals::default(),
                }),
            }
        }
        self.active = None;
        self.cancel_sent = false;
        self.refuse_asks = false;
        self.retire_asks();
        self.mapper.end_run();
        self.queue
            .publish_if_empty(|| self.emit(AgentEvent::QueueDrained));
    }

    async fn process_run(&mut self, run: QueueRun) -> Flow {
        let live = self.cancels.start(self.run_id);
        let messages = match run {
            QueueRun::Compact(compaction) => {
                let flow = self.compact(&compaction, live.token()).await;
                if self.prompt_tag.is_some() {
                    self.active = Some(live);
                }
                return flow;
            }
            QueueRun::Messages(messages) => messages,
        };
        let inputs = messages
            .into_iter()
            .map(|queued| {
                if !queued.displayed {
                    self.emit(AgentEvent::QueueItemConsumed {
                        text: queued.text,
                        images: queued.input.images.clone(),
                    });
                }
                queued.input
            })
            .collect();
        let Some(input) = shared_queue::merge_inputs(inputs) else {
            return Flow::Continue;
        };
        if let Some(flow) = self.preflight(&input.mode, live.token()).await {
            self.retire_asks();
            self.mapper.end_run();
            return flow;
        }
        let meta = self
            .settings_extension
            .then(|| WireSettings::new(input.thinking, input.fast, input.workflow).meta());
        let flow = self.send_prompt(prompt_blocks(&input), meta, live.token());
        if self.prompt_tag.is_some() {
            self.active = Some(live);
        }
        flow
    }

    /// A local compaction runs on the session's model as it is, so this one
    /// sends no switch first. The agent's command list can still wait behind
    /// the session answer, so it is read before the check.
    async fn compact(&mut self, compaction: &Compaction, token: &CancelToken) -> Flow {
        if self.drain_updates() {
            return self.close();
        }
        if !self.compact_offered {
            self.emit(AgentEvent::Error {
                message: UNAVAILABLE_REMOTE.to_owned(),
                auth: false,

                totals: maki_types::event::RunTotals::default(),
            });
            return Flow::Continue;
        }
        // The workflow it was queued under, which the agent applies to this
        // compaction only.
        let meta = self.settings_extension.then(|| {
            let intent = self.configuration.intent.load();
            WireSettings {
                thinking: intent.thinking.clone(),
                fast: intent.fast,
                workflow: compaction.workflow,
            }
            .meta()
        });
        self.mapper.start_compaction();
        let prompt = commands::compaction_prompt(compaction.instructions.as_deref());
        self.send_prompt(prompt, meta, token)
    }

    fn send_prompt(
        &mut self,
        blocks: Vec<ContentBlock>,
        meta: Option<Meta>,
        token: &CancelToken,
    ) -> Flow {
        if token.is_cancelled() {
            self.emit_done(StopReason::Cancelled);
            return Flow::Continue;
        }
        if self.rebound.is_none() {
            self.rebound = self.rebind_rx.try_recv().ok();
        }
        if self.rebound.is_some() {
            return Flow::Continue;
        }
        let tag = self.next_tag;
        self.next_tag += 1;
        if let Err(error) = self.client.prompt_routed(blocks, meta, tag) {
            self.emit(AgentEvent::Error {
                message: error.to_string(),
                auth: false,

                totals: maki_types::event::RunTotals::default(),
            });
            return self.close();
        }
        self.prompt_tag = Some(tag);
        Flow::Continue
    }

    /// `None` when the prompt may go out. Otherwise the run has ended here,
    /// with the reason already sent, and the flow says what follows. Both
    /// switches run first, and the checks come after both, because each
    /// switch drains updates that may change what the other checked.
    async fn preflight(&mut self, mode: &AgentMode, token: &CancelToken) -> Option<Flow> {
        let wanted_mode = self.plan_modes.then(|| mode_id(mode));
        // Once per preflight, before anything is read: a refusal the drain
        // below or a switch takes in still ends the run at a yolo wait.
        self.yolo_refused = false;
        if self.drain_updates() {
            return Some(self.close());
        }
        if let Some(flow) = self.yolo_preflight(token).await {
            return Some(flow);
        }
        if let Some(wanted) = wanted_mode
            && self.configuration.lock().confirmed.mode.as_deref() != Some(wanted)
        {
            let change = self
                .configuration
                .lock()
                .desire(Key::Mode, wanted.into(), false);
            let switch = self.switch(change, token).await;
            if let Some(flow) = self.ended(switch, MODE_NOT_SET) {
                return Some(flow);
            }
        }
        let checked_pick = self.configuration.take_pick().map(|change| {
            let Key::Option(config_id) = &change.key else {
                unreachable!()
            };
            (
                ModelPick {
                    config_id: config_id.clone(),
                    value: change.value.as_str().unwrap_or_default().to_owned(),
                },
                change,
            )
        });
        if let Some((pick, change)) = &checked_pick {
            let offered = self
                .configuration
                .models
                .load()
                .as_ref()
                .is_some_and(|menu| {
                    menu.config_id == pick.config_id
                        && menu.choices.iter().any(|choice| choice.value == pick.value)
                });
            if !offered {
                self.emit(AgentEvent::Error {
                    message: format!("{MODEL_NOT_SET}: {MODEL_NOT_OFFERED}"),
                    auth: false,

                    totals: maki_types::event::RunTotals::default(),
                });
                return Some(Flow::Continue);
            }
            if !pick_confirmed(&self.configuration, pick) {
                let switched = self.switch(change.clone(), token).await;
                if let Some(flow) = self.ended(switched, MODEL_NOT_SET) {
                    return Some(flow);
                }
                if !token.is_cancelled() && !pick_confirmed(&self.configuration, pick) {
                    self.emit(AgentEvent::Error {
                        message: MODEL_NOT_SET.into(),
                        auth: false,

                        totals: maki_types::event::RunTotals::default(),
                    });
                    return Some(Flow::Continue);
                }
            }
        }
        // The switches drained events that may have moved yolo again.
        if let Some(flow) = self.yolo_preflight(token).await {
            return Some(flow);
        }
        if token.is_cancelled() {
            self.emit_done(StopReason::Cancelled);
            return Some(Flow::Continue);
        }
        // The waits after the model switch drained events too.
        if let Some((pick, _)) = checked_pick
            && !pick_confirmed(&self.configuration, &pick)
        {
            self.emit(AgentEvent::Error {
                message: MODEL_NOT_SET.to_owned(),
                auth: false,

                totals: maki_types::event::RunTotals::default(),
            });
            return Some(Flow::Continue);
        }
        // A mode is a permission boundary: the prompt goes out only in the
        // mode it was typed in.
        if let Some(wanted) = wanted_mode
            && self.configuration.lock().confirmed.mode.as_deref() != Some(wanted)
        {
            self.emit(AgentEvent::Error {
                message: MODE_NOT_SET.to_owned(),
                auth: false,

                totals: maki_types::event::RunTotals::default(),
            });
            return Some(Flow::Continue);
        }
        None
    }

    /// What a switch's end means for its run: `None` to go on, otherwise the
    /// run has ended and the flow says what follows. A cancel goes on, so
    /// the checks after both switches end the run as cancelled.
    fn ended<T>(&mut self, switch: Settled<T>, reason: &str) -> Option<Flow> {
        match switch {
            Settled::Done(_) | Settled::Cancelled(_) => None,
            Settled::Rebound => Some(Flow::Continue),
            Settled::Failed(error) => {
                self.emit(AgentEvent::Error {
                    message: format!("{reason}: {error}"),
                    auth: false,

                    totals: maki_types::event::RunTotals::default(),
                });
                Some(Flow::Continue)
            }
            Settled::Closed => Some(self.close()),
        }
    }

    async fn switch(&mut self, change: Change, token: &CancelToken) -> Settled<()> {
        let tag = self.next_tag;
        self.next_tag += 1;
        self.configuration.lock().start(tag, change.clone());
        let client = Arc::clone(&self.client);
        let settled = self
            .settle(
                pin!(client.configure(change.key.clone(), change.value.clone())),
                token,
            )
            .await;
        let (answer, settled) = match settled {
            Settled::Done(answer) => (Ok(answer), Settled::Done(())),
            Settled::Cancelled(Some(answer)) => (Ok(answer), Settled::Cancelled(Some(()))),
            Settled::Failed(error) => (Err(error.clone()), Settled::Failed(error)),
            other => {
                if matches!(other, Settled::Cancelled(_)) {
                    self.configuration
                        .lock()
                        .finish(tag, Err("cancelled".into()));
                }
                return match other {
                    Settled::Cancelled(_) => Settled::Cancelled(None),
                    Settled::Closed => Settled::Closed,
                    _ => Settled::Rebound,
                };
            }
        };
        let mut core = self.configuration.lock();
        let confirmation = core.finish(tag, answer);
        self.configuration.refresh(&core);
        if matches!(settled, Settled::Done(_))
            && let Some((_, Err(error), _)) = confirmation
            && error != maki_wire::configuration::KEPT
        {
            return Settled::Failed(error);
        }
        settled
    }

    /// Waits for a settings request while still drawing updates and
    /// answering asks. An Esc stops watching the run's token but still waits
    /// for the answer: the agent may apply it anyway, and the next run must
    /// know. Asks that arrive meanwhile have no run on screen, so they are
    /// cancelled at once.
    async fn settle<T>(
        &mut self,
        mut request: Pin<&mut impl Future<Output = Result<T, ClientError>>>,
        token: &CancelToken,
    ) -> Settled<T> {
        let mut cancelled = false;
        let settled = loop {
            let token = (!cancelled).then_some(token);
            let wake = self.wake(Wait::Request, request.as_mut(), token).await;
            match wake {
                Wake::Stop
                | Wake::Event(None | Some(ClientEvent::Closed | ClientEvent::RestoreError(_))) => {
                    break Settled::Closed;
                }
                Wake::Reply(Err(ClientError::Closed)) => break Settled::Closed,
                Wake::Reply(result) => {
                    if self.drain_updates() {
                        break Settled::Closed;
                    }
                    break match (result, cancelled) {
                        (Ok(value), false) => Settled::Done(value),
                        (Ok(value), true) => Settled::Cancelled(Some(value)),
                        (Err(error), false) => Settled::Failed(error.to_string()),
                        (Err(_), true) => Settled::Cancelled(None),
                    };
                }
                Wake::Event(event) => {
                    self.handle_event(event);
                }
                Wake::Cancel => {
                    cancelled = true;
                    self.refuse_asks = true;
                    self.retire_asks();
                }
                Wake::Rebind(rebind) => {
                    self.rebound = Some(rebind);
                    break Settled::Rebound;
                }
                _ => {}
            }
        };
        self.refuse_asks = false;
        settled
    }

    /// `/new` in place. The replacement is never abandoned, because its
    /// answer binds the client's session; a rebind that comes meanwhile runs
    /// next. Updates are sorted by wire position, not by when they are read:
    /// those numbered below the answer are the old session's and are dropped.
    /// A failed `session/new` ends the connection, because the agent may
    /// keep the old session, and a prompt sent now could land in it.
    async fn replace(&mut self, rebind: Rebind) -> Flow {
        if self.host_queue
            && let Ok(session_id) = self.client.bound_session()
        {
            let _ = self.client.session_operation_routed(
                &SessionOperation::Close {
                    session_id,
                    reason: Some(maki_types::event::SessionEndReason::Reset),
                },
                self.next_tag,
            );
            self.next_tag += 1;
        } else if self.prompt_tag.is_some() {
            let _ = self.client.cancel();
        }
        self.active = None;
        self.prompt_tag = None;
        self.outcome = None;
        self.cancel_sent = false;
        self.retire_asks();
        self.queue = rebind.queue;
        self.run_id = rebind.run_id;
        self.usage = rebind.usage;
        let generation = rebind.generation;
        // The old session's answer finds nothing to settle and is dropped,
        // and no caller waits on it.
        let carried = {
            let mut core = self.configuration.lock();
            let model_id = self
                .configuration
                .models
                .load_full()
                .map(|menu| menu.config_id.clone());
            let interrupted = core
                .flights
                .values()
                .find_map(|flight| match &flight.change.key {
                    Key::Option(id) if Some(id) == model_id.as_ref() => Some(Arc::new(ModelPick {
                        config_id: id.clone(),
                        value: flight.change.value.as_str()?.to_owned(),
                    })),
                    _ => None,
                });
            core.reset(SESSION_REPLACED);
            interrupted
        }
        .or_else(|| self.confirmed_model());
        let client = Arc::clone(&self.client);
        let mut request = pin!(client.new_session(self.cwd.clone()));
        let mut kept = Vec::new();
        let answer = loop {
            let wake = self.wake(Wait::Replacement, request.as_mut(), None).await;
            match wake {
                Wake::Stop => return Flow::Closed,
                Wake::Reply(answer) => break answer,
                Wake::Event(None | Some(ClientEvent::Closed | ClientEvent::RestoreError(_))) => {
                    return self.close();
                }
                // At once: the agent may hold the answer until it hears back.
                Wake::Event(Some(ClientEvent::Permission(ask))) => {
                    let _ = self
                        .client
                        .reply_permission(ask.id, PermissionReply::Cancelled);
                }
                Wake::Event(Some(ClientEvent::Question(ask))) => {
                    let _ = self.client.reply_question(ask.id, None);
                }
                Wake::Event(Some(event)) => kept.push(event),
                Wake::Rebind(next) => self.rebound = Some(next),
                _ => {}
            }
        };
        let response = match answer {
            Ok(response) => response,
            Err(ClientError::Closed) => return self.close(),
            Err(error) => {
                warn!(%error, "{NEW_SESSION_FAILED}");
                // A notice, not a run error, which would clear the queue the
                // lost connection hands back to the input.
                self.emit(AgentEvent::Notice {
                    text: format!("{NEW_SESSION_FAILED}: {error}"),
                });
                return self.close();
            }
        };
        kept.extend(self.events.try_iter());
        let seq = response.seq;
        self.install(response);
        // A failed replacement never gets here: it ends the link instead.
        self.binding.bind(generation);
        // Against the answer's menu: an update the agent sent after it is
        // newer than the session's default, and applies as it would live.
        self.carry(carried);
        // The App keeps its intent across `/new`, as local `/new` does.
        self.sync_settings();
        for event in kept {
            match event {
                ClientEvent::Update(at, _) if at < seq => {}
                ClientEvent::Permission(ask) => {
                    let _ = self
                        .client
                        .reply_permission(ask.id, PermissionReply::Cancelled);
                }
                ClientEvent::Question(ask) => {
                    let _ = self.client.reply_question(ask.id, None);
                }
                event => {
                    if let Flow::Closed = self.handle_event(Some(event)) {
                        return Flow::Closed;
                    }
                }
            }
        }
        Flow::Continue
    }

    fn install(&mut self, response: Sequenced<NewSessionResponse>) {
        if let Some(writes) = &self.plan_writes {
            writes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        let session = response.value;
        let mut core = self.configuration.lock();
        core.open(
            session.modes.map(|modes| modes.current_mode_id.to_string()),
            response.seq,
        );
        core.publish_options(session.config_options.unwrap_or_default(), response.seq);
        self.configuration.refresh(&core);
        self.compact_offered = false;
        self.mapper.reset();
    }

    /// The model the old session last confirmed.
    fn confirmed_model(&self) -> Option<Arc<ModelPick>> {
        let menu = self.configuration.models.load_full()?;
        Some(Arc::new(ModelPick {
            config_id: menu.config_id.clone(),
            value: menu.current.clone()?,
        }))
    }

    /// The local `/new` keeps the tab's model, and the agent opens a new
    /// session on its own default, so the model is picked again for the
    /// next prompt, unless the user picked one meanwhile or the new menu
    /// does not offer it.
    fn carry(&self, pick: Option<Arc<ModelPick>>) {
        let Some(pick) = pick else { return };
        let mut core = self.configuration.lock();
        let Some(menu) = self.configuration.models.load_full() else {
            return;
        };
        let offered = menu.choices.iter().any(|choice| choice.value == pick.value);
        if offered
            && menu.current.as_deref() != Some(&pick.value)
            && core.deferred_change().is_none()
        {
            core.desire(
                Key::Option(menu.config_id.clone()),
                pick.value.clone().into(),
                true,
            );
        }
    }

    fn preference_keys(&self) -> Vec<Key> {
        Setting::ALL
            .into_iter()
            .filter(|setting| !setting.rides_on_the_prompt() || self.settings_extension)
            .map(Setting::key)
            .collect()
    }

    fn sync_settings(&mut self) {
        let mut core = self.configuration.lock();
        if let Some(change) = core.next(&self.preference_keys(), |key| {
            self.configuration.offered(key)
        }) {
            let Key::Option(id) = &change.key else {
                unreachable!()
            };
            let tag = self.next_tag;
            self.next_tag += 1;
            let value = wire_value(&change.value);
            if self.client.set_config_routed(id, value, tag).is_ok() {
                core.start(tag, change);
            }
        }
        core.settle_held(|key| self.configuration.offered(key));
        self.configuration.refresh(&core);
    }

    fn flush_settings(&mut self) {
        let core = self.configuration.lock();
        for key in self.preference_keys() {
            let Some(wanted) = core.wanted(&key) else {
                continue;
            };
            let pending = core
                .flights
                .values()
                .find(|flight| flight.change.key == key)
                .map(|flight| flight.change.value.clone());
            if pending
                .or_else(|| {
                    self.configuration
                        .offered(&key)
                        .then(|| core.value(&key))
                        .flatten()
                })
                .is_some_and(|held| held != *wanted)
            {
                let value = wire_value(wanted);
                let _ = self
                    .client
                    .set_config_routed(key.name(), value, self.next_tag);
                self.next_tag += 1;
            }
        }
    }

    fn settings_answer(&mut self, seq: u64, tag: u64, result: Result<Value, AcpError>) {
        {
            let mut core = self.configuration.lock();
            let Some(_) = core.flights.get(&tag) else {
                return;
            };
            let answer = result
                .map(|value| Sequenced { seq, value })
                .map_err(|error| error.message);
            let Some((change, result, current)) = core.finish(tag, answer) else {
                return;
            };
            if let Err(reason) = result {
                self.configuration.failure.store(Some(Arc::new(format!(
                    "{SETTING_NOT_SET}: {}: {reason}",
                    change.key.name()
                ))));
                if change.key == Setting::Yolo.key() && current {
                    self.yolo_refused = true;
                }
            }
            self.configuration.refresh(&core);
        }
        self.sync_settings();
    }

    /// Yolo is policy for the prompt's tool calls, so the prompt waits for a
    /// pending change to it; the other three ride on the prompt itself. A
    /// refusal while it waits ends the run, even once the App has put the
    /// intent back.
    async fn yolo_preflight(&mut self, token: &CancelToken) -> Option<Flow> {
        loop {
            if std::mem::take(&mut self.yolo_refused) {
                self.emit(AgentEvent::Error {
                    message: format!("{SETTING_NOT_SET}: {}", Setting::Yolo.id()),
                    auth: false,

                    totals: maki_types::event::RunTotals::default(),
                });
                return Some(Flow::Continue);
            }
            if !self.yolo_pending() {
                return None;
            }
            self.sync_settings();
            let wake = self
                .wake(Wait::Yolo, pin!(future::pending::<()>()), Some(token))
                .await;
            match wake {
                Wake::Stop => return Some(Flow::Closed),
                Wake::Event(event) => {
                    if let Flow::Closed = self.handle_event(event) {
                        return Some(Flow::Closed);
                    }
                }
                // The checks after the preflight end the run as cancelled.
                Wake::Cancel => return None,
                Wake::Rebind(rebind) => {
                    self.rebound = Some(rebind);
                    return Some(Flow::Continue);
                }
                _ => {}
            }
        }
    }

    /// A yolo request is out, or the intent differs from what the agent
    /// holds. The intent is read fresh each time, so a change back to the
    /// agent's value ends the wait.
    fn yolo_pending(&self) -> bool {
        let core = self.configuration.lock();
        let key = Setting::Yolo.key();
        core.pending(&key)
            || core
                .value(&key)
                .is_some_and(|held| core.wanted(&key) != Some(&held))
    }

    /// The reader puts every update in the channel before it resolves the
    /// response that follows them, so the updates queued now came first and
    /// must be drawn before the run ends. A close queued behind them came
    /// after the response, so it must not replace it: `true` says one did.
    fn drain_updates(&mut self) -> bool {
        while let Ok(event) = self.events.try_recv() {
            if matches!(event, ClientEvent::Closed | ClientEvent::RestoreError(_)) {
                return true;
            }
            self.handle_event(Some(event));
        }
        false
    }

    /// Checked right before the take, since the event arm can run before the
    /// cancel arm: a cancelled, torn down or rebound run takes nothing, and a
    fn handle_event(&mut self, event: Option<ClientEvent>) -> Flow {
        match event {
            Some(ClientEvent::Update(seq, update)) => {
                match &update {
                    SessionUpdate::CurrentModeUpdate(current) => {
                        self.configuration
                            .lock()
                            .publish_mode(current.current_mode_id.to_string(), seq);
                    }
                    SessionUpdate::ConfigOptionUpdate(update) => {
                        self.configuration.publish(&update.config_options, seq);
                    }
                    SessionUpdate::AvailableCommandsUpdate(update) => {
                        self.compact_offered = commands::offers_compaction(update);
                    }
                    // Kept before the mapper emits the call's `ToolDone`,
                    // which the App takes it with.
                    SessionUpdate::ToolCallUpdate(call) => {
                        if let Some(writes) = &self.plan_writes
                            && let Some(written) = plan::written(call.meta.as_ref())
                        {
                            writes
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(call.tool_call_id.to_string(), written);
                        }
                    }
                    _ => {}
                }
                for event in self.mapper.map(update) {
                    self.emit(event);
                }
                // Session state, not run state: an error has no `Done`, and
                // Esc makes the run's events stale.
                if let Some(usage) = self.mapper.take_usage() {
                    self.usage.store(Some(Arc::new(usage)));
                }
            }
            Some(ClientEvent::Restored(_)) => {}
            Some(ClientEvent::Task(mut update)) => {
                if self
                    .client
                    .bound_session()
                    .is_ok_and(|id| id == update.session_id)
                {
                    update.envelope.run_id = self.run_id;
                    if let Some(snapshot) = update.inbox
                        && let Some(info) = &update.envelope.subagent
                    {
                        let _ = self.agent_tx.send(Envelope {
                            event: AgentEvent::TaskQueue {
                                id: info.parent_tool_use_id.clone(),
                                snapshot,
                            },
                            subagent: Some(info.clone()),
                            run_id: self.run_id,
                        });
                    }
                    let _ = self.agent_tx.send(update.envelope);
                }
            }
            Some(ClientEvent::QueueUpdate(update)) => {
                if self
                    .client
                    .bound_session()
                    .is_ok_and(|id| id == update.session_id)
                {
                    self.queue_receipt(update.receipt);
                }
            }
            Some(ClientEvent::Permission(ask)) => self.ask(ask),
            // Only the live prompt owns a form, see `question`.
            Some(ClientEvent::Question(ask)) => match &self.active {
                Some(active) => {
                    let token = active.token().clone();
                    self.question(ask, &token, self.cancel_sent);
                }
                None => {
                    let _ = self.client.reply_question(ask.id, None);
                }
            },
            Some(ClientEvent::Malformed(reason)) => self.emit(AgentEvent::Notice {
                text: format!("{MALFORMED_UPDATE}: {reason}"),
            }),
            // Only a load sends it, and the load reads its own marker.
            Some(ClientEvent::ReplayEnd) => {}
            Some(ClientEvent::Answer { seq, tag, result }) => {
                if let Some(id) = self.queue.acknowledge(tag, result.is_ok()) {
                    match result.and_then(|value| {
                        serde_json::from_value::<QueueReceipt>(value)
                            .map_err(|error| AcpError::new(-32603, error.to_string()))
                    }) {
                        Ok(receipt) => {
                            self.queue_receipt(receipt);
                        }
                        Err(error) => {
                            self.queue.acknowledge(tag, false);
                            self.client.forget_queue(id);
                            self.submission_ended(Err(error.to_string()));
                        }
                    }
                    self.finish_submissions();
                } else if self.prompt_tag == Some(tag) {
                    self.prompt_tag = None;
                    match result.and_then(|value| {
                        serde_json::from_value::<PromptResponse>(value)
                            .map_err(|error| AcpError::new(-32603, error.to_string()))
                    }) {
                        Ok(response) => self.emit_done(response.stop_reason),
                        Err(error) => self.emit(AgentEvent::Error {
                            message: error.to_string(),
                            auth: false,

                            totals: maki_types::event::RunTotals::default(),
                        }),
                    }
                    self.active = None;
                    self.cancel_sent = false;
                    self.refuse_asks = false;
                    self.retire_asks();
                    self.mapper.end_run();
                    self.queue
                        .publish_if_empty(|| self.emit(AgentEvent::QueueDrained));
                } else {
                    self.settings_answer(seq, tag, result);
                }
            }
            Some(ClientEvent::Closed | ClientEvent::RestoreError(_)) | None => return self.close(),
        }
        Flow::Continue
    }

    fn ask(&mut self, ask: PermissionAsk) {
        if self.refuse_asks {
            let _ = self
                .client
                .reply_permission(ask.id, PermissionReply::Cancelled);
            return;
        }
        let id = format!("{ASK_ID_PREFIX}{}", self.next_ask);
        self.next_ask += 1;
        let call_id = ask.request.tool_call.tool_call_id.to_string();
        let title = ask
            .request
            .tool_call
            .fields
            .title
            .clone()
            .unwrap_or_else(|| call_id.clone());
        let tool = self
            .mapper
            .tool_name(&call_id)
            .filter(|name| is_valid_wire_name(name))
            .unwrap_or_else(|| REMOTE_TOOL.into());
        let choices: Vec<PermissionChoice> = ask
            .request
            .options
            .iter()
            .map(|option| PermissionChoice {
                id: option.option_id.to_string(),
                label: option.name.clone(),
                allow: matches!(
                    option.kind,
                    PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                ),
            })
            .collect();
        // Nothing to choose means nothing the user could approve, and the
        // local choices must never stand in for the agent's.
        if choices.is_empty() {
            let _ = self
                .client
                .reply_permission(ask.id, PermissionReply::Cancelled);
            return;
        }
        let event = AgentEvent::PermissionRequest {
            id: id.clone(),
            tool: ToolKey::Native(tool),
            scopes: vec![title],
            reason: None,
            choices,
        };
        let subagent = ask
            .request
            .meta
            .as_ref()
            .and_then(|meta| meta.get(maki_wire::session::TASK_PERMISSION_KEY))
            .and_then(|value| serde_json::from_value(value.clone()).ok());
        let envelope = Envelope {
            event,
            subagent,
            run_id: self.run_id,
        };
        let event = ask
            .request
            .meta
            .as_ref()
            .and_then(|meta| meta.get(maki_wire::session::PLUGIN_TASK_KEY))
            .and_then(|value| value.as_str())
            .map(Arc::from);
        let envelope = match event {
            Some(plugin) => Envelope {
                event: AgentEvent::PluginTask {
                    plugin,
                    envelope: Box::new(envelope),
                },
                subagent: None,
                run_id: self.run_id,
            },
            None => envelope,
        };
        let _ = self.agent_tx.send(envelope);
        self.asks.insert(id, ask);
    }

    /// The bundled form, drawn by the client's own Lua host as a local tool
    /// call draws it, for a question of the live prompt, which retires it.
    /// A run already cancelled or replaced gets no form. The answer goes from a
    /// task of its own, since the form outlives the event that opened it.
    fn question(&mut self, ask: QuestionAsk, token: &CancelToken, cancel_sent: bool) {
        if self.rebound.is_none() {
            self.rebound = self.rebind_rx.try_recv().ok();
        }
        let ended = cancel_sent
            || token.is_cancelled()
            || self.cancels.teardown().is_cancelled()
            || self.rebound.is_some();
        if ended {
            let _ = self.client.reply_question(ask.id, None);
            return;
        }
        let (answer, cancel) = self.lua.ui.open_question_form(ask.questions);
        let client = Arc::clone(&self.client);
        tokio::spawn(async move {
            let answers = match answer.recv_async().await {
                Ok(QuestionFormAnswer::Submitted(answers)) => Some(answers),
                Ok(QuestionFormAnswer::Dismissed) | Err(_) => None,
            };
            let _ = client.reply_question(ask.id, answers);
        });
        self.forms.push((ask.id, cancel));
    }

    /// An id the agent did not offer is a client bug, never an answer: it
    /// sends nothing and the ask waits for its run to end.
    fn handle_answer(&mut self, answer: &AnsweredRequest) {
        let request_id = &answer.id;
        let option = answer.answer.as_deref();
        let Some(ask) = self.asks.get(request_id) else {
            return;
        };
        let reply = match option {
            Some(option) => {
                let offered = ask
                    .request
                    .options
                    .iter()
                    .any(|o| o.option_id.to_string() == option);
                if !offered {
                    return;
                }
                PermissionReply::Selected(option.to_owned().into())
            }
            None => PermissionReply::Cancelled,
        };
        if let Some(ask) = self.asks.remove(request_id) {
            let _ = self.client.reply_permission(ask.id, reply);
        }
    }

    /// The agent's turn is over, so nothing it asked can still be answered.
    fn retire_asks(&mut self) {
        for (_, ask) in self.asks.drain() {
            let _ = self
                .client
                .reply_permission(ask.id, PermissionReply::Cancelled);
        }
        // Dismissed here, not by the form's task, so an answer the form
        // queued a moment ago finds no ask left to reach the agent with.
        for (ask, _close) in self.forms.drain(..) {
            let _ = self.client.reply_question(ask, None);
        }
    }

    /// The loss is the session's, not a run's: the event loop sees this task
    /// end and reports it once, before anything can clear the unsent queue.
    /// So no run event goes out here.
    fn close(&mut self) -> Flow {
        self.asks.clear();
        self.forms.clear();
        Flow::Closed
    }

    fn emit_done(&mut self, stop_reason: StopReason) {
        let done = self.mapper.done(stop_reason);
        self.emit(done);
    }

    fn emit(&self, event: AgentEvent) {
        let _ = EventSender::new(self.agent_tx.clone(), self.run_id).send(event);
    }
}

/// The pair, not the value alone: an option replaced meanwhile can reuse the
/// same opaque value for something else.
fn pick_confirmed(models: &RemoteSettings, pick: &ModelPick) -> bool {
    models.models.load().as_ref().is_some_and(|menu| {
        menu.config_id == pick.config_id && menu.current.as_deref() == Some(pick.value.as_str())
    })
}

/// One prompt for a burst: earlier messages first, in order, each with its
/// images, then the last. Shell results ride in front as text.
fn prompt_blocks(input: &AgentInput) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    let mut push = |message: &str, images: &[ImageSource], preamble: &[Message]| {
        for block in preamble.iter().flat_map(|m| &m.content) {
            if let MessageBlock::Text { text } = block {
                blocks.push(ContentBlock::Text(TextContent::new(text.clone())));
            }
        }
        if !message.is_empty() {
            blocks.push(ContentBlock::Text(TextContent::new(message)));
        }
        for image in images {
            blocks.push(ContentBlock::Image(ImageContent::new(
                image.data.to_string(),
                image.media_type.mime(),
            )));
        }
    };
    for earlier in &input.earlier {
        push(&earlier.message, &earlier.images, &earlier.preamble);
    }
    push(&input.message, &input.images, &input.preamble);
    blocks
}

#[cfg(test)]
pub(crate) mod tests {
    use std::iter;
    use std::path::PathBuf;
    use std::time::Duration;

    use agent_client_protocol_schema::v1::{SessionConfigOption, StopReason};

    use maki_types::input::{AgentMode, EarlierInput, InputSource};
    use maki_types::stop_reason::DoneReason;
    use maki_types::thinking::ThinkingConfig;

    use maki_lua_ui::{Key, UiAction, WinCommand, WinEvent};
    use maki_storage::id::MakiId;
    use maki_types::message::ImageMediaType;
    use maki_wire::client::Link;
    use maki_wire::{elicitation, methods};
    use serde::Serialize;
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;
    use crate::agent::shared_queue::{Compaction, QueueItem, QueuedInput};
    use crate::agent::{AgentHandles, RemoteAgent, spawn_remote};
    use maki_wire::extensions::Agreed;

    pub(crate) const SESSION: &str = "remote-session";
    pub(crate) const CWD: &str = "/agent/workspace";
    const PROMPT: &str = "list the files";
    const EARLIER: &str = "first";
    const SHELL_RESULT: &str = "$ ls\nCargo.toml";
    const REPLY: &str = "two files";
    const ALLOW_ONCE: &str = "allow-once-id";
    const ALLOW_ALWAYS: &str = "allow-always-id";
    const REJECT_ONCE: &str = "reject-once-id";
    const REJECT_ALWAYS: &str = "reject-always-id";
    const ALLOW_TWICE: &str = "allow-once-other-id";
    const UNOFFERED: &str = "approve-everything";
    const RUN_ID: u64 = 1;
    const NEXT_RUN_ID: u64 = 2;
    const IMAGE_DATA: &str = "aGVsbG8=";
    const GUIDANCE: &str = "keep the API notes";
    const ENQUEUE_REJECTED: &str = "queued input rejected";

    /// The agent's end of the link.
    pub(crate) struct Peer {
        pub(crate) from_client: flume::Receiver<String>,
        to_client: flume::Sender<String>,
    }

    impl Peer {
        pub(crate) fn recv(&self) -> Value {
            serde_json::from_str(&self.from_client.recv().unwrap()).unwrap()
        }

        pub(crate) fn send(&self, message: Value) {
            self.to_client.send(message.to_string()).unwrap();
        }

        pub(crate) fn respond(&self, request: &Value, result: impl Serialize) {
            self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}));
        }
    }

    pub(crate) fn remote() -> (AgentHandles, Peer) {
        remote_with(false, None)
    }

    fn remote_sessions() -> (AgentHandles, Peer) {
        remote_settled(
            false,
            None,
            None,
            Agreed {
                sessions: true,
                ..Agreed::default()
            },
        )
    }

    fn remote_with(plan_modes: bool, mode: Option<&str>) -> (AgentHandles, Peer) {
        remote_full(plan_modes, mode, None)
    }

    /// `plan_modes` and `mode` are what `maki connect` reads from the
    /// `session/new` response.
    fn remote_full(
        plan_modes: bool,
        mode: Option<&str>,
        models: Option<RemoteSettings>,
    ) -> (AgentHandles, Peer) {
        remote_settled(plan_modes, mode, models, Agreed::default())
    }

    pub(crate) fn remote_settled(
        plan_modes: bool,
        mode: Option<&str>,
        settings: Option<RemoteSettings>,
        extensions: Agreed,
    ) -> (AgentHandles, Peer) {
        remote_hosted(
            plan_modes,
            mode,
            settings,
            extensions,
            EventHandle::disconnected_for_test(),
            Vec::new(),
        )
    }

    fn remote_hosted(
        plan_modes: bool,
        mode: Option<&str>,
        settings: Option<RemoteSettings>,
        extensions: Agreed,
        lua: EventHandle,
        early: Vec<ClientEvent>,
    ) -> (AgentHandles, Peer) {
        let _runtime = crate::test_runtime().enter();
        let (client_tx, from_client) = flume::unbounded();
        let (to_client, client_rx) = flume::unbounded();
        let (client, events) = AcpClient::new(Link::new(client_tx, client_rx));
        let peer = Peer {
            from_client,
            to_client,
        };
        let (session, ()) =
            crate::test_block_on(future::zip(client.new_session(CWD.into()), async {
                let request = peer.recv();
                peer.respond(&request, methods::new_session_response(SESSION));
            }));
        session.unwrap();
        let handles = spawn_remote(
            RemoteAgent {
                client: Arc::new(client),
                events,
                extensions,
                images: true,
                plan_modes,
                mode: mode.map(str::to_owned),
                session_seq: SESSION_SEQ,
                replay: Vec::new(),
                unreadable: 0,
                settings: settings.unwrap_or_else(|| RemoteSettings::from_options(&[])),
                login: None,
                input: None,
                reload: false,
                early,
            },
            None,
            PathBuf::from(CWD),
            lua,
        );
        (handles, peer)
    }

    #[test]
    fn rewind_drains_earlier_task_events_before_returning_the_cut() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_sessions();
        let link = handles.remote_link.clone();
        let (result, ()) = crate::test_block_on(future::zip(
            tokio::task::spawn_blocking(move || link.rewind(0, PROMPT.into())),
            async {
                let request = peer.recv();
                assert_eq!(request["params"]["operation"], "rewind");
                let info = crate::app::tests::subagent_info("task", "worker");
                peer.send(json!({"jsonrpc":"2.0", "method":maki_wire::session::TASK_UPDATE_METHOD, "params":maki_wire::session::TaskUpdate { session_id: SESSION.into(), envelope: Envelope { event: AgentEvent::TextDelta { text: REPLY.into() }, subagent: Some(info), run_id: 0 }, inbox: None }}));
                peer.respond(&request, StoredSession::new("", CWD));
            },
        ));
        let mut app = crate::app::tests::test_app();
        app.run_id = 0;
        app.finish_remote_rewind(
            result.unwrap().unwrap(),
            crate::components::rewind_picker::RewindEntry {
                turn_index: 0,
                prompt_preview: String::new(),
                prompt_text: PROMPT.into(),
            },
        );
        for event in handles.agent_rx.try_iter() {
            app.update(crate::app::Msg::Agent(Box::new(event)));
        }
        assert_eq!(app.chats.len(), 1);
        assert_eq!(app.input_box.buffer.value(), PROMPT);
    }

    fn input(message: &str) -> AgentInput {
        AgentInput {
            message: message.into(),
            mode: AgentMode::default(),
            images: Vec::new(),
            preamble: Vec::new(),
            earlier: Vec::new(),
            thinking: ThinkingConfig::default(),
            fast: false,
            workflow: false,
            prompt: None,
            source: InputSource::Tui,
        }
    }

    fn push_prompt(handles: &AgentHandles) {
        push_prompt_in(handles, AgentMode::Build);
    }

    fn push_prompt_in(handles: &AgentHandles, mode: AgentMode) {
        push_run(handles, mode, RUN_ID);
    }

    fn push_run(handles: &AgentHandles, mode: AgentMode, run_id: u64) {
        handles.queue.push(QueueItem::Message(QueuedInput {
            text: PROMPT.into(),
            input: AgentInput {
                mode,
                ..input(PROMPT)
            },
            run_id,
            displayed: true,
        }));
    }

    fn plan() -> AgentMode {
        AgentMode::Plan(PathBuf::new())
    }

    /// Every event of the run, up to and including `QueueDrained`.
    fn run_events(handles: &AgentHandles) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        loop {
            let envelope = handles.agent_rx.recv().unwrap();
            assert_eq!(envelope.run_id, RUN_ID);
            let drained = matches!(envelope.event, AgentEvent::QueueDrained);
            events.push(envelope.event);
            if drained {
                return events;
            }
        }
    }

    /// Every event the loop sent before its task ended.
    fn events_until_end(handles: AgentHandles) -> Vec<AgentEvent> {
        let rx = handles.agent_rx.clone();
        crate::test_block_on(handles.into_task()).unwrap();
        rx.try_iter().map(|envelope| envelope.event).collect()
    }

    fn text_update(text: &str) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": SESSION,
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}
        }})
    }

    fn done_reason(events: &[AgentEvent]) -> Option<DoneReason> {
        events.iter().find_map(|event| match event {
            AgentEvent::Done { reason, .. } => Some(*reason),
            _ => None,
        })
    }

    /// The response is resolved after the update before it, but on another
    /// channel, so the run must still draw the update before it ends.
    #[test]
    fn prompt_draws_its_updates_then_ends() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        assert_eq!(prompt["method"], "session/prompt");
        assert_eq!(prompt["params"]["prompt"][0]["text"], PROMPT);
        peer.send(text_update(REPLY));
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));

        let events = run_events(&handles);
        assert!(matches!(&events[0], AgentEvent::TextDelta { text } if text == REPLY));
        assert!(matches!(
            events[1],
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
                ..
            }
        ));
        assert!(matches!(events[2], AgentEvent::QueueDrained));
    }

    /// The close lands right behind the response, on either branch the loop
    /// may take. The run still ends with its own result.
    #[test]
    fn close_behind_the_response_keeps_the_result() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        drop(peer);
        let events = events_until_end(handles);
        assert_eq!(done_reason(&events), Some(DoneReason::EndTurn));
    }

    #[test]
    fn cancel_asks_the_agent_and_waits_for_its_answer() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        handles.cancel_run(RUN_ID);
        let cancel = peer.recv();
        assert_eq!(cancel["method"], "session/cancel");
        peer.respond(&prompt, PromptResponse::new(StopReason::Cancelled));
        assert_eq!(
            done_reason(&run_events(&handles)),
            Some(DoneReason::Cancelled)
        );
    }

    #[test]
    fn rejected_prompt_ends_the_run_with_an_error() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        peer.send(json!({"jsonrpc": "2.0", "id": prompt["id"],
            "error": {"code": -32603, "message": "boom"}}));
        let events = run_events(&handles);
        assert!(matches!(events[0], AgentEvent::Error { .. }));
        assert_eq!(done_reason(&events), None);
    }

    fn push_compaction(handles: &AgentHandles, instructions: Option<&str>, workflow: bool) {
        handles.queue.push(QueueItem::Compact(Compaction {
            run_id: RUN_ID,
            instructions: instructions.map(str::to_owned),
            workflow,
        }));
    }

    /// The loop reads every update queued before a prompt's answer, so the
    /// command list is in by the time the next run starts.
    fn offer_compaction(handles: &AgentHandles, peer: &Peer) {
        push_prompt(handles);
        let prompt = peer.recv();
        peer.send(
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": SESSION, "update": commands::advertised()
            }}),
        );
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        run_events(handles);
    }

    const QUEUED: &str = "and run the tests";

    fn queue_message(handles: &AgentHandles, text: &str, mode: AgentMode) {
        handles.queue.push(QueueItem::Message(QueuedInput {
            text: text.into(),
            input: AgentInput {
                mode,
                ..input(text)
            },
            run_id: RUN_ID,
            displayed: false,
        }));
    }

    fn host_receipt(peer: &Peer, id: MakiId, status: QueueStatus) {
        peer.send(json!({ "jsonrpc": "2.0", "method": maki_wire::session::QUEUE_UPDATE_METHOD,
            "params": { "sessionId": SESSION, "receipt": QueueReceipt { id, status, error: None, input: None } } }));
    }

    fn accepted(peer: &Peer, request: &Value) -> MakiId {
        assert_eq!(request["method"], maki_wire::session::METHOD);
        assert_eq!(request["params"]["operation"], "enqueue");
        let id = serde_json::from_value(request["params"]["id"].clone()).unwrap();
        peer.respond(
            request,
            QueueReceipt {
                id,
                status: QueueStatus::Queued,
                error: None,
                input: Some(request["params"]["input"].clone()),
            },
        );
        id
    }

    #[test]
    fn host_queue_keeps_accepted_rows_deletable_without_resending_them() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_sessions();
        push_prompt(&handles);
        let first = peer.recv();
        let first_id = accepted(&peer, &first);
        host_receipt(&peer, first_id, QueueStatus::Running);
        queue_message(&handles, QUEUED, AgentMode::Build);
        let second = peer.recv();
        let second_id = accepted(&peer, &second);
        peer.send(text_update(REPLY));
        loop {
            if matches!(handles.agent_rx.recv().unwrap().event, AgentEvent::TextDelta { text } if text == REPLY)
            {
                break;
            }
        }
        assert!(handles.queue.text_messages().is_empty());
        assert_eq!(handles.queue.panel_entries()[0].text, QUEUED);
        handles.queue.remove(0);
        let cancel = peer.recv();
        assert_eq!(cancel["params"]["operation"], "cancel_queued");
        assert_eq!(
            cancel["params"]["id"],
            serde_json::to_value(second_id).unwrap()
        );
        host_receipt(&peer, second_id, QueueStatus::Cancelled);
        peer.respond(&cancel, true);
        host_receipt(&peer, first_id, QueueStatus::Done);
        let events = run_events(&handles);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::QueueItemConsumed { .. }))
        );
        assert_eq!(done_reason(&events), Some(DoneReason::EndTurn));
    }

    #[test_case(false, false ; "rejected_message")]
    #[test_case(false, true ; "invalid_receipt")]
    #[test_case(true, false ; "rejected_compaction")]
    fn rejected_host_input_returns_to_the_composer_for_explicit_retry(
        compact: bool,
        malformed: bool,
    ) {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_sessions();
        let running = if !compact && !malformed {
            queue_message(&handles, EARLIER, AgentMode::Build);
            let request = peer.recv();
            let id = accepted(&peer, &request);
            host_receipt(&peer, id, QueueStatus::Running);
            Some(id)
        } else {
            None
        };
        let mut app = crate::app::tests::test_app();
        app.queue.set_shared(handles.queue.clone());
        app.run_id = if malformed { NEXT_RUN_ID } else { RUN_ID };
        app.input_box.set_input(GUIDANCE);
        let mut prompt_input = input(PROMPT);
        prompt_input
            .images
            .push(ImageSource::new(ImageMediaType::Png, IMAGE_DATA.into()));
        if compact {
            push_compaction(&handles, Some(PROMPT), false);
        } else {
            handles.queue.push(QueueItem::Message(QueuedInput {
                text: PROMPT.into(),
                input: prompt_input,
                run_id: RUN_ID,
                displayed: true,
            }));
        }
        let request = peer.recv();
        if malformed {
            peer.respond(&request, json!({}));
        } else {
            peer.send(json!({"jsonrpc":"2.0", "id":request["id"],
                "error":{"code":-32603, "message":ENQUEUE_REJECTED}}));
        }
        if let Some(id) = running {
            host_receipt(&peer, id, QueueStatus::Done);
        }
        crate::test_block_on(future::or(
            async {
                loop {
                    let envelope = handles.agent_rx.recv_async().await.unwrap();
                    if matches!(envelope.event, AgentEvent::QueueDrained) {
                        break;
                    }
                    app.update(crate::app::Msg::Agent(Box::new(envelope)));
                }
            },
            async {
                let retry = peer.from_client.recv_async().await.unwrap();
                panic!("rejected input was retried without user input: {retry}");
            },
        ));
        assert!(handles.queue.is_empty());
        let submission = app.input_box.submit().unwrap();
        let text = if compact {
            format!("/compact {PROMPT}")
        } else {
            PROMPT.into()
        };
        assert_eq!(submission.text, format!("{text}\n{GUIDANCE}"));
        assert_eq!(submission.images.len(), usize::from(!compact));
        if let Some(image) = submission.images.first() {
            assert_eq!(image.data.as_ref(), IMAGE_DATA);
        }
        assert!(peer.from_client.try_recv().is_err());
        if compact {
            push_compaction(&handles, Some(PROMPT), false);
        } else {
            handles.queue.push(QueueItem::Message(QueuedInput {
                text: submission.text.clone(),
                input: AgentInput {
                    message: submission.text,
                    images: submission.images,
                    ..input(PROMPT)
                },
                run_id: RUN_ID,
                displayed: true,
            }));
        }
        let retry = peer.recv();
        let id = accepted(&peer, &retry);
        assert_ne!(retry["params"]["id"], request["params"]["id"]);
        host_receipt(&peer, id, QueueStatus::Done);
        assert_eq!(
            done_reason(&run_events(&handles)),
            Some(if compact {
                DoneReason::Compact
            } else {
                DoneReason::EndTurn
            })
        );
    }

    #[test]
    fn a_host_outcome_before_acceptance_is_drawn_once() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_sessions();
        queue_message(&handles, QUEUED, AgentMode::Build);
        let request = peer.recv();
        let id = serde_json::from_value(request["params"]["id"].clone()).unwrap();
        host_receipt(&peer, id, QueueStatus::Running);
        host_receipt(&peer, id, QueueStatus::Done);
        accepted(&peer, &request);
        let events = run_events(&handles);
        assert_eq!(events.iter().filter(|event| matches!(event, AgentEvent::QueueItemConsumed { text, .. } if text == QUEUED)).count(), 1);
        assert_eq!(done_reason(&events), Some(DoneReason::EndTurn));
        assert!(handles.queue.is_empty());
    }

    #[test]
    fn old_queue_updates_cannot_enter_a_replaced_session() {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer) = remote_sessions();
        handles.rebind_queue(NEXT_RUN_ID);
        let close = peer.recv();
        assert_eq!(close["params"]["operation"], "close");
        peer.respond(&close, true);
        let new = peer.recv();
        let receipt = QueueReceipt {
            id: MakiId::generate(),
            status: QueueStatus::Running,
            error: None,
            input: Some(serde_json::to_value(SessionInput::Prompt(input(QUEUED))).unwrap()),
        };
        peer.send(
            json!({ "jsonrpc": "2.0", "method": maki_wire::session::QUEUE_UPDATE_METHOD,
            "params": { "sessionId": SESSION, "receipt": receipt } }),
        );
        peer.respond(&new, new_session_answer(NEW_SESSION, None));
        drop(peer);
        assert!(
            !events_until_end(handles)
                .iter()
                .any(|event| matches!(event, AgentEvent::QueueItemConsumed { .. }))
        );
    }

    /// What the client read off the link before the loop started is the
    /// loop's first word: a command list the agent sent right after a
    /// reload's load still offers compaction.
    #[test]
    fn events_read_before_the_loop_are_handled_first() {
        let _runtime = crate::test_runtime().enter();
        let early = vec![ClientEvent::Update(SESSION_SEQ, commands::advertised())];
        let (handles, peer) = remote_hosted(
            false,
            None,
            None,
            Agreed::default(),
            EventHandle::disconnected_for_test(),
            early,
        );
        push_compaction(&handles, None, false);

        let prompt = peer.recv();
        assert_eq!(prompt["method"], "session/prompt");
    }

    #[test]
    fn compaction_the_agent_does_not_offer_is_refused() {
        let _runtime = crate::test_runtime().enter();
        let (handles, _peer) = remote();
        push_compaction(&handles, None, false);
        let events = run_events(&handles);
        assert!(
            matches!(&events[0], AgentEvent::Error { message, .. } if message == UNAVAILABLE_REMOTE)
        );
    }

    /// No switch goes first, and the workflow is the one it was queued
    /// under, not the intent's.
    #[test]
    fn compaction_sends_the_compact_prompt_with_its_captured_workflow() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer, _settings) = with_settings(false, false);
        offer_compaction(&handles, &peer);
        push_compaction(&handles, Some(GUIDANCE), true);

        let prompt = peer.recv();
        assert_eq!(prompt["method"], "session/prompt");
        let text = prompt["params"]["prompt"][0]["text"].as_str().unwrap();
        assert_eq!(text, format!("/{} {GUIDANCE}", commands::COMPACT));
        assert_eq!(
            prompt["params"]["_meta"]["maki"]["settings"]["workflow"],
            true
        );
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        assert_eq!(
            done_reason(&run_events(&handles)),
            Some(DoneReason::Compact)
        );
    }

    #[test]
    fn esc_during_a_compaction_cancels_it() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        offer_compaction(&handles, &peer);
        push_compaction(&handles, None, false);
        let prompt = peer.recv();
        handles.cancel_run(RUN_ID);
        assert_eq!(peer.recv()["method"], "session/cancel");
        peer.respond(&prompt, PromptResponse::new(StopReason::Cancelled));
        assert_eq!(
            done_reason(&run_events(&handles)),
            Some(DoneReason::Cancelled)
        );
    }

    #[test]
    fn lost_link_ends_the_loop_with_an_error() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        peer.recv();
        drop(peer);
        let events = events_until_end(handles);
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
            "the loss is the session's, so no run error goes out"
        );
    }

    /// Shutdown must not wait for a peer that never answers the prompt.
    #[test]
    fn teardown_ends_a_pending_prompt() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        peer.recv();
        handles.cancel_all();
        crate::test_block_on(handles.into_task()).unwrap();
    }

    #[test]
    fn a_running_prompt_finishes_after_its_input_channels_close() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        let updates = handles.agent_rx.clone();
        let task = handles.into_task();
        let _ = crate::test_block_on(future::zip(task, async {
            for text in [PROMPT, REPLY] {
                peer.send(
                    json!({"jsonrpc":"2.0", "method":"session/update", "params":{
                    "sessionId":SESSION, "update":{"sessionUpdate":"agent_message_chunk",
                        "content":{"type":"text", "text":text}}}}),
                );
                assert!(matches!(updates.recv_async().await.unwrap().event,
                    AgentEvent::TextDelta { text: received } if received == text));
            }
            peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        }));
    }

    const QUESTION_ID: &str = "question-1";
    const PICK: &str = "axum";
    /// A bound on a wait a working host answers at once, never a pause.
    const FORM_WAIT: Duration = Duration::from_secs(10);

    fn question_request() -> Value {
        let input = json!({ "questions": [{ "question": "Framework?", "header": "Framework", "options": [{ "label": PICK }] }] });
        let request = elicitation::form_request(SESSION, None, &input, true).unwrap();
        json!({"jsonrpc": "2.0", "id": QUESTION_ID, "method": "elicitation/create", "params": request})
    }

    /// A run that asks a question, and the window of the form it opened.
    struct Asked {
        _host: maki_lua_ui::UiHost,
        handles: AgentHandles,
        peer: Peer,
        prompt: Value,
        keys: flume::Sender<WinEvent>,
        commands: flume::Receiver<WinCommand>,
    }

    /// A live prompt on a loop with its own Lua host.
    fn hosted() -> (maki_lua_ui::UiHost, AgentHandles, Peer, Value) {
        let host = maki_lua_ui::UiHost::bare();
        let (handles, peer) = remote_hosted(
            false,
            None,
            None,
            Agreed::default(),
            EventHandle::from_ui_host(&host),
            Vec::new(),
        );
        push_prompt(&handles);
        let prompt = peer.recv();
        (host, handles, peer, prompt)
    }

    /// The question's answer among what the client sends next, bounded, so
    /// a form left open fails the test instead of hanging it.
    fn question_answer(peer: &Peer) -> Value {
        iter::from_fn(|| peer.from_client.recv_timeout(FORM_WAIT).ok())
            .map(|line| serde_json::from_str::<Value>(&line).unwrap())
            .find(|message| message["id"] == QUESTION_ID)
            .expect("the question is answered")
    }

    /// Esc or `/new` is in before the loop reads the question.
    #[test_case(true ; "cancelled")]
    #[test_case(false ; "replaced")]
    fn a_run_that_already_ended_gets_no_form(cancelled: bool) {
        let _runtime = crate::test_runtime().enter();
        let (_host, mut handles, peer, _prompt) = hosted();
        if cancelled {
            handles.cancel_run(RUN_ID);
        } else {
            handles.rebind_queue(NEXT_RUN_ID);
        }
        peer.send(question_request());
        assert_eq!(question_answer(&peer)["result"]["action"], "cancel");
    }

    fn asked() -> Asked {
        let (host, handles, peer, prompt) = hosted();
        peer.send(question_request());
        let (keys, commands) = loop {
            if let UiAction::OpenWin {
                event_tx, cmd_rx, ..
            } = host.ui_action_rx().recv_timeout(FORM_WAIT).unwrap()
            {
                break (event_tx, cmd_rx);
            }
        };
        Asked {
            _host: host,
            handles,
            peer,
            prompt,
            keys,
            commands,
        }
    }

    #[test]
    fn a_question_is_answered_from_the_clients_own_form() {
        let _runtime = crate::test_runtime().enter();
        let asked = asked();
        let key = Key::parse("<CR>").unwrap();
        asked.keys.send(WinEvent::Key { key }).unwrap();
        let answer = asked.peer.recv();
        assert_eq!(answer["id"], QUESTION_ID);
        assert_eq!(answer["result"]["action"], "accept");
        assert_eq!(
            answer["result"]["_meta"]["maki"]["question"]["answers"],
            json!([[PICK]])
        );
        asked
            .peer
            .respond(&asked.prompt, PromptResponse::new(StopReason::EndTurn));
        run_events(&asked.handles);
    }

    /// No run owns a form while the tab is idle.
    #[test]
    fn a_question_without_a_live_prompt_is_dismissed() {
        let _runtime = crate::test_runtime().enter();
        let (_handles, peer) = remote();
        peer.send(question_request());
        let answer = peer.recv();
        assert_eq!(answer["id"], QUESTION_ID);
        assert_eq!(answer["result"]["action"], "cancel");
    }

    /// No run is left to read the answer once the agent ended it, so the
    /// form goes with the run, as its permission asks do.
    #[test]
    fn the_end_of_its_run_closes_the_form_and_dismisses_the_question() {
        let _runtime = crate::test_runtime().enter();
        let asked = asked();
        asked.handles.cancel_run(RUN_ID);
        assert_eq!(asked.peer.recv()["method"], "session/cancel");
        asked
            .peer
            .respond(&asked.prompt, PromptResponse::new(StopReason::Cancelled));
        let answer = asked.peer.recv();
        assert_eq!(answer["id"], QUESTION_ID);
        assert_eq!(answer["result"]["action"], "cancel");
        let closed = iter::from_fn(|| asked.commands.recv_timeout(FORM_WAIT).ok())
            .any(|command| matches!(command, WinCommand::Close));
        assert!(closed, "the form stays open after its run");
        run_events(&asked.handles);
    }

    const PLAN_PATH: &str = "/agent/plans/plan.md";
    const FIRST_CALL: &str = "write-1";
    const SECOND_CALL: &str = "write-2";
    const FIRST_PLAN: &str = "# First";
    const SECOND_PLAN: &str = "# Second";

    /// A write the agent marked as its plan's, start and completion.
    fn plan_write(call: &str, text: &str) -> [Value; 2] {
        let update = |update: Value| {
            json!({"jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": SESSION, "update": update}})
        };
        [
            update(
                json!({"sessionUpdate": "tool_call", "toolCallId": call, "title": "write",
                "kind": "edit", "status": "in_progress"}),
            ),
            update(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": call,
                "status": "completed",
                "_meta": {"maki": {"plan": {"path": PLAN_PATH, "text": text}}}}),
            ),
        ]
    }

    /// Both writes of a run, done before the App handles either.
    fn two_plan_writes() -> (AgentHandles, Peer) {
        let plan = Agreed {
            plan: true,
            ..Agreed::default()
        };
        let (handles, peer) = remote_settled(false, None, None, plan);
        push_prompt(&handles);
        let prompt = peer.recv();
        for (call, text) in [(FIRST_CALL, FIRST_PLAN), (SECOND_CALL, SECOND_PLAN)] {
            for message in plan_write(call, text) {
                peer.send(message);
            }
        }
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        let events = run_events(&handles);
        let done = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolDone(_)))
            .count();
        assert_eq!(done, 2);
        (handles, peer)
    }

    #[test]
    fn each_plan_write_is_kept_for_its_own_tool_done() {
        let _runtime = crate::test_runtime().enter();
        let (handles, _peer) = two_plan_writes();
        let plan = handles.remote_plan.clone().unwrap();
        for (call, text) in [(SECOND_CALL, SECOND_PLAN), (FIRST_CALL, FIRST_PLAN)] {
            let written = plan.take(call).unwrap();
            assert_eq!(written.path, PLAN_PATH);
            assert_eq!(written.text.as_deref(), Some(text));
        }
    }

    #[test]
    fn a_new_session_drops_the_old_plan_writes() {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer) = two_plan_writes();
        handles.rebind_queue(NEXT_RUN_ID);
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        answer_new(&peer, NEW_SESSION, None);
        finish_prompt(&peer, NEW_SESSION);
        let plan = handles.remote_plan.clone().unwrap();
        assert!(plan.writes.lock().unwrap().is_empty());
    }

    fn permission_request() -> Value {
        json!({"jsonrpc": "2.0", "id": "ask-1", "method": "session/request_permission", "params": {
            "sessionId": SESSION,
            "toolCall": {"toolCallId": "call-1", "title": "bash: rm -rf build"},
            "options": [
                {"optionId": ALLOW_ONCE, "name": "Allow", "kind": "allow_once"},
                {"optionId": ALLOW_TWICE, "name": "Allow, same kind", "kind": "allow_once"},
                {"optionId": ALLOW_ALWAYS, "name": "Always", "kind": "allow_always"},
                {"optionId": REJECT_ONCE, "name": "Reject", "kind": "reject_once"},
                {"optionId": REJECT_ALWAYS, "name": "Never", "kind": "reject_always"},
            ]
        }})
    }

    /// The ask the UI is shown, and the local id it answers with.
    fn ask(handles: &AgentHandles, peer: &Peer) -> (String, Vec<PermissionChoice>) {
        peer.send(permission_request());
        match handles.agent_rx.recv().unwrap().event {
            AgentEvent::PermissionRequest {
                id, tool, choices, ..
            } => {
                assert_eq!(tool, ToolKey::Native(REMOTE_TOOL.into()));
                (id, choices)
            }
            other => panic!("expected a permission request, got {other:?}"),
        }
    }

    #[test]
    fn ask_shows_the_agents_own_options_in_order() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        let (_, choices) = ask(&handles, &peer);
        let ids: Vec<&str> = choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                ALLOW_ONCE,
                ALLOW_TWICE,
                ALLOW_ALWAYS,
                REJECT_ONCE,
                REJECT_ALWAYS
            ]
        );
        let allows: Vec<bool> = choices.iter().map(|c| c.allow).collect();
        assert_eq!(allows, [true, true, true, false, false]);
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        run_events(&handles);
    }

    #[test_case(ALLOW_ONCE ; "allow_once")]
    #[test_case(ALLOW_TWICE ; "second_option_of_the_same_kind")]
    #[test_case(REJECT_ALWAYS ; "reject_always")]
    fn answer_sends_the_exact_option(option: &str) {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        let (id, _) = ask(&handles, &peer);
        handles
            .answer_tx
            .send(AnsweredRequest {
                id,
                answer: Some(option.into()),
            })
            .unwrap();
        let reply = peer.recv();
        assert_eq!(reply["id"], "ask-1");
        assert_eq!(reply["result"]["outcome"]["optionId"], option);
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        run_events(&handles);
    }

    #[test]
    fn cancel_answers_cancelled() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        let (id, _) = ask(&handles, &peer);
        handles
            .answer_tx
            .send(AnsweredRequest { id, answer: None })
            .unwrap();
        assert_eq!(peer.recv()["result"]["outcome"]["outcome"], "cancelled");
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        run_events(&handles);
    }

    /// An unoffered id is never sent. The ask stays open, so the next thing
    /// the agent hears about it is the cancel when the run ends.
    #[test]
    fn unoffered_option_is_never_sent() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        let (id, _) = ask(&handles, &peer);
        handles
            .answer_tx
            .send(AnsweredRequest {
                id,
                answer: Some(UNOFFERED.into()),
            })
            .unwrap();
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        let reply = peer.recv();
        assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
        run_events(&handles);
    }

    #[test]
    fn ask_without_options_is_cancelled_unseen() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        let mut request = permission_request();
        request["params"]["options"] = json!([]);
        peer.send(request);
        assert_eq!(peer.recv()["result"]["outcome"]["outcome"], "cancelled");
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        let events = run_events(&handles);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::PermissionRequest { .. }))
        );
    }

    fn mode_update(mode: &str) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": SESSION,
            "update": {"sessionUpdate": "current_mode_update", "currentModeId": mode}
        }})
    }

    #[test]
    fn plan_prompt_switches_the_agent_first() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_with(true, Some(BUILD_MODE));
        push_prompt_in(&handles, plan());
        let switch = peer.recv();
        assert_eq!(switch["method"], "session/set_mode");
        assert_eq!(switch["params"]["modeId"], PLAN_MODE);
        peer.respond(&switch, json!({}));
        let prompt = peer.recv();
        assert_eq!(prompt["method"], "session/prompt");
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        assert_eq!(
            done_reason(&run_events(&handles)),
            Some(DoneReason::EndTurn)
        );
    }

    #[test]
    fn prompt_in_the_current_mode_sends_no_switch() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_with(true, Some(BUILD_MODE));
        push_prompt(&handles);
        let prompt = peer.recv();
        assert_eq!(prompt["method"], "session/prompt");
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        run_events(&handles);
    }

    /// Without the build and plan pair the agent's modes are its own, and a
    /// plan prompt goes out unchanged.
    #[test]
    fn agent_without_plan_modes_gets_no_switch() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_with(false, Some("code"));
        push_prompt_in(&handles, plan());
        assert_eq!(peer.recv()["method"], "session/prompt");
    }

    fn no_more_requests(peer: &Peer) {
        assert!(peer.from_client.try_recv().is_err(), "no prompt may follow");
    }

    #[test]
    fn failed_switch_ends_the_run_without_the_prompt() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_with(true, Some(BUILD_MODE));
        push_prompt_in(&handles, plan());
        let switch = peer.recv();
        peer.send(json!({"jsonrpc": "2.0", "id": switch["id"],
            "error": {"code": -32602, "message": "unknown mode"}}));
        let events = run_events(&handles);
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::Error { message, .. } if message.starts_with(MODE_NOT_SET))
        ));
        no_more_requests(&peer);
    }

    /// The agent said build before it answered the switch to plan. On the
    /// wire the answer is newer, so the agent is in plan and the prompt goes.
    #[test]
    fn update_older_than_the_switch_answer_loses() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_with(true, Some(BUILD_MODE));
        push_prompt_in(&handles, plan());
        let switch = peer.recv();
        peer.send(mode_update(BUILD_MODE));
        peer.respond(&switch, json!({}));
        let prompt = peer.recv();
        assert_eq!(prompt["method"], "session/prompt");
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        run_events(&handles);
    }

    #[test]
    fn cancel_during_the_switch_sends_no_prompt_and_keeps_the_mode() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_with(true, Some(BUILD_MODE));
        push_prompt_in(&handles, plan());
        let switch = peer.recv();
        handles.cancel_run(RUN_ID);
        peer.respond(&switch, json!({}));
        let events = run_events(&handles);
        assert_eq!(done_reason(&events), Some(DoneReason::Cancelled));
        no_more_requests(&peer);

        push_run(&handles, plan(), NEXT_RUN_ID);
        assert_eq!(peer.recv()["method"], "session/prompt");
    }

    /// The ask arrives right before the failure, unread: it is still
    /// drained and cancelled before the run ends.
    #[test]
    fn ask_queued_before_a_failed_switch_is_cancelled() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_with(true, Some(BUILD_MODE));
        push_prompt_in(&handles, plan());
        let switch = peer.recv();
        peer.send(permission_request());
        peer.send(json!({"jsonrpc": "2.0", "id": switch["id"],
            "error": {"code": -32602, "message": "unknown mode"}}));
        let reply = peer.recv();
        assert_eq!(reply["id"], "ask-1");
        assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
        run_events(&handles);
        no_more_requests(&peer);
    }

    /// After Esc no prompt is on screen for the cancelled run, so an ask
    /// sent while the switch settles is cancelled at once.
    #[test]
    fn ask_during_a_cancelled_switch_is_cancelled_at_once() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_with(true, Some(BUILD_MODE));
        push_prompt_in(&handles, plan());
        let switch = peer.recv();
        handles.cancel_run(RUN_ID);
        peer.send(permission_request());
        let reply = peer.recv();
        assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
        peer.respond(&switch, json!({}));
        assert_eq!(
            done_reason(&run_events(&handles)),
            Some(DoneReason::Cancelled)
        );
    }

    /// An ask the agent sent during the switch belongs to the run the
    /// switch failed, so it is cancelled with it.
    #[test]
    fn failed_switch_retires_its_asks() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_with(true, Some(BUILD_MODE));
        push_prompt_in(&handles, plan());
        let switch = peer.recv();
        peer.send(permission_request());
        handles.agent_rx.recv().unwrap();
        peer.send(json!({"jsonrpc": "2.0", "id": switch["id"],
            "error": {"code": -32602, "message": "unknown mode"}}));
        let reply = peer.recv();
        assert_eq!(reply["id"], "ask-1");
        assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
        run_events(&handles);
        no_more_requests(&peer);
    }

    /// Each queued prompt carries the mode it was typed in, in queue order.
    #[test]
    fn queued_prompts_keep_their_modes() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote_with(true, Some(BUILD_MODE));
        push_prompt_in(&handles, plan());
        push_prompt_in(&handles, AgentMode::Build);
        let switch = peer.recv();
        assert_eq!(switch["params"]["modeId"], PLAN_MODE);
        peer.respond(&switch, json!({}));
        let first = peer.recv();
        peer.respond(&first, PromptResponse::new(StopReason::EndTurn));
        let back = peer.recv();
        assert_eq!(back["params"]["modeId"], BUILD_MODE);
        peer.respond(&back, json!({}));
        let second = peer.recv();
        assert_eq!(second["method"], "session/prompt");
        peer.respond(&second, PromptResponse::new(StopReason::EndTurn));
        run_events(&handles);
    }

    #[test]
    fn unanswered_ask_is_cancelled_when_the_run_ends() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        peer.send(permission_request());
        handles.agent_rx.recv().unwrap();
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        let reply = peer.recv();
        assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
    }

    /// A burst goes out oldest first, each message after its shell result
    /// and before its images.
    #[test]
    fn prompt_blocks_keep_the_burst_in_order() {
        let _runtime = crate::test_runtime().enter();
        let mut last = input(PROMPT);
        last.images = vec![ImageSource::new(ImageMediaType::Png, IMAGE_DATA.into())];
        last.earlier = vec![EarlierInput {
            message: EARLIER.into(),
            images: Vec::new(),
            preamble: vec![Message::user(SHELL_RESULT.into())],
        }];
        let blocks = serde_json::to_value(prompt_blocks(&last)).unwrap();
        assert_eq!(blocks[0]["text"], SHELL_RESULT);
        assert_eq!(blocks[1]["text"], EARLIER);
        assert_eq!(blocks[2]["text"], PROMPT);
        assert_eq!(blocks[3]["type"], "image");
        assert_eq!(blocks[3]["mimeType"], "image/png");
        assert_eq!(blocks.as_array().unwrap().len(), 4);
    }

    const MODEL_A: &str = "vendor/model-a";
    const MODEL_CONFIG: &str = "model";
    /// The `session/new` answer is the first line the client numbers.
    const SESSION_SEQ: u64 = 1;
    const MODEL_B: &str = "vendor/model-b";

    fn model_options(current: &str) -> Value {
        json!([{
            "id": "model", "name": "Model", "type": "select", "category": "model",
            "currentValue": current,
            "options": [{"value": MODEL_A, "name": MODEL_A}, {"value": MODEL_B, "name": MODEL_B}],
        }])
    }

    fn models(current: &str) -> RemoteSettings {
        let options: Vec<SessionConfigOption> =
            serde_json::from_value(model_options(current)).unwrap();
        RemoteSettings::from_options(&options)
    }

    fn current_model(models: &RemoteSettings) -> Option<String> {
        models
            .models
            .load()
            .as_ref()
            .and_then(|menu| menu.current.clone())
    }

    fn want(models: &RemoteSettings, value: &str) {
        models.pick(ModelPick {
            config_id: MODEL_CONFIG.into(),
            value: value.into(),
        });
    }

    #[test]
    fn picked_model_switches_the_agent_before_the_prompt() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (handles, peer) = remote_full(false, None, Some(models.clone()));
        want(&models, MODEL_B);
        push_prompt(&handles);
        let switch = peer.recv();
        assert_eq!(switch["method"], "session/set_config_option");
        assert_eq!(switch["params"]["configId"], "model");
        assert_eq!(switch["params"]["value"], MODEL_B);
        peer.respond(&switch, json!({"configOptions": model_options(MODEL_B)}));
        let prompt = peer.recv();
        assert_eq!(prompt["method"], "session/prompt");
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        run_events(&handles);
        assert_eq!(current_model(&models).as_deref(), Some(MODEL_B));
        assert!(models.wanted().is_none());
    }

    #[test]
    fn picked_current_model_sends_no_switch() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (handles, peer) = remote_full(false, None, Some(models.clone()));
        want(&models, MODEL_A);
        push_prompt(&handles);
        assert_eq!(peer.recv()["method"], "session/prompt");
        assert!(models.wanted().is_none());
    }

    #[test]
    fn config_update_replaces_the_menu() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (handles, peer) = remote_full(false, None, Some(models.clone()));
        push_prompt(&handles);
        let prompt = peer.recv();
        peer.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": SESSION,
            "update": {"sessionUpdate": "config_option_update", "configOptions": model_options(MODEL_B)}
        }}));
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        run_events(&handles);
        assert_eq!(current_model(&models).as_deref(), Some(MODEL_B));
    }

    #[test_case(json!({"error": {"code": -32602, "message": "not allowed"}}) ; "refused")]
    #[test_case(json!({"result": {"configOptions": model_options(MODEL_A)}}) ; "kept_another_model")]
    #[test_case(json!({"result": {}}) ; "malformed_answer")]
    fn unswitched_model_ends_the_run_without_the_prompt(answer: Value) {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (handles, peer) = remote_full(false, None, Some(models.clone()));
        want(&models, MODEL_B);
        push_prompt(&handles);
        let switch = peer.recv();
        let mut reply = json!({"jsonrpc": "2.0", "id": switch["id"]});
        for (key, value) in answer.as_object().unwrap() {
            reply[key] = value.clone();
        }
        peer.send(reply);
        let events = run_events(&handles);
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::Error { message, .. } if message.starts_with(MODEL_NOT_SET))
        ));
        no_more_requests(&peer);
        assert!(models.wanted().is_none(), "a refused model is not retried");
    }

    #[test]
    fn cancel_during_the_model_switch_sends_no_prompt() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (handles, peer) = remote_full(false, None, Some(models.clone()));
        want(&models, MODEL_B);
        push_prompt(&handles);
        let switch = peer.recv();
        handles.cancel_run(RUN_ID);
        peer.respond(&switch, json!({"configOptions": model_options(MODEL_B)}));
        assert_eq!(
            done_reason(&run_events(&handles)),
            Some(DoneReason::Cancelled)
        );
        no_more_requests(&peer);
        assert_eq!(current_model(&models).as_deref(), Some(MODEL_B));
    }

    fn model_options_with(current: &str, extra: &str) -> Value {
        json!([{
            "id": MODEL_CONFIG, "name": "Model", "type": "select", "category": "model",
            "currentValue": current,
            "options": [
                {"value": MODEL_A, "name": MODEL_A},
                {"value": MODEL_B, "name": MODEL_B},
                {"value": extra, "name": extra},
            ],
        }])
    }

    pub(crate) fn config_update(options: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": SESSION,
            "update": {"sessionUpdate": "config_option_update", "configOptions": options}
        }})
    }

    fn choice_values(models: &RemoteSettings) -> Vec<String> {
        models
            .models
            .load()
            .as_ref()
            .map(|menu| menu.choices.iter().map(|c| c.value.clone()).collect())
            .unwrap_or_default()
    }

    const MODEL_C: &str = "vendor/model-c";

    /// The mode check comes after the model switch, whose drain can carry
    /// the agent's word that it left the mode the prompt was typed in.
    #[test]
    fn mode_change_during_the_model_switch_stops_the_prompt() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (handles, peer) = remote_full(true, Some(PLAN_MODE), Some(models.clone()));
        want(&models, MODEL_B);
        push_prompt_in(&handles, plan());
        let switch = peer.recv();
        assert_eq!(switch["method"], "session/set_config_option");
        peer.send(mode_update(BUILD_MODE));
        peer.respond(&switch, json!({"configOptions": model_options(MODEL_B)}));
        let events = run_events(&handles);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error { message, .. } if message == MODE_NOT_SET))
        );
        no_more_requests(&peer);
    }

    /// An update the agent sent before its answer is older, so the answer's
    /// menu wins.
    #[test]
    fn switch_answer_wins_over_an_older_update() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (handles, peer) = remote_full(false, None, Some(models.clone()));
        want(&models, MODEL_B);
        push_prompt(&handles);
        let switch = peer.recv();
        peer.send(config_update(model_options_with(MODEL_A, MODEL_C)));
        peer.respond(&switch, json!({"configOptions": model_options(MODEL_B)}));
        let prompt = peer.recv();
        assert_eq!(prompt["method"], "session/prompt");
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        run_events(&handles);
        assert_eq!(current_model(&models).as_deref(), Some(MODEL_B));
        assert_eq!(choice_values(&models), [MODEL_A, MODEL_B]);
    }

    /// An update the agent sent after its answer is newer, so it wins: a
    /// model discovery found after the switch is kept.
    #[test]
    fn newer_update_wins_over_the_switch_answer() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (handles, peer) = remote_full(false, None, Some(models.clone()));
        want(&models, MODEL_B);
        push_prompt(&handles);
        let switch = peer.recv();
        peer.respond(&switch, json!({"configOptions": model_options(MODEL_B)}));
        peer.send(config_update(model_options_with(MODEL_B, MODEL_C)));
        let prompt = peer.recv();
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        run_events(&handles);
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        let next = peer.recv();
        assert_eq!(next["method"], "session/prompt");
        assert_eq!(choice_values(&models), [MODEL_A, MODEL_B, MODEL_C]);
    }

    #[test]
    fn update_without_a_model_option_removes_the_menu() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (handles, peer) = remote_full(false, None, Some(models.clone()));
        push_prompt(&handles);
        let prompt = peer.recv();
        peer.send(config_update(json!([])));
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        run_events(&handles);
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        peer.recv();
        assert!(models.models.load().is_none());
    }

    /// The pick names an option the agent has since replaced, so it is never
    /// sent against the new one.
    #[test]
    fn pick_from_a_replaced_option_is_refused_unsent() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (handles, peer) = remote_full(false, None, Some(models.clone()));
        models.pick(ModelPick {
            config_id: "replaced-option".into(),
            value: MODEL_B.into(),
        });
        push_prompt(&handles);
        let events = run_events(&handles);
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::Error { message, .. } if message.contains(MODEL_NOT_OFFERED))
        ));
        no_more_requests(&peer);
    }

    /// The answer swaps in another option whose current value is the same
    /// string. That is not the picked model, so no prompt goes.
    #[test]
    fn replaced_option_with_the_same_value_is_not_confirmed() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (handles, peer) = remote_full(false, None, Some(models.clone()));
        want(&models, MODEL_B);
        push_prompt(&handles);
        let switch = peer.recv();
        let mut replaced = model_options(MODEL_B);
        replaced[0]["id"] = json!("other-option");
        peer.respond(&switch, json!({"configOptions": replaced}));
        let events = run_events(&handles);
        assert!(
            events.iter().any(
                |e| matches!(e, AgentEvent::Error { message, .. } if message == MODEL_NOT_SET)
            )
        );
        no_more_requests(&peer);
    }

    const NEW_SESSION: &str = "remote-session-2";
    const NEWER_SESSION: &str = "remote-session-3";
    const LATER_RUN_ID: u64 = 3;

    /// Every envelope, of any run, up to the queue of `run_id` draining.
    fn envelopes_until_drained(handles: &AgentHandles, run_id: u64) -> Vec<Envelope> {
        let mut envelopes = Vec::new();
        loop {
            let envelope = handles.agent_rx.recv().unwrap();
            let drained =
                envelope.run_id == run_id && matches!(envelope.event, AgentEvent::QueueDrained);
            envelopes.push(envelope);
            if drained {
                return envelopes;
            }
        }
    }

    fn new_session_answer(session: &str, options: Option<Value>) -> Value {
        let mut answer = serde_json::to_value(methods::new_session_response(session)).unwrap();
        if let Some(options) = options {
            answer["configOptions"] = options;
        }
        answer
    }

    /// Opens the next session and returns the request the loop sends after it.
    fn answer_new(peer: &Peer, session: &str, options: Option<Value>) {
        let request = peer.recv();
        assert_eq!(request["method"], "session/new");
        assert_eq!(request["params"]["cwd"], CWD);
        peer.respond(&request, new_session_answer(session, options));
    }

    fn finish_prompt(peer: &Peer, session: &str) {
        let prompt = peer.recv();
        assert_eq!(prompt["method"], "session/prompt");
        assert_eq!(prompt["params"]["sessionId"], session);
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
    }

    /// The prompt typed after `/new` waits in the new queue until the new
    /// session's answer, and the old queue closing is not a shutdown: the
    /// rebind is sent first, so both are ready when the idle loop wakes.
    #[test]
    fn idle_rebind_opens_the_new_session_before_its_prompt() {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer) = remote();
        handles.rebind_queue(NEXT_RUN_ID);
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        answer_new(&peer, NEW_SESSION, None);
        finish_prompt(&peer, NEW_SESSION);
        let envelopes = envelopes_until_drained(&handles, NEXT_RUN_ID);
        assert!(envelopes.iter().all(|e| e.run_id == NEXT_RUN_ID));
    }

    #[test]
    fn rebind_during_a_run_cancels_it_and_draws_nothing_more_of_it() {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        handles.rebind_queue(NEXT_RUN_ID);
        assert_eq!(peer.recv()["method"], "session/cancel");
        let new = peer.recv();
        assert_eq!(new["method"], "session/new");
        peer.send(text_update(REPLY));
        peer.respond(&prompt, PromptResponse::new(StopReason::Cancelled));
        peer.respond(&new, new_session_answer(NEW_SESSION, None));
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        finish_prompt(&peer, NEW_SESSION);
        let envelopes = envelopes_until_drained(&handles, NEXT_RUN_ID);
        assert!(
            !envelopes.iter().any(|e| matches!(
                &e.event,
                AgentEvent::TextDelta { .. } | AgentEvent::Done { .. }
            ) && e.run_id == RUN_ID),
            "{:?}",
            envelopes.iter().map(|e| &e.event).collect::<Vec<_>>()
        );
        assert!(!envelopes.iter().any(|e| matches!(&e.event,
            AgentEvent::TextDelta { text } if text == REPLY)));
    }

    /// The prompt never went out, so nothing is cancelled: the switch is
    /// abandoned and the new session opens.
    #[test]
    fn rebind_during_a_mode_switch_opens_the_new_session() {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer) = remote_with(true, Some(BUILD_MODE));
        push_prompt_in(&handles, plan());
        assert_eq!(peer.recv()["method"], "session/set_mode");
        handles.rebind_queue(NEXT_RUN_ID);
        answer_new(&peer, NEW_SESSION, None);
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        finish_prompt(&peer, NEW_SESSION);
    }

    #[test]
    fn prompts_queued_for_the_old_session_are_not_sent() {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        push_prompt(&handles);
        handles.rebind_queue(NEXT_RUN_ID);
        assert_eq!(peer.recv()["method"], "session/cancel");
        peer.respond(&prompt, PromptResponse::new(StopReason::Cancelled));
        answer_new(&peer, NEW_SESSION, None);
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        finish_prompt(&peer, NEW_SESSION);
    }

    /// A replacement is never abandoned: its answer binds the client. The
    /// second `/new` runs after it, and the last one's queue is served.
    #[test]
    fn a_second_new_waits_for_the_first_and_then_opens_another() {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer) = remote();
        handles.rebind_queue(NEXT_RUN_ID);
        let first = peer.recv();
        assert_eq!(first["method"], "session/new");
        handles.rebind_queue(LATER_RUN_ID);
        push_run(&handles, AgentMode::Build, LATER_RUN_ID);
        peer.respond(&first, new_session_answer(NEW_SESSION, None));
        answer_new(&peer, NEWER_SESSION, None);
        finish_prompt(&peer, NEWER_SESSION);
    }

    fn config_update_for(session: &str, options: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": session,
            "update": {"sessionUpdate": "config_option_update", "configOptions": options}
        }})
    }

    /// Whichever the loop reads first, the wire order decides: the old
    /// session's update before the answer is dropped, and the new session's
    /// update after it wins over the answer.
    #[test_case(SESSION, false, MODEL_A ; "older_update_is_dropped")]
    #[test_case(NEW_SESSION, true, MODEL_B ; "newer_update_wins")]
    fn updates_around_the_new_session_answer_follow_the_wire(
        session: &str,
        after: bool,
        expected: &str,
    ) {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (mut handles, peer) = remote_full(false, None, Some(models.clone()));
        handles.rebind_queue(NEXT_RUN_ID);
        let new = peer.recv();
        let update = config_update_for(session, model_options(MODEL_B));
        if !after {
            peer.send(update.clone());
        }
        peer.respond(
            &new,
            new_session_answer(NEW_SESSION, Some(model_options(MODEL_A))),
        );
        if after {
            peer.send(update);
        }
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        finish_prompt(&peer, NEW_SESSION);
        envelopes_until_drained(&handles, NEXT_RUN_ID);
        assert_eq!(current_model(&models).as_deref(), Some(expected));
    }

    /// A side question between `/new` and the new session's answer would ask
    /// the conversation the user left.
    #[test]
    fn the_binding_follows_a_replacement_once_it_is_installed() {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer) = remote();
        handles.rebind_queue(NEXT_RUN_ID);
        assert!(!handles.binding.is_current());
        answer_new(&peer, NEW_SESSION, None);
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        finish_prompt(&peer, NEW_SESSION);
        envelopes_until_drained(&handles, NEXT_RUN_ID);
        assert!(handles.binding.is_current());
    }

    #[test]
    fn a_failed_new_session_leaves_a_notice_and_closes() {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer) = remote();
        handles.rebind_queue(NEXT_RUN_ID);
        let binding = Arc::clone(&handles.binding);
        let new = peer.recv();
        peer.send(json!({"jsonrpc": "2.0", "id": new["id"],
            "error": {"code": -32603, "message": "boom"}}));
        let events = events_until_end(handles);
        assert!(
            !binding.is_current(),
            "the old session must stay out of reach"
        );
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::Notice { text } if text.starts_with(NEW_SESSION_FAILED))
        ));
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    }

    fn only_model_a() -> Value {
        json!([{
            "id": MODEL_CONFIG, "name": "Model", "type": "select", "category": "model",
            "currentValue": MODEL_A,
            "options": [{"value": MODEL_A, "name": MODEL_A}],
        }])
    }

    /// The agent opens the new session on its own default, so the model the
    /// tab was on is asked for again before the next prompt.
    #[test]
    fn the_confirmed_model_carries_across_new() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_B);
        let (mut handles, peer) = remote_full(false, None, Some(models.clone()));
        handles.rebind_queue(NEXT_RUN_ID);
        answer_new(&peer, NEW_SESSION, Some(model_options(MODEL_A)));
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        let switch = peer.recv();
        assert_eq!(switch["method"], "session/set_config_option");
        assert_eq!(switch["params"]["value"], MODEL_B);
    }

    #[test]
    fn an_interrupted_pick_carries_across_new() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (mut handles, peer) = remote_full(false, None, Some(models.clone()));
        want(&models, MODEL_B);
        push_prompt(&handles);
        assert_eq!(peer.recv()["method"], "session/set_config_option");
        handles.rebind_queue(NEXT_RUN_ID);
        answer_new(&peer, NEW_SESSION, Some(model_options(MODEL_A)));
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        let switch = peer.recv();
        assert_eq!(switch["method"], "session/set_config_option");
        assert_eq!(switch["params"]["value"], MODEL_B);
    }

    #[test]
    fn a_refused_pick_is_not_retried_after_new() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_A);
        let (mut handles, peer) = remote_full(false, None, Some(models.clone()));
        want(&models, MODEL_B);
        push_prompt(&handles);
        let switch = peer.recv();
        peer.send(json!({"jsonrpc": "2.0", "id": switch["id"],
            "error": {"code": -32602, "message": "not allowed"}}));
        run_events(&handles);
        handles.rebind_queue(NEXT_RUN_ID);
        answer_new(&peer, NEW_SESSION, Some(model_options(MODEL_A)));
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        finish_prompt(&peer, NEW_SESSION);
    }

    #[test]
    fn a_model_the_new_menu_lacks_is_not_carried() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_B);
        let (mut handles, peer) = remote_full(false, None, Some(models.clone()));
        handles.rebind_queue(NEXT_RUN_ID);
        answer_new(&peer, NEW_SESSION, Some(only_model_a()));
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        finish_prompt(&peer, NEW_SESSION);
    }

    #[test]
    fn a_pick_made_during_the_replacement_is_kept() {
        let _runtime = crate::test_runtime().enter();
        let models = models(MODEL_B);
        let (mut handles, peer) = remote_full(false, None, Some(models.clone()));
        handles.rebind_queue(NEXT_RUN_ID);
        let new = peer.recv();
        want(&models, MODEL_A);
        peer.respond(
            &new,
            new_session_answer(NEW_SESSION, Some(model_options(MODEL_A))),
        );
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        finish_prompt(&peer, NEW_SESSION);
    }

    /// The switch answer and the rebind can be ready together. Whichever
    /// `settle` takes, the prompt must not go to the old session.
    #[test]
    fn a_rebind_ready_with_the_switch_answer_sends_no_old_prompt() {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer) = remote_with(true, Some(BUILD_MODE));
        push_prompt_in(&handles, plan());
        let switch = peer.recv();
        assert_eq!(switch["method"], "session/set_mode");
        handles.rebind_queue(NEXT_RUN_ID);
        peer.respond(&switch, json!({}));
        assert_eq!(peer.recv()["method"], "session/new");
    }

    /// The agent may hold its answer until the old session's ask is settled,
    /// so the ask is cancelled while the replacement waits.
    #[test_case(permission_request(), "/result/outcome/outcome", "cancelled" ; "permission")]
    #[test_case(question_request(), "/result/action", "cancel" ; "question")]
    fn an_ask_during_the_replacement_is_cancelled_at_once(
        ask: Value,
        outcome: &str,
        cancelled: &str,
    ) {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer) = remote();
        handles.rebind_queue(NEXT_RUN_ID);
        let new = peer.recv();
        assert_eq!(new["method"], "session/new");
        peer.send(ask);
        let reply = peer.recv();
        assert_eq!(
            reply.pointer(outcome).and_then(Value::as_str),
            Some(cancelled)
        );
        peer.respond(&new, new_session_answer(NEW_SESSION, None));
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        finish_prompt(&peer, NEW_SESSION);
    }

    const SPENT: f64 = 0.42;

    fn usage_update(cost: f64) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": SESSION,
            "update": {"sessionUpdate": "usage_update", "used": 10, "size": 100,
                "cost": {"amount": cost, "currency": "USD"}}
        }})
    }

    fn published_cost(handles: &AgentHandles) -> Option<f64> {
        handles
            .remote_usage
            .load_full()
            .and_then(|usage| usage.cost)
    }

    /// An error ends the run with no `Done`, but the cost it spent is
    /// session state and still reaches the App.
    #[test]
    fn usage_before_an_error_is_still_published() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        push_prompt(&handles);
        let prompt = peer.recv();
        peer.send(usage_update(SPENT));
        peer.send(json!({"jsonrpc": "2.0", "id": prompt["id"],
            "error": {"code": -32603, "message": "boom"}}));
        let events = run_events(&handles);
        assert!(matches!(events[0], AgentEvent::Error { .. }));
        assert_eq!(published_cost(&handles), Some(SPENT));
    }

    #[test]
    fn a_rebind_publishes_to_a_fresh_slot() {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer) = remote();
        let old = handles.remote_usage.clone();
        handles.rebind_queue(NEXT_RUN_ID);
        let new = peer.recv();
        peer.respond(&new, new_session_answer(NEW_SESSION, None));
        peer.send(
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": NEW_SESSION,
                "update": {"sessionUpdate": "usage_update", "used": 10, "size": 100,
                    "cost": {"amount": SPENT, "currency": "USD"}}
            }}),
        );
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        finish_prompt(&peer, NEW_SESSION);
        envelopes_until_drained(&handles, NEXT_RUN_ID);
        assert_eq!(published_cost(&handles), Some(SPENT));
        assert!(old.load().is_none(), "the old session's slot is left alone");
    }

    fn settings_options(fast: bool, yolo: bool) -> Value {
        json!([
            {"id": "thinking", "name": "Thinking", "type": "select", "category": "thought_level",
             "currentValue": "off", "options": [{"value": "off", "name": "off"}]},
            {"id": "fast", "name": "Fast", "type": "boolean", "currentValue": fast},
            {"id": "workflow", "name": "Workflow", "type": "boolean", "currentValue": false},
            {"id": "yolo", "name": "Yolo", "type": "boolean", "currentValue": yolo},
        ])
    }

    fn with_settings(fast: bool, yolo: bool) -> (AgentHandles, Peer, RemoteSettings) {
        let options: Vec<SessionConfigOption> =
            serde_json::from_value(settings_options(fast, yolo)).unwrap();
        let settings = RemoteSettings::from_options(&options);
        let extensions = Agreed {
            settings: true,
            ..Agreed::default()
        };
        let (handles, peer) = remote_settled(false, None, Some(settings.clone()), extensions);
        (handles, peer, settings)
    }

    fn answer_settings(peer: &Peer, request: &Value, fast: bool, yolo: bool) {
        peer.respond(
            request,
            json!({"configOptions": settings_options(fast, yolo)}),
        );
    }

    fn expect_setting(peer: &Peer, id: &str, value: bool) -> Value {
        let request = peer.recv();
        assert_eq!(request["method"], "session/set_config_option");
        assert_eq!(request["params"]["configId"], id);
        assert_eq!(request["params"]["value"], value);
        request
    }

    #[test]
    fn a_prompt_carries_its_settings() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer, _settings) = with_settings(false, false);
        handles.queue.push(QueueItem::Message(QueuedInput {
            text: PROMPT.into(),
            input: AgentInput {
                fast: true,
                workflow: true,
                ..input(PROMPT)
            },
            run_id: RUN_ID,
            displayed: true,
        }));
        let prompt = peer.recv();
        assert_eq!(prompt["method"], "session/prompt");
        let carried = &prompt["params"]["_meta"]["maki"]["settings"];
        assert_eq!(carried["fast"], true);
        assert_eq!(carried["workflow"], true);
    }

    #[test]
    fn a_toggle_syncs_when_idle_and_the_answer_publishes_the_menu() {
        let _runtime = crate::test_runtime().enter();
        let (_handles, peer, settings) = with_settings(false, false);
        settings.change(Setting::Fast, Value::Bool(true));
        let request = expect_setting(&peer, "fast", true);
        answer_settings(&peer, &request, true, false);
        settings.change(Setting::Yolo, Value::Bool(true));
        expect_setting(&peer, "yolo", true);
        assert_eq!(settings.menu.load().fast, Some(true));
    }

    /// The run goes on: a change during it is sent next to the prompt.
    #[test]
    fn a_toggle_during_a_run_syncs_without_ending_it() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer, settings) = with_settings(false, false);
        push_prompt(&handles);
        let prompt = peer.recv();
        settings.change(Setting::Yolo, Value::Bool(true));
        let request = expect_setting(&peer, "yolo", true);
        answer_settings(&peer, &request, false, true);
        peer.respond(&prompt, PromptResponse::new(StopReason::EndTurn));
        assert_eq!(
            done_reason(&run_events(&handles)),
            Some(DoneReason::EndTurn)
        );
    }

    /// One request at a time: the second goes once the first is answered.
    #[test]
    fn two_quick_toggles_are_sent_one_after_the_other() {
        let _runtime = crate::test_runtime().enter();
        let (_handles, peer, settings) = with_settings(false, false);
        settings.change(Setting::Fast, Value::Bool(true));
        settings.change(Setting::Yolo, Value::Bool(true));
        let first = expect_setting(&peer, "fast", true);
        answer_settings(&peer, &first, true, false);
        expect_setting(&peer, "yolo", true);
    }

    /// Yolo is policy for the prompt's tool calls: the prompt waits for it,
    /// and a refused yolo sends no prompt.
    #[test]
    fn a_prompt_waits_for_yolo_and_a_refusal_sends_none() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer, settings) = with_settings(false, false);
        settings.change(Setting::Yolo, Value::Bool(true));
        let request = expect_setting(&peer, "yolo", true);
        push_prompt(&handles);
        peer.send(json!({"jsonrpc": "2.0", "id": request["id"],
            "error": {"code": -32602, "message": "not allowed"}}));
        let events = run_events(&handles);
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::Error { message, .. } if message.starts_with(SETTING_NOT_SET))
        ));
        no_more_requests(&peer);
        assert!(
            !settings.intent.load().yolo,
            "the refused change is put back"
        );
        assert!(settings.failure.load().is_some());
    }

    fn refuse(peer: &Peer, request: &Value) {
        peer.send(json!({"jsonrpc": "2.0", "id": request["id"],
            "error": {"code": -32602, "message": "not allowed"}}));
    }

    /// Every refusal is handled as it comes, and a change of another setting
    /// does not keep it from being put back.
    #[test]
    fn a_refusal_is_put_back_after_an_unrelated_change() {
        let _runtime = crate::test_runtime().enter();
        let (_handles, peer, settings) = with_settings(false, false);
        settings.change(Setting::Fast, Value::Bool(true));
        let fast = expect_setting(&peer, "fast", true);
        settings.change(Setting::Workflow, Value::Bool(true));
        refuse(&peer, &fast);
        let workflow = expect_setting(&peer, "workflow", true);
        settings.change(Setting::Yolo, Value::Bool(true));
        refuse(&peer, &workflow);
        // Sent only once the workflow refusal was handled.
        expect_setting(&peer, "yolo", true);
        let intent = settings.intent.load();
        assert!(
            !intent.fast && !intent.workflow,
            "both refusals are put back"
        );
    }

    #[test]
    fn a_refusal_leaves_a_newer_change_of_the_same_setting() {
        let _runtime = crate::test_runtime().enter();
        let (_handles, peer, settings) = with_settings(false, false);
        settings.change(Setting::Fast, Value::Bool(true));
        let fast = expect_setting(&peer, "fast", true);
        settings.change(Setting::Fast, Value::Bool(false));
        settings.change(Setting::Fast, Value::Bool(true));
        refuse(&peer, &fast);
        assert!(settings.intent.load().fast, "the newer change wins");
        expect_setting(&peer, "fast", true);
    }

    /// An answer that does not hold the asked value, or cannot be read, is a
    /// refusal, so the same change is not sent forever.
    #[test_case(json!({"configOptions": settings_options(false, false)}) ; "kept_the_old_value")]
    #[test_case(json!("not a response") ; "unreadable")]
    fn an_unconfirmed_answer_is_a_refusal(answer: Value) {
        let _runtime = crate::test_runtime().enter();
        let (_handles, peer, settings) = with_settings(false, false);
        settings.change(Setting::Fast, Value::Bool(true));
        let request = expect_setting(&peer, "fast", true);
        peer.respond(&request, answer);
        settings.change(Setting::Yolo, Value::Bool(true));
        expect_setting(&peer, "yolo", true);
        assert!(!settings.intent.load().fast);
    }

    /// A change back to the agent's value ends the wait, even while another
    /// setting's answer is still out.
    #[test]
    fn the_yolo_wait_ends_when_the_change_is_undone() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer, settings) = with_settings(false, false);
        settings.change(Setting::Fast, Value::Bool(true));
        let _fast = expect_setting(&peer, "fast", true);
        settings.change(Setting::Yolo, Value::Bool(true));
        push_prompt(&handles);
        settings.change(Setting::Yolo, Value::Bool(false));
        assert_eq!(peer.recv()["method"], "session/prompt");
    }

    /// The waits after a model switch drain events, so the pick is checked
    /// again before the prompt.
    #[test]
    fn a_model_update_during_the_yolo_wait_stops_the_prompt() {
        let _runtime = crate::test_runtime().enter();
        let mut all = model_options(MODEL_A).as_array().unwrap().clone();
        all.extend(settings_options(false, false).as_array().unwrap().clone());
        let options: Vec<SessionConfigOption> = serde_json::from_value(json!(all)).unwrap();
        let settings = RemoteSettings::from_options(&options);
        let extensions = Agreed {
            settings: true,
            ..Agreed::default()
        };
        let (handles, peer) = remote_settled(false, None, Some(settings.clone()), extensions);
        want(&settings, MODEL_B);
        push_prompt(&handles);
        let switch = peer.recv();
        assert_eq!(switch["params"]["configId"], "model");
        settings.change(Setting::Yolo, Value::Bool(true));
        let yolo = expect_setting(&peer, "yolo", true);
        peer.respond(&switch, json!({"configOptions": model_options(MODEL_B)}));
        peer.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": SESSION,
            "update": {"sessionUpdate": "config_option_update", "configOptions": model_options(MODEL_A)}
        }}));
        answer_settings(&peer, &yolo, false, true);
        let events = run_events(&handles);
        assert!(
            events.iter().any(
                |e| matches!(e, AgentEvent::Error { message, .. } if message == MODEL_NOT_SET)
            )
        );
        no_more_requests(&peer);
    }

    /// The loop stopping sends what the agent has not heard yet, ahead of
    /// the client's end of input.
    #[test]
    fn teardown_sends_the_unsynced_settings() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer, settings) = with_settings(false, false);
        settings.change(Setting::Fast, Value::Bool(true));
        let _fast = expect_setting(&peer, "fast", true);
        settings.change(Setting::Yolo, Value::Bool(true));
        handles.cancel_all();
        // Fast is out with the value wanted, so only yolo is left to send.
        expect_setting(&peer, "yolo", true);
    }

    /// The new session starts on the agent's own yolo; the tab's intent is
    /// synced and the next prompt waits for it.
    #[test]
    fn after_new_a_prompt_waits_for_the_tabs_yolo() {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer, _settings) = with_settings(false, false);
        handles.rebind_queue(NEXT_RUN_ID);
        answer_new(&peer, NEW_SESSION, Some(settings_options(false, true)));
        let request = expect_setting(&peer, "yolo", false);
        push_run(&handles, AgentMode::Build, NEXT_RUN_ID);
        assert!(
            peer.from_client.try_recv().is_err(),
            "no prompt before the yolo answer"
        );
        answer_settings(&peer, &request, false, false);
        finish_prompt(&peer, NEW_SESSION);
    }

    /// The agent will hold the value out for it, so an undo of that request
    /// goes in the flush too.
    #[test]
    fn teardown_sends_the_undo_of_a_request_still_out() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer, settings) = with_settings(false, false);
        settings.change(Setting::Fast, Value::Bool(true));
        let _enable = expect_setting(&peer, "fast", true);
        settings.change(Setting::Fast, Value::Bool(false));
        handles.cancel_all();
        expect_setting(&peer, "fast", false);
    }

    /// A yolo refusal while the model switches still stops the prompt at the
    /// second wait, although the intent was put back meanwhile.
    #[test]
    fn a_yolo_refusal_during_the_model_switch_stops_the_prompt() {
        let _runtime = crate::test_runtime().enter();
        let mut all = model_options(MODEL_A).as_array().unwrap().clone();
        all.extend(settings_options(false, false).as_array().unwrap().clone());
        let options: Vec<SessionConfigOption> = serde_json::from_value(json!(all)).unwrap();
        let settings = RemoteSettings::from_options(&options);
        let extensions = Agreed {
            settings: true,
            ..Agreed::default()
        };
        let (handles, peer) = remote_settled(false, None, Some(settings.clone()), extensions);
        want(&settings, MODEL_B);
        push_prompt(&handles);
        let switch = peer.recv();
        settings.change(Setting::Yolo, Value::Bool(true));
        let yolo = expect_setting(&peer, "yolo", true);
        refuse(&peer, &yolo);
        peer.respond(&switch, json!({"configOptions": model_options(MODEL_B)}));
        let events = run_events(&handles);
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::Error { message, .. } if message.starts_with(SETTING_NOT_SET))
        ));
        no_more_requests(&peer);
    }

    /// A refused yolo the user already undid is not the prompt's policy, so
    /// the prompt goes on the agent's value.
    #[test]
    fn a_refusal_of_a_superseded_yolo_lets_the_prompt_go() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer, settings) = with_settings(false, false);
        settings.change(Setting::Yolo, Value::Bool(true));
        let yolo = expect_setting(&peer, "yolo", true);
        settings.change(Setting::Yolo, Value::Bool(false));
        push_prompt(&handles);
        refuse(&peer, &yolo);
        assert_eq!(peer.recv()["method"], "session/prompt");
    }
}
