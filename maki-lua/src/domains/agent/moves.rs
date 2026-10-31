use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::tools::MovedCall;
use maki_agent::{AgentSeed, CallScope, SessionMailbox, TaskHome, receipt_tag};
use maki_storage::id::MakiId;
use maki_storage::now_epoch;
use mlua::{Function, Lua, MultiValue, RegistryKey, Value as LuaValue};
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{timeout, timeout_at};

use super::tools::compute_summary;
use crate::api::agent::adopt_session;
use crate::api::tool::ToolCallReply;
use crate::api::util::ctx::DeadlineExpire;
use crate::background::{
    BackgroundInfo, BackgroundTask, BackgroundTasks, Fire, Kind, Lifetime, MovedEnd, Stop, StopMark,
};
use crate::plugin_permissions::{Permission, denied_error};
use crate::runtime::{
    CANCELLED_MSG, DrainOwner, GateGuard, ScopedFuture, TaskExt, TaskHandle, covered, lock_cell,
    strip_traceback,
};

pub(crate) const MAX_BACKGROUND_TASKS: usize = 16;
/// How long a Ctrl+B or a background deadline waits for a call's
/// `ctx:on_background` hook to answer before the call stays put.
const BACKGROUND_HOOK_TIMEOUT: Duration = Duration::from_secs(5);
const ALREADY_MOVED_MSG: &str = "already moved";
const NO_LIVE_CHAT_MSG: &str = "no live chat";
const BACKGROUND_OFF_MSG: &str = "background tasks are turned off here (--background-timeout 0)";
const NO_RUN_MSG: &str = "no agent run to report to";
/// A subagent waits for its moved calls before it answers, so a call that
/// moved itself (a dev server, say) would hang it. The user's Ctrl+B is the
/// only way one moves.
pub(crate) const SUBAGENT_SELF_MOVE_MSG: &str =
    "background: a subagent's tool call only moves when the user moves it";
const MOVED_BY_USER: &str = "Moved to the background by the user";
const MOVED_AS_ASKED: &str = "Started in the background as asked";
const NOT_LIVE_CALL_MSG: &str = "background: not a running model call";
const MOVED_AT_TIMEOUT_PREFIX: &str = "Hit its";
const MOVED_AT_TIMEOUT_SUFFIX: &str = "s timeout and was moved to the background";
const CAP_REACHED_MSG: &str = "background tasks already running";
const HOOK_SILENT_MSG: &str = "on_background hook did not answer within";
const MOVE_FAILED_MSG: &str = "could not move";
const MOVE_DECLINED_MSG: &str = "declined to move";
const RECEIPT_TAIL: &str = "Its result arrives as a message. Do not poll or sleep for it: \
     continue other work or end your turn. Stop it with task_stop if no longer needed.";
/// Only the session's own agent has `task_stop`: a subagent's calls move only
/// when the user moves them, which is too rare to pay for the tool in every
/// subagent request.
const SUBAGENT_RECEIPT_TAIL: &str = "Its result arrives as a message before you finish. Do not \
     poll or sleep for it: continue other work or end your turn.";

pub use maki_types::MoveOutcome;

#[derive(Default)]
pub(crate) struct AgentTask {
    pub call: Option<Arc<CallScope>>,
    pub background_hook: Option<RegistryKey>,
    pub deadline_expire: Cell<DeadlineExpire>,
}

impl AgentTask {
    fn moved(&self) -> bool {
        self.call.as_ref().is_some_and(|call| call.home().is_some())
    }
}

struct CallOwner(Arc<CallScope>);

impl DrainOwner for CallOwner {
    fn released(&self) -> bool {
        self.0.home().is_some()
    }
    fn changed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self.0.moved_listener())
    }
}

impl TaskExt for AgentTask {
    fn inherit(&self, _inline: bool) -> Option<Box<dyn TaskExt>> {
        Some(Box::new(Self {
            call: self.call.clone(),
            ..Self::default()
        }))
    }
    fn holds_deadline(&self) -> bool {
        self.moved() || self.deadline_expire.get() == DeadlineExpire::Background
    }
    fn cancel_reason(&self, lua: &Lua) -> Option<&'static str> {
        self.call.as_ref().and_then(|call| {
            (BackgroundTasks::of(lua).stopped_by(call.id()) == Stop::Model).then_some("stopped")
        })
    }
    fn end(&mut self) {
        self.background_hook.take();
    }
    fn owner_cancel(&self) -> Option<maki_agent::cancel::CancelToken> {
        self.call.as_ref().map(|call| call.owner().clone())
    }
    fn drain_owner(&self) -> Option<Arc<dyn DrainOwner>> {
        self.call
            .clone()
            .map(|call| Arc::new(CallOwner(call)) as Arc<dyn DrainOwner>)
    }
}

pub fn drain_owner(call: Option<Arc<CallScope>>) -> Option<Arc<dyn DrainOwner>> {
    call.map(|call| Arc::new(CallOwner(call)) as Arc<dyn DrainOwner>)
}

type LiveTasks = Rc<RefCell<HashMap<String, Rc<LiveTask>>>>;
thread_local! {
    static LIVE_TASKS: LiveTasks = Rc::new(RefCell::new(HashMap::new()));
}

pub fn start(lua: &Lua) {
    let (tx, requests) = flume::unbounded();
    lua.set_app_data(Moves(tx));
    let tasks = LIVE_TASKS.with(Rc::clone);
    tokio::task::spawn_local(serve_moves(lua.clone(), tasks, requests));
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn register(
    id: String,
    handle: TaskHandle,
    plugin: Arc<str>,
    tool: Arc<str>,
    input: Value,
    chat: Option<MakiId>,
    mailbox: Option<SessionMailbox>,
    subagent: bool,
    can_move: bool,
    answer: Rc<CallAnswer>,
) -> Rc<LiveTask> {
    let task = Rc::new(LiveTask {
        id,
        handle,
        plugin,
        tool,
        input,
        chat,
        mailbox,
        subagent,
        can_move,
        answer,
        move_lock: AsyncMutex::new(()),
    });
    LIVE_TASKS.with(|tasks| tasks.borrow_mut().insert(task.id.clone(), Rc::clone(&task)));
    task
}

pub(crate) fn remove(id: &str) {
    LIVE_TASKS.with(|tasks| tasks.borrow_mut().remove(id));
}

/// What put a call in the background, worded into its receipt.
enum MoveTrigger {
    User,
    Timeout(u64),
    /// The call moved itself with `ctx:background()`.
    Model,
}

impl MoveTrigger {
    /// Says how the call moved and that the result comes later. Without the
    /// tail, models poll or sleep for the result anyway.
    fn receipt_suffix(self, id: &str, subagent: bool) -> String {
        let how = match self {
            Self::User => Cow::Borrowed(MOVED_BY_USER),
            Self::Timeout(secs) => Cow::Owned(format!(
                "{MOVED_AT_TIMEOUT_PREFIX} {secs} {MOVED_AT_TIMEOUT_SUFFIX}"
            )),
            Self::Model => Cow::Borrowed(MOVED_AS_ASKED),
        };
        let tail = if subagent {
            SUBAGENT_RECEIPT_TAIL
        } else {
            RECEIPT_TAIL
        };
        format!("{how} {}. {tail}", receipt_tag(id))
    }
}

fn not_movable(reason: &str) -> MoveOutcome {
    MoveOutcome::NotMovable(reason.to_owned())
}

/// A model call's way back to whoever dispatched it. A move and the final
/// reply both take `reply`, and both run on the Lua thread, so whichever
/// comes first answers and the other finds it gone. A move leaves its tail's
/// sender behind, so the final reply of a moved call takes `reply` too.
pub(crate) struct CallAnswer {
    reply: RefCell<Option<flume::Sender<ToolCallReply>>>,
    /// The call's [`MAX_INFLIGHT_TOOLS`] slot. A move gives it back, so long
    /// background work never starves foreground calls.
    slot: RefCell<Option<GateGuard>>,
}

impl CallAnswer {
    pub(crate) fn new(reply: flume::Sender<ToolCallReply>, slot: Option<GateGuard>) -> Self {
        Self {
            reply: RefCell::new(Some(reply)),
            slot: RefCell::new(slot),
        }
    }

    fn open(&self) -> bool {
        self.reply
            .borrow()
            .as_ref()
            .is_some_and(|tx| !tx.is_disconnected())
    }

    /// The final reply goes to the dispatcher, or once the call moved, to
    /// the tail dispatch runs it through before it reaches the chat. A tail
    /// that went away took the call's [`MovedEnd`] with it, which abandoned
    /// the call.
    pub(crate) fn finish(&self, lua: &Lua, id: Option<&str>, reply: ToolCallReply) {
        drop(self.slot.take());
        if let Some(tx) = self.reply.take()
            && tx.send(reply).is_ok()
            && let Some(id) = id
        {
            BackgroundTasks::of(lua).final_ready(id);
        }
    }
}

/// A running model call the UI reaches by `tool_use_id`: clicks, and moving
/// it to the background. Lives until the call's final reply, moved or not.
pub(crate) struct LiveTask {
    id: String,
    handle: TaskHandle,
    plugin: Arc<str>,
    tool: Arc<str>,
    /// For the start summary that names the task in `/tasks`.
    input: Value,
    /// Where the moved call shows: task tabs, `/tasks`, the cap.
    chat: Option<MakiId>,
    /// The run the moved call reports to. `None` outside a run: never moves.
    mailbox: Option<SessionMailbox>,
    /// The call of a subagent, not of the agent owning the session.
    subagent: bool,
    can_move: bool,
    answer: Rc<CallAnswer>,
    /// A hook runs for a move, so the next request waits for its verdict.
    move_lock: AsyncMutex<()>,
}

impl LiveTask {
    async fn try_move(&self, lua: &Lua, trigger: MoveTrigger) -> MoveOutcome {
        if let Err(refusal) = self.may_move_by(&trigger) {
            return refusal;
        }
        let _moving = self.move_lock.lock().await;
        let (hook, call) = {
            let cell = lock_cell(&self.handle);
            let hook = cell
                .exts
                .get::<AgentTask>()
                .and_then(|task| task.background_hook.as_ref())
                .and_then(|key| lua.registry_value::<Function>(key).ok());
            (
                hook,
                cell.exts
                    .get::<AgentTask>()
                    .and_then(|task| task.call.clone()),
            )
        };
        let (Some(hook), Some(call)) = (hook, call) else {
            return MoveOutcome::NoHook;
        };
        if let Err(refusal) = self.movable_to(lua, &call) {
            return refusal;
        }
        // Named before the hook runs: nothing may yield between its receipt
        // and the commit, or its side effects could land without a move.
        let summary = compute_summary(lua, &self.plugin, &self.tool, &self.input)
            .unwrap_or_else(|| self.tool.to_string());
        if let Err(refusal) = self.movable_to(lua, &call) {
            return refusal;
        }
        let receipt = match self.ask(lua, hook).await {
            Ok(receipt) => receipt,
            Err(outcome) => return outcome,
        };
        self.commit(lua, &call, receipt, summary, trigger)
    }

    /// A press needs nothing. A call moving itself, by `ctx:background()` or
    /// an `expire = "background"` deadline, makes its result wake the chat
    /// and cost a turn, the price of `notify{ wake = true }`. A subagent waits
    /// for its moved calls, so it never moves one itself.
    fn may_move_by(&self, trigger: &MoveTrigger) -> Result<(), MoveOutcome> {
        if matches!(trigger, MoveTrigger::User) {
            return Ok(());
        }
        if self.subagent {
            return Err(not_movable(SUBAGENT_SELF_MOVE_MSG));
        }
        let granted = self.can_move;
        if !granted {
            return Err(MoveOutcome::NotMovable(
                denied_error(Permission::Agent).to_string(),
            ));
        }
        Ok(())
    }

    /// The chat the call can move to right now, and the mailbox it reports
    /// to. Checked before the hook runs, so its side effects (bash starting a
    /// log) only happen for a move that can land, and again at the commit,
    /// because the hook yields.
    fn movable_to(
        &self,
        lua: &Lua,
        call: &CallScope,
    ) -> Result<(MakiId, &SessionMailbox), MoveOutcome> {
        if call.home().is_some() {
            return Err(not_movable(ALREADY_MOVED_MSG));
        }
        // Esc already ended the call: moving it would only hide that.
        if call.cancel().is_cancelled() {
            return Err(not_movable(CANCELLED_MSG));
        }
        if !self.answer.open() {
            return Err(MoveOutcome::NoHook);
        }
        let Some(chat) = self.chat else {
            return Err(not_movable(NO_LIVE_CHAT_MSG));
        };
        if AgentSeed::live(chat).is_none() {
            // A session that is up but never published a seed runs with
            // background work off (`BackgroundMode::Blocking`).
            let reason = if SessionMailbox::open(chat).is_ok() {
                BACKGROUND_OFF_MSG
            } else {
                NO_LIVE_CHAT_MSG
            };
            return Err(not_movable(reason));
        }
        let Some(mailbox) = &self.mailbox else {
            return Err(not_movable(NO_RUN_MSG));
        };
        let running = BackgroundTasks::of(lua)
            .list(chat)
            .iter()
            .filter(|info| matches!(info.kind, Kind::Call { .. }))
            .count();
        if running >= MAX_BACKGROUND_TASKS {
            return Err(MoveOutcome::NotMovable(format!(
                "{MAX_BACKGROUND_TASKS} {CAP_REACHED_MSG}"
            )));
        }
        Ok((chat, mailbox))
    }

    /// Runs the hook on the call's own task, as a click does. `Err` is the
    /// outcome of a call that stays in the foreground.
    async fn ask(&self, lua: &Lua, hook: Function) -> Result<ToolCallReply, MoveOutcome> {
        let answered = covered(
            None,
            ScopedFuture::new(
                lua.clone(),
                Arc::clone(&self.handle),
                hook.call_async::<MultiValue>(()),
            ),
        );
        let mut values = timeout(BACKGROUND_HOOK_TIMEOUT, answered)
            .await
            .unwrap_or_else(|_| {
                Err(mlua::Error::runtime(format!(
                    "{HOOK_SILENT_MSG} {}s",
                    BACKGROUND_HOOK_TIMEOUT.as_secs()
                )))
            })
            .map_err(|e| {
                MoveOutcome::NotMovable(format!(
                    "{} {MOVE_FAILED_MSG}: {}",
                    self.tool,
                    strip_traceback(&e)
                ))
            })?
            .into_iter();
        match values.next().unwrap_or(LuaValue::Nil) {
            LuaValue::Nil => Err(MoveOutcome::NotMovable(match values.next() {
                Some(LuaValue::String(reason)) => reason.to_string_lossy(),
                _ => format!("{} {MOVE_DECLINED_MSG}", self.tool),
            })),
            receipt => {
                let reply = ToolCallReply::from_lua_value(lua, &receipt);
                match &reply.result {
                    Ok(_) => Ok(reply),
                    Err(e) => Err(MoveOutcome::NotMovable(e.clone())),
                }
            }
        }
    }

    fn commit(
        &self,
        lua: &Lua,
        call: &CallScope,
        mut receipt: ToolCallReply,
        summary: String,
        trigger: MoveTrigger,
    ) -> MoveOutcome {
        let (chat, mailbox) = match self.movable_to(lua, call) {
            Ok(target) => target,
            Err(refusal) => return refusal,
        };
        // Every step that can fail comes before the landing, so a move that
        // gives up leaves the call unmoved, and the trigger it claimed ends
        // the call as cancelled.
        let Ok((claim, cancel)) = call.claim_move() else {
            return not_movable(ALREADY_MOVED_MSG);
        };
        // An Esc that fired the run while the call was moving reaches it
        // through the claim. The user stopped it, so its end is quiet.
        let stop = StopMark::default();
        if call.cancel().is_cancelled() {
            stop.mark(Stop::User);
        }
        let entry = BackgroundTask {
            info: BackgroundInfo {
                id: self.id.clone(),
                plugin: Arc::clone(&self.plugin),
                name: summary,
                kind: Kind::Call {
                    tool: Arc::clone(&self.tool),
                },
                running: true,
                started_at: now_epoch(),
                cost: None,
            },
            session: chat,
            task: None,
            lifetime: Lifetime::Call,
            stop,
            cancel: Fire::Owned(cancel),
            mailbox: Some(mailbox.clone()),
        };
        let registry = BackgroundTasks::of(lua);
        // A refused entry fires the trigger it holds, so the call ends as
        // cancelled, answering the dispatcher it never left.
        if let Err(e) = registry.register(entry) {
            return MoveOutcome::NotMovable(e);
        }
        let (tail_tx, tail_rx) = flume::bounded(1);
        receipt.result = receipt.result.map(|text| {
            format!(
                "{text}\n\n{}",
                trigger.receipt_suffix(&self.id, self.subagent)
            )
        });
        receipt.moved = Some(MovedCall {
            result: Box::pin(async move {
                let reply: ToolCallReply = tail_rx.recv_async().await.ok()?;
                Some(reply.into_exec_result())
            }),
            land: {
                let end = MovedEnd::new(registry.clone(), self.id.clone());
                Box::new(move |done| end.land(done))
            },
        });
        let delivered = self
            .answer
            .reply
            .take()
            .is_some_and(|tx| tx.send(receipt).is_ok());
        // The dispatcher gave up on the call (Esc, its backstop) as the move
        // was landing. The receipt came back unread, and its `MovedEnd`
        // abandoned the entry as it dropped.
        if !delivered {
            return not_movable(CANCELLED_MSG);
        }
        self.answer.reply.replace(Some(tail_tx));
        let home = TaskHome {
            chat,
            id: self.id.clone(),
            plugin: Arc::clone(&self.plugin),
        };
        claim.land(home.clone());
        // Sessions opened from here on find the home and register themselves.
        for stub in call.take_sessions() {
            adopt_session(&registry, &home, stub);
        }
        drop(self.answer.slot.take());
        tracing::info!(
            id = %self.id,
            tool = %self.tool,
            plugin = %self.plugin,
            %chat,
            mailbox = %mailbox.session_id(),
            "tool call moved to the background"
        );
        MoveOutcome::Moved
    }
}

/// A press of `Ctrl+B` on a call, or the call's own `ctx:background()`.
pub(crate) struct MoveRequest {
    tool_use_id: String,
    trigger: MoveTrigger,
    reply: flume::Sender<MoveOutcome>,
}

impl MoveRequest {
    pub(crate) fn by_user(tool_use_id: String) -> (Self, flume::Receiver<MoveOutcome>) {
        Self::new(tool_use_id, MoveTrigger::User)
    }

    fn new(tool_use_id: String, trigger: MoveTrigger) -> (Self, flume::Receiver<MoveOutcome>) {
        let (reply, rx) = flume::bounded(1);
        let request = Self {
            tool_use_id,
            trigger,
            reply,
        };
        (request, rx)
    }
}

/// Where every move is asked for. [`serve_moves`] answers apart from the
/// request queue, which would wait behind a plugin load draining the very
/// call to move.
struct Moves(flume::Sender<MoveRequest>);

async fn serve_moves(lua: Lua, live_tasks: LiveTasks, requests: flume::Receiver<MoveRequest>) {
    while let Ok(request) = requests.recv_async().await {
        tokio::task::spawn_local(serve_move(lua.clone(), Rc::clone(&live_tasks), request));
    }
}

async fn serve_move(lua: Lua, live_tasks: LiveTasks, request: MoveRequest) {
    let MoveRequest {
        tool_use_id,
        trigger,
        reply,
    } = request;
    let task = live_tasks.borrow().get(&tool_use_id).map(Rc::clone);
    let outcome = match (task, trigger) {
        (None, MoveTrigger::User) => MoveOutcome::NoHook,
        (None, _) => not_movable(NOT_LIVE_CALL_MSG),
        (Some(task), trigger) => task.try_move(&lua, trigger).await,
    };
    let _ = reply.send(outcome);
}

/// `ctx:background()`, waiting on the call's own task while
/// [`serve_moves`] runs the hook.
pub(crate) async fn move_self(lua: &Lua, tool_use_id: &str) -> MoveOutcome {
    let Some(tx) = lua.app_data_ref::<Moves>().map(|own| own.0.clone()) else {
        return not_movable(NOT_LIVE_CALL_MSG);
    };
    let (request, rx) = MoveRequest::new(tool_use_id.to_owned(), MoveTrigger::Model);
    if tx.send(request).is_err() {
        return not_movable(NOT_LIVE_CALL_MSG);
    }
    rx.recv_async()
        .await
        .unwrap_or_else(|_| not_movable(NOT_LIVE_CALL_MSG))
}

/// Moves a call whose `expire = "background"` deadline passed. A call that
/// stays (not live, no live chat, no hook, a decline) gets its deadline back
/// as a plain one, already lapsed, so it times out as usual.
pub(crate) async fn expire_to_background(lua: &Lua, handle: &TaskHandle, task: Option<&LiveTask>) {
    let secs = loop {
        let (changed, due) = {
            let cell = lock_cell(handle);
            let held = cell
                .exts
                .get::<AgentTask>()
                .is_some_and(|task| task.deadline_expire.get() == DeadlineExpire::Background);
            let due = cell.deadline.get().filter(|_| held);
            let secs = cell.deadline_secs.get().unwrap_or(0);
            (cell.deadline_changed.listen(), due.map(|at| (at, secs)))
        };
        match due {
            Some((at, secs)) if at <= Instant::now() => break secs,
            Some((at, _)) => _ = timeout_at(at.into(), changed).await,
            None => changed.await,
        }
    };
    if let Some(task) = task {
        task.try_move(lua, MoveTrigger::Timeout(secs)).await;
    }
    let cell = lock_cell(handle);
    if let Some(task) = cell.exts.get::<AgentTask>()
        && !task.moved()
    {
        task.deadline_expire.set(DeadlineExpire::Timeout);
        cell.deadline_changed.notify(usize::MAX);
    }
}

pub(crate) struct MoveCall(pub String, pub flume::Sender<MoveOutcome>);
impl crate::domain::DomainRequest for MoveCall {
    fn serve<'a>(
        self: Box<Self>,
        vm: &'a crate::domain::Vm<'a>,
    ) -> futures::future::LocalBoxFuture<'a, ()> {
        let Self(tool_use_id, reply) = *self;
        Box::pin(async move {
            let (request, rx) = MoveRequest::by_user(tool_use_id);
            if let Some(tx) = vm
                .rt
                .lua
                .app_data_ref::<Moves>()
                .map(|moves| moves.0.clone())
            {
                let _ = tx.send(request);
                tokio::task::spawn_local(async move {
                    let _ = reply.send(rx.recv_async().await.unwrap_or(MoveOutcome::NoHook));
                });
            } else {
                let _ = reply.send(MoveOutcome::NoHook);
            }
        })
    }
}
