use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Instant;

use futures_lite::FutureExt;
use futures_lite::future::{or, pending, race, zip};
use maki_providers::{BackgroundToolResult, ContentBlock, DeliveryReceipt, Message, Role};
use maki_storage::id::MakiId;
use serde_json::{Value, json};

use super::CallInstructions;
use super::tool_dispatch;
use crate::cancel::{CancelMap, CancelSlot, CancelToken, CancelTrigger};
use crate::permissions::TaggedAnswer;
use crate::tools::ToolContext;
use crate::{
    AgentEvent, ChildPresentation, DETACHED_RUN_ID, Envelope, EventSender, Notice, ResultDelivery,
    SessionMailbox, SubagentInfo, SubagentVerdict, TaskKind, ToolDoneEvent, ToolOutput,
    ToolStartEvent, event_stream,
};

const CANCELLED: &str = "cancelled";
pub const BACKGROUND_STOP_REASON: &str = "background";
pub const MOVED_CALL_NO_RECEIPT: &str = "The user moved this call to the background, and it gave no receipt, so its result may never arrive. Do not wait for it. Continue with other work, or end your turn.";

/// Why an invocation stopped, read by the invocation itself after the
/// managed batch is gone. A background stop becomes a move only once the
/// invocation answers it with a receipt.
pub struct InvocationStop {
    cancel: CancelToken,
    background: AtomicBool,
    receipt: AtomicBool,
    detached: AtomicBool,
}

impl InvocationStop {
    pub fn is_background(&self) -> bool {
        self.background.load(Ordering::Acquire)
    }

    /// The call moved without stopping, so its foreground timeout no longer
    /// applies: it runs until it ends or the user stops its task.
    pub fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Acquire)
    }

    pub fn matches(&self, cancel: &CancelToken) -> bool {
        self.cancel.same_token(cancel)
    }
}

struct MoveSelection {
    remaining: AtomicUsize,
    committed: AtomicUsize,
    event_tx: EventSender,
    owner: CancelToken,
}

impl MoveSelection {
    fn settle(&self, committed: bool) {
        if committed {
            self.committed.fetch_add(1, Ordering::AcqRel);
        }
        if self.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.event_tx.try_send(AgentEvent::BackgroundMoveSettled {
                count: self.committed.load(Ordering::Acquire),
                owner_cancel: self.owner.clone(),
            });
        }
    }
}

/// A nested dispatch reads the same ownership decision as its invocation.
#[derive(Clone)]
pub struct RunningTool {
    children: Arc<Mutex<ChildPresentations>>,
    batch: Weak<ForegroundTools>,
    index: usize,
    id: Arc<str>,
    opted_in: bool,
    stop: Arc<InvocationStop>,
}

struct ChildPresentations {
    session: MakiId,
    detached: bool,
    children: Vec<Weak<ChildPresentation>>,
}

impl RunningTool {
    pub fn invocation_stop(&self, cancel: &CancelToken) -> Option<Arc<InvocationStop>> {
        self.stop.matches(cancel).then(|| Arc::clone(&self.stop))
    }

    pub fn matches_invocation(&self, id: Option<&str>, cancel: &CancelToken) -> bool {
        id == Some(&self.id) && self.stop.matches(cancel)
    }

    /// The invocation answered its background stop, so its reply is the
    /// receipt the model reads in place of the result.
    pub fn commit_receipt(&self) {
        self.stop.receipt.store(true, Ordering::Release);
    }

    pub fn set_execution_deadline(&self, at: Instant) {
        if let Some(batch) = self.batch.upgrade() {
            batch.set_execution_deadline(self.index, at);
        }
    }

    pub fn background_session(&self) -> Option<MakiId> {
        let children = self.children.lock().unwrap_or_else(PoisonError::into_inner);
        children.detached.then_some(children.session)
    }

    pub(crate) fn register_child(&self, child: &Arc<ChildPresentation>) {
        let mut children = self.children.lock().unwrap_or_else(PoisonError::into_inner);
        if children.detached {
            child.detach();
        }
        children.children.retain(|child| child.strong_count() > 0);
        children.children.push(Arc::downgrade(child));
    }

    fn detach_children(&self) {
        let mut children = self.children.lock().unwrap_or_else(PoisonError::into_inner);
        children.detached = true;
        for child in children.children.iter().filter_map(Weak::upgrade) {
            child.detach();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Phase {
    Attached,
    /// An opted-in call was stopped for the move and has not answered yet.
    Moving,
    DetachedGeneric,
    Finished,
}

struct PermissionAsk {
    original: String,
    published: String,
    tool: maki_config::ToolKey,
    scopes: Vec<String>,
    reason: Option<String>,
    reply_tx: flume::Sender<String>,
    info: Option<SubagentInfo>,
}

struct Call {
    id: String,
    name: String,
    input: Value,
    task_id: String,
    phase: Phase,
    running: RunningTool,
    execution_deadline: Option<Instant>,
    selection: Option<Arc<MoveSelection>>,
    trigger: Option<CancelTrigger>,
    cancel: CancelToken,
    slot: Option<CancelSlot>,
    result_tx: flume::Sender<ManagedResult>,
    answer_tx: Option<flume::Sender<String>>,
    private_answer_tx: Option<flume::Sender<String>>,
    delivery: ResultDelivery,
    start: Option<ToolStartEvent>,
    output: Option<String>,
    live: Option<Arc<crate::SharedBuf>>,
    permissions: Vec<PermissionAsk>,
}

pub(super) struct ManagedResult {
    pub done: ToolDoneEvent,
    pub promoted: bool,
    pub delivery: Option<Arc<DeliveryReceipt>>,
}

/// The mailbox holds this weakly. Execution workers own it until they settle,
/// and dropping the foreground waiter cancels only still-attached calls.
pub(crate) struct ForegroundTools {
    calls: Mutex<Vec<Call>>,
    mailbox: SessionMailbox,
    owner: CancelToken,
    run_cancel: CancelToken,
    event_tx: EventSender,
    cancels: Arc<CancelMap<String>>,
}

struct ForegroundWait {
    batch: Arc<ForegroundTools>,
    /// Fires after `drop` ran, and stops each call still attached.
    _waiter: CancelTrigger,
}

impl Drop for ForegroundWait {
    fn drop(&mut self) {
        for call in self.batch.calls().iter_mut() {
            if matches!(call.phase, Phase::Attached | Phase::Moving) {
                ForegroundTools::settle(call, false);
                call.trigger.take();
            }
        }
    }
}

impl ForegroundTools {
    fn calls(&self) -> MutexGuard<'_, Vec<Call>> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn run_id(&self) -> u64 {
        self.event_tx.run_id()
    }

    pub(crate) fn owned_by(&self, owner: &CancelToken) -> bool {
        !owner.is_cancelled() && self.owner.same_token(owner)
    }

    fn info(&self, call: &Call) -> SubagentInfo {
        SubagentInfo {
            kind: TaskKind::Tool,
            parent_tool_use_id: call.task_id.clone(),
            name: call.name.clone(),
            prompt: Some(call.input.to_string()),
            model: None,
            detached: true,
            owner_cancel: Some(self.owner.clone()),
            spawn_tool_use_id: Some(call.id.clone()),
            opts: None,
            answer_tx: call.answer_tx.clone(),
            permission_tool_use_id: None,
            inbox: None,
        }
    }

    fn send_detached(&self, call: &Call, event: AgentEvent) {
        let _ = self.event_tx.send_envelope(Envelope {
            event,
            subagent: Some(self.info(call)),
            run_id: DETACHED_RUN_ID,
        });
    }

    fn permission_event(ask: &PermissionAsk) -> AgentEvent {
        AgentEvent::PermissionRequest {
            id: ask.published.clone(),
            tool: ask.tool.clone(),
            scopes: ask.scopes.clone(),
            reason: ask.reason.clone(),
        }
    }

    fn settle(call: &mut Call, committed: bool) {
        if let Some(selection) = call.selection.take() {
            selection.settle(committed);
        }
    }

    fn set_execution_deadline(&self, index: usize, at: Instant) {
        let mut calls = self.calls();
        let call = &mut calls[index];
        if call.phase == Phase::Attached {
            call.execution_deadline = Some(call.execution_deadline.map_or(at, |old| old.min(at)));
        }
    }

    pub(crate) fn promote(self: &Arc<Self>) -> usize {
        let now = Instant::now();
        let mut calls = self.calls();
        if self.run_cancel.is_cancelled() || !self.mailbox.owns_generation(&self.owner) {
            return 0;
        }
        let unfinished: Vec<_> = calls
            .iter()
            .enumerate()
            .filter(|(_, call)| {
                call.phase == Phase::Attached
                    && !call.cancel.is_cancelled()
                    && call.execution_deadline.is_none_or(|at| now < at)
            })
            .map(|(index, _)| index)
            .collect();
        let count = unfinished.len();
        let generic = unfinished
            .iter()
            .filter(|&&index| !calls[index].running.opted_in)
            .count();
        if count == 0 || !self.mailbox.reserve_tools(generic) {
            return 0;
        }
        let selection = Arc::new(MoveSelection {
            remaining: AtomicUsize::new(count),
            committed: AtomicUsize::new(0),
            event_tx: self.event_tx.clone(),
            owner: self.owner.clone(),
        });
        // Fired once the lock is released: a stop wakes the invocation, and
        // its answer comes back through `finish`.
        let mut stops = Vec::new();
        for index in unfinished {
            let call = &mut calls[index];
            call.selection = Some(Arc::clone(&selection));
            if call.running.opted_in {
                call.phase = Phase::Moving;
                call.running.stop.background.store(true, Ordering::Release);
                stops.extend(call.trigger.take());
                continue;
            }
            call.phase = Phase::DetachedGeneric;
            call.running.stop.detached.store(true, Ordering::Release);
            call.running.detach_children();
            call.slot = call
                .trigger
                .take()
                .map(|trigger| self.cancels.insert_detached(call.task_id.clone(), trigger));
            let start = call.start.clone().unwrap_or_else(|| ToolStartEvent {
                id: call.id.clone(),
                tool: Arc::from(call.name.as_str()),
                summary: call.name.clone(),
                render_header: None,
                annotation: None,
                input: None,
                raw_input: Some(call.input.clone()),
                output: None,
            });
            self.send_detached(call, AgentEvent::ToolStart(Box::new(start)));
            if let Some(content) = &call.output {
                self.send_detached(
                    call,
                    AgentEvent::ToolOutput {
                        id: call.id.clone(),
                        content: content.clone(),
                    },
                );
            }
            if let Some(body) = &call.live {
                self.send_detached(
                    call,
                    AgentEvent::LiveToolBuf {
                        id: call.id.clone(),
                        body: Arc::clone(body),
                    },
                );
            }
            for ask in &call.permissions {
                let mut info = ask.info.clone().unwrap_or_else(|| self.info(call));
                info.detached = true;
                info.owner_cancel = Some(self.owner.clone());
                info.answer_tx = call.answer_tx.clone();
                let _ = self.event_tx.send_envelope(Envelope {
                    event: Self::permission_event(ask),
                    subagent: Some(info),
                    run_id: DETACHED_RUN_ID,
                });
            }
            let done = ToolDoneEvent {
                id: call.id.clone(),
                tool: Arc::from(call.name.as_str()),
                output: Arc::new(ToolOutput::Plain(
                    json!({
                        "task_id": call.task_id,
                        "status": "running",
                        "tool": call.name,
                        "notification": "The existing invocation continues in the background. Its final result will arrive as a completion notification.",
                    })
                    .to_string()
                    .into(),
                )),
                is_error: false,
                annotation: None,
                written_path: None,
                call: None,
            };
            self.event_tx
                .try_send(AgentEvent::ToolBackgrounded(Box::new(done.clone())));
            Self::settle(call, true);
            let _ = call.result_tx.try_send(ManagedResult {
                done,
                promoted: true,
                delivery: None,
            });
        }
        drop(calls);
        drop(stops);
        count
    }

    pub(crate) fn reply(self: &Arc<Self>, index: usize, request_id: &str, raw: &str) -> bool {
        TaggedAnswer::decode(raw).is_some_and(|answer| answer.request_id == request_id)
            && self.forward_answer(index, raw.to_owned())
    }

    pub(crate) fn permission(
        &self,
        index: usize,
        request_id: &str,
    ) -> Option<(String, flume::Sender<String>)> {
        let calls = self.calls();
        let call = &calls[index];
        if call.cancel.is_cancelled() || !self.mailbox.owns_generation(&self.owner) {
            return None;
        }
        let ask = call
            .permissions
            .iter()
            .find(|ask| ask.published == request_id)?;
        Some((ask.original.clone(), call.answer_tx.clone()?))
    }

    fn forward_answer(self: &Arc<Self>, index: usize, raw: String) -> bool {
        let mut calls = self.calls();
        let call = &mut calls[index];
        if call.cancel.is_cancelled() || !self.mailbox.owns_generation(&self.owner) {
            return false;
        }
        let (tx, answer, remove) = match TaggedAnswer::decode(&raw) {
            Some(answer) => {
                let Some(ask) = call
                    .permissions
                    .iter()
                    .find(|ask| ask.published == answer.request_id)
                else {
                    return false;
                };
                (
                    ask.reply_tx.clone(),
                    TaggedAnswer::new(&ask.original, answer.answer).encode(),
                    Some(ask.published.clone()),
                )
            }
            None => {
                let Some(tx) = &call.private_answer_tx else {
                    return false;
                };
                (tx.clone(), raw, None)
            }
        };
        if tx.try_send(answer).is_ok() {
            if let Some(published) = remove {
                call.permissions.retain(|ask| ask.published != published);
                self.mailbox.remove_tool_reply(&published);
            }
            true
        } else {
            false
        }
    }

    fn relay(self: &Arc<Self>, index: usize, mut envelope: Envelope) {
        let mut calls = self.calls();
        let call = &mut calls[index];
        if call.phase == Phase::Finished {
            return;
        }
        match &mut envelope.event {
            AgentEvent::ToolStart(start) if start.id == call.id => {
                call.start = Some((**start).clone())
            }
            AgentEvent::ToolOutput { id, content } if *id == call.id => {
                call.output = Some(content.clone())
            }
            AgentEvent::LiveToolBuf { id, body } if *id == call.id => {
                call.live = Some(Arc::clone(body))
            }
            AgentEvent::PermissionRequest {
                id,
                tool,
                scopes,
                reason,
            } => {
                let nested = envelope.subagent.as_ref();
                let published = format!("{}:{}", call.task_id, MakiId::generate());
                let reply_tx = nested
                    .and_then(|info| info.answer_tx.clone())
                    .or_else(|| call.private_answer_tx.clone());
                if let Some(reply_tx) = reply_tx {
                    call.permissions.push(PermissionAsk {
                        original: std::mem::replace(id, published.clone()),
                        published: published.clone(),
                        tool: tool.clone(),
                        scopes: scopes.clone(),
                        reason: reason.clone(),
                        reply_tx,
                        info: envelope.subagent.clone(),
                    });
                    self.mailbox.route_tool_reply(published, self, index);
                    if let Some(info) = &mut envelope.subagent {
                        info.answer_tx = call.answer_tx.clone();
                    }
                }
            }
            _ => {}
        }
        if call.phase == Phase::DetachedGeneric {
            envelope.run_id = DETACHED_RUN_ID;
            if let Some(info) = &mut envelope.subagent {
                info.detached = true;
                info.owner_cancel = Some(self.owner.clone());
            } else {
                envelope.subagent = Some(self.info(call));
            }
        }
        if !self.owner.is_cancelled() {
            let _ = self.event_tx.send_envelope(envelope);
        }
    }

    fn is_attached(&self, index: usize) -> bool {
        matches!(self.calls()[index].phase, Phase::Attached | Phase::Moving)
    }

    fn cancel_attached(&self, index: usize) {
        let mut calls = self.calls();
        let call = &mut calls[index];
        if matches!(call.phase, Phase::Attached | Phase::Moving) {
            Self::settle(call, false);
            call.trigger.take();
        }
    }

    fn finish(self: &Arc<Self>, index: usize, done: ToolDoneEvent) {
        let mut calls = self.calls();
        let call = &mut calls[index];
        if call.phase == Phase::Finished {
            return;
        }
        let detached = call.phase == Phase::DetachedGeneric;
        // A result that won the race against the stop completes normally.
        let moved =
            call.phase == Phase::Moving && call.running.stop.receipt.load(Ordering::Acquire);
        call.phase = Phase::Finished;
        Self::settle(call, moved);
        // A call that ended on its own leaves its pending work, like a
        // render queued with `maki.async.run`, to finish. Only a call still in
        // the run holds its trigger here, and that work ends with the run.
        if let Some(slot) = call.slot.take() {
            self.cancels.release(&call.task_id, slot);
        }
        if let Some(trigger) = call.trigger.take() {
            self.cancels.insert(call.task_id.clone(), trigger);
        }
        self.mailbox.retire_tool_replies(self, index);
        let receipt = self
            .mailbox
            .delivery_receipt(call.delivery.clone(), self.owner.clone());
        if detached {
            self.mailbox.release_tool();
            self.send_detached(call, AgentEvent::ToolDone(Box::new(done.clone())));
            let messages = vec![
                Message::user(call.input.to_string()),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::tool_use(
                        &call.id,
                        &call.name,
                        call.input.clone(),
                    )],
                    ..Default::default()
                },
                crate::types::tool_results(vec![done.clone()]),
            ];
            self.send_detached(
                call,
                AgentEvent::SubagentHistory {
                    tool_use_id: call.task_id.clone(),
                    messages,
                    status: Some(if done.is_error {
                        SubagentVerdict::Error
                    } else {
                        SubagentVerdict::Done
                    }),
                },
            );
            let mut message = Message::observation(format!(
                "Background task {} (tool {}) {}.\n{}",
                call.task_id,
                call.name,
                if done.is_error { "failed" } else { "finished" },
                done.output.as_text(),
            ));
            if let ToolOutput::Image { source, .. } = done.output.as_ref() {
                message.content.push(ContentBlock::Image {
                    source: source.clone(),
                });
            }
            message.background_tool = Some(BackgroundToolResult {
                task_id: call.task_id.clone(),
                tool_use_id: call.id.clone(),
                is_error: done.is_error,
                loaded_tools: done.output.loaded_tools().to_vec(),
            });
            message.delivery = Some(receipt);
            message.display_text =
                Some(format!("Background tool {}: {}", outcome(&done), call.name));
            if self.mailbox.owns_generation(&self.owner) {
                let _ = SessionMailbox::notify(
                    self.mailbox.session_id(),
                    Notice {
                        message,
                        owner_cancel: Some(self.owner.clone()),
                    },
                    true,
                );
            }
        } else {
            let event = if moved {
                call.running.detach_children();
                AgentEvent::ToolBackgrounded(Box::new(done.clone()))
            } else {
                AgentEvent::ToolDone(Box::new(done.clone()))
            };
            self.event_tx.try_send(event);
            let _ = call.result_tx.try_send(ManagedResult {
                done,
                promoted: moved,
                delivery: Some(receipt),
            });
        }
        call.answer_tx = None;
        call.private_answer_tx = None;
        call.start = None;
        call.output = None;
        call.live = None;
        call.permissions.clear();
    }
}

fn outcome(done: &ToolDoneEvent) -> &'static str {
    match (done.is_error, done.output.as_ref()) {
        (false, _) => "finished",
        (true, ToolOutput::Plain(output)) if output.text == CANCELLED => CANCELLED,
        (true, _) => "failed",
    }
}

/// Returns `None` for a context without a current main-session generation.
pub(super) async fn execute(
    runnable: &[(String, String, Value)],
    ctx: &ToolContext,
) -> Option<Vec<ManagedResult>> {
    if ctx.task_id.is_some() || runnable.is_empty() {
        return None;
    }
    let owner = ctx.event_tx.owner_cancel()?;
    let mailbox = SessionMailbox::current(ctx.session_id.as_ref()?.id(), &owner)?;
    let batch = Arc::new(ForegroundTools {
        calls: Mutex::new(Vec::new()),
        mailbox,
        owner: owner.clone(),
        run_cancel: ctx.cancel.clone(),
        event_tx: ctx.event_tx.clone(),
        cancels: Arc::clone(&ctx.subagent_cancels),
    });
    let mut receivers = Vec::with_capacity(runnable.len());
    let mut workers = Vec::with_capacity(runnable.len());
    for (index, (id, name, input)) in runnable.iter().enumerate() {
        let route = tool_dispatch::resolve_owned(ctx, name);
        let (trigger, cancel) = owner.child();
        let (result_tx, result_rx) = flume::bounded(1);
        let (answer_tx, answer_rx) = flume::unbounded();
        let (private_tx, private_rx) = flume::unbounded();
        let can_answer = ctx.user_response_rx.is_some();
        let delivery = ResultDelivery::default();
        let (stream, events) = event_stream();
        let running = RunningTool {
            batch: Arc::downgrade(&batch),
            index,
            id: Arc::from(id.as_str()),
            opted_in: route.background_receipt(),
            stop: Arc::new(InvocationStop {
                cancel: cancel.clone(),
                background: AtomicBool::new(false),
                receipt: AtomicBool::new(false),
                detached: AtomicBool::new(false),
            }),
            children: Arc::new(Mutex::new(ChildPresentations {
                session: batch.mailbox.session_id(),
                detached: false,
                children: Vec::new(),
            })),
        };
        let tool_ctx = ToolContext {
            tool_use_id: Some(id.clone()),
            call_instructions: CallInstructions::default(),
            cancel: cancel.clone(),
            event_tx: ctx.event_tx.tool_sender(&stream),
            user_response_rx: can_answer.then(|| Arc::new(smol::lock::Mutex::new(private_rx))),
            result_delivery: Some(delivery.clone()),
            running_tool: Some(running.clone()),
            ..ctx.clone()
        };
        batch.calls().push(Call {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            task_id: format!("tool-{}", MakiId::generate()),
            phase: Phase::Attached,
            running,
            execution_deadline: ctx.deadline.at(),
            selection: None,
            trigger: Some(trigger),
            cancel,
            slot: None,
            result_tx,
            answer_tx: can_answer.then_some(answer_tx),
            private_answer_tx: can_answer.then_some(private_tx),
            delivery,
            start: None,
            output: None,
            live: None,
            permissions: Vec::new(),
        });
        receivers.push(result_rx);
        workers.push((
            id.clone(),
            route,
            input.clone(),
            tool_ctx,
            stream,
            events,
            answer_rx,
        ));
    }
    batch.mailbox.publish_tools(&batch);
    let (waiter, waiter_gone) = CancelToken::new();
    let _wait = ForegroundWait {
        batch: Arc::clone(&batch),
        _waiter: waiter,
    };
    for (index, (id, route, input, tool_ctx, stream, mut events, answers)) in
        workers.into_iter().enumerate()
    {
        let batch = Arc::clone(&batch);
        let waiter_gone = waiter_gone.clone();
        smol::spawn(async move {
            let execution = async {
                let run = AssertUnwindSafe(route.run(id.clone(), &input, &tool_ctx)).catch_unwind();
                // A cancelled tool ends on its own, so a reply its cancel
                // path gives, like partial output, still reaches history.
                let execute = async {
                    run.await.unwrap_or_else(|panic| {
                        ToolDoneEvent::error(
                            id.clone(),
                            format!(
                                "internal error: tool panicked: {}",
                                crate::task_set::panic_to_string(panic)
                            ),
                        )
                    })
                };
                let cancel_foreground = async {
                    batch.run_cancel.cancelled().await;
                    batch.cancel_attached(index);
                    pending::<ToolDoneEvent>().await
                };
                // Nobody reads an attached call's result once its waiter is
                // gone, so the call stops where it is, as it would have
                // inside the run that dropped it. A moved call goes on.
                // Polled first, so a call the drop found waiting never runs.
                let abandoned = async {
                    waiter_gone.cancelled().await;
                    if !batch.is_attached(index) {
                        pending::<()>().await;
                    }
                    ToolDoneEvent::error(id.clone(), tool_dispatch::ERROR_CANCELLED.to_owned())
                };
                let done = or(abandoned, race(execute, cancel_foreground)).await;
                drop(stream);
                done
            };
            let forwarding = async {
                race(
                    async {
                        while let Some(envelope) = events.next().await {
                            batch.relay(index, envelope);
                        }
                    },
                    async {
                        while let Ok(raw) = answers.recv_async().await {
                            batch.forward_answer(index, raw);
                        }
                        pending::<()>().await
                    },
                )
                .await;
            };
            let (done, ()) = zip(execution, forwarding).await;
            batch.finish(index, done);
        })
        .detach();
    }
    let mut results = Vec::with_capacity(receivers.len());
    for receiver in receivers {
        if let Ok(result) = receiver.recv_async().await {
            results.push(result);
        }
    }
    Some(results)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_lite::future::or;
    use maki_config::{PermissionsConfig, ProjectConfig};
    use maki_providers::{ImageMediaType, ImageSource, StreamResponse};
    use maki_storage::id::SessionRef;
    use test_case::test_case;

    use super::*;
    use crate::History;
    use crate::live_session::BACKGROUND_PLAN_WRITE_DENIED;
    use crate::permissions::{PermissionAnswer, PermissionManager};
    use crate::tools::CallOrigin;
    use crate::tools::registry::{
        ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolInvocation, ToolSource,
    };
    use crate::tools::test_support::{
        GUARDED_TOOL_NAME, GuardedMock, stub_ctx, stub_seed_owned_by,
    };
    use crate::tools::{DescriptionContext, ToolAudience, local_tool};
    use crate::{AgentMode, TextOutput};

    const RUN: u64 = 41;
    const FIRST: &str = "compute";
    const SECOND: &str = "inspect";
    const THIRD: &str = "render";
    const FAILED: &str = "tool failed deliberately";
    const OUTPUT: &str = "finished output";
    const FIRST_FAILED: &str = "Background tool failed: compute";
    const FIRST_CANCELLED: &str = "Background tool cancelled: compute";
    const SECOND_FINISHED: &str = "Background tool finished: inspect";
    const THIRD_FINISHED: &str = "Background tool finished: render";
    const LIVE: &str = "live before promotion";
    const LOADED: &str = "server__discovered";
    const WRAPPER: &str = "wrapper";
    const REUSED_ID: &str = "reused";
    const INSTRUCTION_PATH: &str = "/repo/sub/AGENTS.md";
    const CHILD_PROMPT: &str = "inspect the queued child";
    const INSTRUCTION_TEXT: &str = "subdirectory rule";

    struct OnDrop(flume::Sender<()>);

    impl Drop for OnDrop {
        fn drop(&mut self) {
            let _ = self.0.try_send(());
        }
    }

    #[derive(Clone)]
    struct ControlledTool {
        name: &'static str,
        started: flume::Sender<ToolContext>,
        release: flume::Receiver<Result<ToolOutput, String>>,
        ended: flume::Sender<()>,
        count: Arc<AtomicUsize>,
        receipt: bool,
        path: Option<PathBuf>,
    }

    impl ToolInvocation for ControlledTool {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain(self.name.into()))
        }
        fn mutable_path(&self) -> Option<&Path> {
            self.path.as_deref()
        }
        fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
            Box::pin(async move {
                let _ended = OnDrop(self.ended.clone());
                self.count.fetch_add(1, Ordering::Relaxed);
                self.started.send(ctx.clone()).unwrap();
                let released = or(async { Some(self.release.recv_async().await) }, async {
                    ctx.cancel.cancelled().await;
                    None
                })
                .await;
                let moving = ctx
                    .running_tool
                    .as_ref()
                    .is_some_and(|running| running.stop.is_background());
                match released {
                    Some(reply) => reply.unwrap().into(),
                    None if moving => self.release.recv_async().await.unwrap().into(),
                    None => Err(CANCELLED.to_owned()).into(),
                }
            })
        }
    }

    impl Tool for ControlledTool {
        fn background_receipt(&self) -> bool {
            self.receipt
        }
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self, _: &DescriptionContext) -> Cow<'_, str> {
            self.name.into()
        }
        fn schema(&self) -> Value {
            json!({"type":"object"})
        }
        fn parse(&self, _: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(self.clone()))
        }
    }

    struct Control {
        started: flume::Receiver<ToolContext>,
        release: flume::Sender<Result<ToolOutput, String>>,
        ended: flume::Receiver<()>,
        count: Arc<AtomicUsize>,
    }

    fn controlled(ctx: &ToolContext, name: &'static str) -> Control {
        controlled_tool(ctx, name, false, None)
    }

    fn controlled_receipt(ctx: &ToolContext, name: &'static str, receipt: bool) -> Control {
        controlled_tool(ctx, name, receipt, None)
    }

    fn controlled_tool(
        ctx: &ToolContext,
        name: &'static str,
        receipt: bool,
        path: Option<PathBuf>,
    ) -> Control {
        let (started, starts) = flume::unbounded();
        let (release, releases) = flume::unbounded();
        let (ended, ends) = flume::unbounded();
        let count = Arc::new(AtomicUsize::new(0));
        ctx.registry
            .register(
                Arc::new(ControlledTool {
                    name,
                    receipt,
                    path,
                    started,
                    release: releases,
                    ended,
                    count: Arc::clone(&count),
                }),
                ToolSource::Lua {
                    plugin: "fixture".into(),
                },
            )
            .unwrap();
        Control {
            started: starts,
            release,
            ended: ends,
            count,
        }
    }

    struct Fixture {
        ctx: ToolContext,
        mailbox: SessionMailbox,
        events: flume::Receiver<Envelope>,
        generation: CancelTrigger,
        run: CancelTrigger,
    }

    fn fixture() -> Fixture {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let (generation, owner) = CancelToken::new();
        mailbox.publish_generation(owner.clone());
        let (run, cancel) = CancelToken::new();
        let (tx, events) = flume::unbounded();
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.event_tx = EventSender::new(tx, RUN).with_owner_cancel(owner);
        ctx.session_id = Some(SessionRef::from(id));
        ctx.cancel = cancel;
        ctx.user_response_rx = Some(Arc::new(smol::lock::Mutex::new(flume::unbounded().1)));
        Fixture {
            ctx,
            mailbox,
            events,
            generation,
            run,
        }
    }

    fn dispatch(ctx: ToolContext, calls: Vec<(&str, String)>) -> smol::Task<History> {
        let content = calls
            .into_iter()
            .enumerate()
            .map(|(index, (name, id))| ContentBlock::tool_use(id, name, json!({"index": index})))
            .collect();
        smol::spawn(async move {
            let mut history = History::new(Vec::new());
            tool_dispatch::process_tool_calls(
                StreamResponse {
                    message: Message {
                        role: Role::Assistant,
                        content,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                &mut tool_dispatch::RecentCalls::new(),
                &mut history,
                &ctx.event_tx,
                &ctx,
            )
            .await
            .unwrap();
            history
        })
    }

    async fn notices(mailbox: &SessionMailbox, count: usize) -> Vec<Notice> {
        let mut notices = Vec::new();
        while notices.len() < count {
            let wake = mailbox.wake_listener();
            notices.extend(mailbox.claim_wake());
            if notices.len() < count {
                wake.await;
            }
        }
        notices
    }

    fn task_id(history: &History, index: usize) -> String {
        let ContentBlock::ToolResult { content, .. } = &history.as_slice()[1].content[index] else {
            panic!("expected receipt");
        };
        serde_json::from_str::<Value>(content).unwrap()["task_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    const TEST_TOOL_CAPACITY: usize = 64;
    const RECEIPT: &str = "the call moved to the background";
    const PARTIAL: &str = "partial output before the stop";

    fn settled_counts(events: &flume::Receiver<Envelope>) -> Vec<usize> {
        events
            .drain()
            .filter_map(|event| match event.event {
                AgentEvent::BackgroundMoveSettled { count, .. } => Some(count),
                _ => None,
            })
            .collect()
    }

    fn answer_with_receipt(ctx: &ToolContext, control: &Control) {
        ctx.running_tool.as_ref().unwrap().commit_receipt();
        control
            .release
            .send(Ok(ToolOutput::Plain(RECEIPT.into())))
            .unwrap();
    }

    fn result_text(history: &History, index: usize) -> (&str, bool) {
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &history.as_slice()[1].content[index]
        else {
            panic!("expected a tool result");
        };
        (content, *is_error)
    }

    #[test_case(false; "free_generic_capacity")]
    #[test_case(true; "full_generic_capacity")]
    fn a_receipt_moves_the_call_without_generic_resources(full: bool) {
        smol::block_on(async {
            let fixture = fixture();
            let control = controlled_receipt(&fixture.ctx, FIRST, true);
            let foreground = dispatch(fixture.ctx.clone(), vec![(FIRST, FIRST.into())]);
            let ctx = control.started.recv_async().await.unwrap();
            let running = ctx.running_tool.as_ref().unwrap();
            if full {
                assert!(fixture.mailbox.reserve_tools(TEST_TOOL_CAPACITY));
            }
            assert_eq!(fixture.mailbox.background_tools(RUN), 1);
            assert_eq!(fixture.mailbox.background_tools(RUN), 0);
            assert!(ctx.cancel.is_cancelled());
            assert!(running.stop.is_background());
            assert!(settled_counts(&fixture.events).is_empty());
            answer_with_receipt(&ctx, &control);
            let history = foreground.await;
            assert_eq!(result_text(&history, 0), (RECEIPT, false));
            fixture.run.cancel();
            assert!(fixture.mailbox.claim_wake().is_empty());
            if !full {
                assert!(fixture.mailbox.reserve_tools(TEST_TOOL_CAPACITY));
            }
            let events: Vec<_> = fixture.events.drain().collect();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        &event.event,
                        AgentEvent::BackgroundMoveSettled { count: 1, .. }
                    ))
                    .count(),
                1
            );
            assert!(events.iter().any(|event| matches!(
                &event.event,
                AgentEvent::ToolBackgrounded(done) if done.output.as_text() == RECEIPT
            )));
            assert!(!events.iter().any(|event| matches!(
                &event.event,
                AgentEvent::ToolDone(_)
                    | AgentEvent::SubagentHistory { .. }
                    | AgentEvent::ToolResultsSubmitted { .. }
            )));
        });
    }

    #[test_case(Ok(ToolOutput::Plain(OUTPUT.into())), (OUTPUT, false); "result")]
    #[test_case(Err(FAILED.into()), (FAILED, true); "failure")]
    fn a_result_that_wins_the_race_completes_normally(
        reply: Result<ToolOutput, String>,
        expected: (&str, bool),
    ) {
        smol::block_on(async {
            let fixture = fixture();
            let control = controlled_receipt(&fixture.ctx, FIRST, true);
            let foreground = dispatch(fixture.ctx.clone(), vec![(FIRST, FIRST.into())]);
            control.started.recv_async().await.unwrap();
            assert_eq!(fixture.mailbox.background_tools(RUN), 1);
            control.release.send(reply).unwrap();
            let history = foreground.await;
            assert_eq!(result_text(&history, 0), expected);
            let events: Vec<_> = fixture.events.drain().collect();
            assert!(
                events
                    .iter()
                    .any(|event| matches!(&event.event, AgentEvent::ToolDone(_)))
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(&event.event, AgentEvent::ToolBackgrounded(_)))
            );
            assert!(events.iter().any(|event| matches!(
                &event.event,
                AgentEvent::BackgroundMoveSettled { count: 0, .. }
            )));
            assert!(fixture.mailbox.claim_wake().is_empty());
        });
    }

    #[test_case(false; "completion_first")]
    #[test_case(true; "cancel_first")]
    fn a_finished_or_cancelled_call_cannot_move(cancel: bool) {
        smol::block_on(async {
            let fixture = fixture();
            let control = controlled_receipt(&fixture.ctx, FIRST, true);
            let foreground = dispatch(fixture.ctx.clone(), vec![(FIRST, FIRST.into())]);
            let ctx = control.started.recv_async().await.unwrap();
            if cancel {
                fixture.run.cancel();
            } else {
                control
                    .release
                    .send(Ok(ToolOutput::Plain(OUTPUT.into())))
                    .unwrap();
                while !matches!(
                    fixture.events.recv_async().await.unwrap().event,
                    AgentEvent::ToolDone(_)
                ) {}
            }
            assert_eq!(fixture.mailbox.background_tools(RUN), 0);
            foreground.await;
            assert!(!ctx.running_tool.as_ref().unwrap().stop.is_background());
        });
    }

    #[test]
    fn generic_capacity_failure_leaves_every_call_attached() {
        smol::block_on(async {
            let fixture = fixture();
            let first = controlled_receipt(&fixture.ctx, FIRST, true);
            let second = controlled(&fixture.ctx, SECOND);
            let foreground = dispatch(
                fixture.ctx.clone(),
                vec![(FIRST, FIRST.into()), (SECOND, SECOND.into())],
            );
            let ctx = first.started.recv_async().await.unwrap();
            let other = second.started.recv_async().await.unwrap();
            assert!(fixture.mailbox.reserve_tools(TEST_TOOL_CAPACITY));
            assert_eq!(fixture.mailbox.background_tools(RUN), 0);
            assert!(!ctx.running_tool.as_ref().unwrap().stop.is_background());
            assert!(!ctx.cancel.is_cancelled());
            assert!(!other.cancel.is_cancelled());
            for control in [first, second] {
                control
                    .release
                    .send(Ok(ToolOutput::Plain(OUTPUT.into())))
                    .unwrap();
            }
            foreground.await;
        });
    }

    #[test_case(false; "child_before_the_receipt")]
    #[test_case(true; "child_after_the_receipt")]
    fn a_receipt_detaches_current_and_late_children(late: bool) {
        smol::block_on(async {
            let fixture = fixture();
            let control = controlled_receipt(&fixture.ctx, FIRST, true);
            let foreground = dispatch(fixture.ctx.clone(), vec![(FIRST, FIRST.into())]);
            let ctx = control.started.recv_async().await.unwrap();
            let running = ctx.running_tool.as_ref().unwrap();
            let batch = running.batch.upgrade().unwrap();
            let info = batch.info(&batch.calls()[running.index]);
            let make_child = || {
                ChildPresentation::new(
                    Some(fixture.mailbox.session_id()),
                    Some(running),
                    SubagentInfo {
                        detached: false,
                        ..info.clone()
                    },
                    ctx.event_tx.root_sender(),
                )
            };
            let early = (!late).then(make_child);
            assert_eq!(fixture.mailbox.background_tools(RUN), 1);
            if let Some(child) = &early {
                assert!(
                    !child.is_detached(),
                    "a pending stop leaves the child attached"
                );
            }
            answer_with_receipt(&ctx, &control);
            foreground.await;
            let child = early.unwrap_or_else(make_child);
            assert!(child.is_detached());
            assert!(running.background_session().is_some());
        });
    }

    #[test_case(FIRST; "first_tool")]
    #[test_case(SECOND; "second_tool")]
    fn owned_route_keeps_opt_in_and_execution_after_registry_replacement(name: &'static str) {
        smol::block_on(async {
            let fixture = fixture();
            let control = controlled_receipt(&fixture.ctx, name, true);
            let route = tool_dispatch::resolve_owned(&fixture.ctx, name);
            assert!(route.background_receipt());
            fixture.ctx.registry.clear_plugin("fixture");
            let ctx = fixture.ctx.clone();
            let execution =
                smol::spawn(async move { route.run(FIRST.into(), &json!({}), &ctx).await });
            control.started.recv_async().await.unwrap();
            control
                .release
                .send(Ok(ToolOutput::Plain(OUTPUT.into())))
                .unwrap();
            assert_eq!(execution.await.output.as_text(), OUTPUT);
        });
    }

    #[test_case(false, false; "single_press")]
    #[test_case(true, false; "repeated_press")]
    #[test_case(false, true; "failure_instead_of_a_receipt")]
    fn mixed_press_settles_once_after_every_stop_answers(repeat: bool, fail: bool) {
        smol::block_on(async {
            let fixture = fixture();
            let first = controlled_receipt(&fixture.ctx, FIRST, true);
            let second = controlled_receipt(&fixture.ctx, SECOND, true);
            let third = controlled(&fixture.ctx, THIRD);
            let foreground = dispatch(
                fixture.ctx.clone(),
                vec![
                    (FIRST, FIRST.into()),
                    (SECOND, SECOND.into()),
                    (THIRD, THIRD.into()),
                ],
            );
            let moved = first.started.recv_async().await.unwrap();
            second.started.recv_async().await.unwrap();
            third.started.recv_async().await.unwrap();
            assert_eq!(fixture.mailbox.background_tools(RUN), 3);
            if repeat {
                assert_eq!(fixture.mailbox.background_tools(RUN), 0);
            }
            answer_with_receipt(&moved, &first);
            assert!(settled_counts(&fixture.events).is_empty());
            let reply = if fail {
                Err(FAILED.into())
            } else {
                Ok(ToolOutput::Plain(OUTPUT.into()))
            };
            second.release.send(reply).unwrap();
            let history = foreground.await;
            assert_eq!(result_text(&history, 0), (RECEIPT, false));
            assert_eq!(result_text(&history, 1).1, fail);
            assert_eq!(settled_counts(&fixture.events), vec![2]);
            third
                .release
                .send(Ok(ToolOutput::Plain(OUTPUT.into())))
                .unwrap();
            notices(&fixture.mailbox, 1).await;
        });
    }

    #[test_case(false; "replaced_generation")]
    #[test_case(true; "cancelled_generation")]
    fn stale_generation_cannot_promote(cancel: bool) {
        smol::block_on(async {
            let fixture = fixture();
            let control = controlled_receipt(&fixture.ctx, FIRST, true);
            let foreground = dispatch(fixture.ctx.clone(), vec![(FIRST, FIRST.into())]);
            let ctx = control.started.recv_async().await.unwrap();
            let (_next_generation, next) = CancelToken::new();
            if cancel {
                fixture.generation.cancel();
            } else {
                fixture.mailbox.publish_generation(next);
            }
            assert_eq!(fixture.mailbox.background_tools(RUN), 0);
            assert!(!ctx.running_tool.as_ref().unwrap().stop.is_background());
            control
                .release
                .send(Ok(ToolOutput::Plain(OUTPUT.into())))
                .ok();
            foreground.await;
        });
    }

    #[test_case(false; "attached")]
    #[test_case(true; "moving")]
    fn dropping_the_foreground_waiter_cancels_unanswered_work(moving: bool) {
        smol::block_on(async {
            let fixture = fixture();
            let control = controlled_receipt(&fixture.ctx, FIRST, true);
            let foreground = dispatch(fixture.ctx.clone(), vec![(FIRST, FIRST.into())]);
            let ctx = control.started.recv_async().await.unwrap();
            if moving {
                assert_eq!(fixture.mailbox.background_tools(RUN), 1);
            }
            foreground.cancel().await;
            assert!(ctx.cancel.is_cancelled());
            assert_eq!(
                ctx.running_tool.as_ref().unwrap().stop.is_background(),
                moving
            );
            assert_eq!(
                settled_counts(&fixture.events),
                if moving { vec![0] } else { Vec::new() }
            );
        });
    }

    #[test]
    fn a_call_moved_while_it_waits_for_its_file_rechecks_the_write_gate() {
        smol::block_on(async {
            let fixture = fixture();
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join(FIRST);
            let control = controlled_tool(&fixture.ctx, FIRST, false, Some(path.clone()));
            let seed = fixture.mailbox.publish_seed(stub_seed_owned_by(
                fixture.mailbox.session_id(),
                fixture.ctx.event_tx.owner_cancel().unwrap(),
            ));
            let lock = fixture
                .ctx
                .file_access
                .acquire(&crate::tools::FileKey::new(&path))
                .await;
            let foreground = dispatch(fixture.ctx.clone(), vec![(FIRST, FIRST.into())]);
            while !matches!(
                fixture.events.recv_async().await.unwrap().event,
                AgentEvent::ToolStart(_)
            ) {}
            assert_eq!(fixture.mailbox.background_tools(RUN), 1);
            foreground.await;
            seed.set_chat_settings(Default::default(), &AgentMode::Plan(path.clone()));
            drop(lock);
            let notice = or(async { notices(&fixture.mailbox, 1).await.pop() }, async {
                control.started.recv_async().await.unwrap();
                None
            })
            .await
            .expect("the moved call wrote while its chat plans");
            assert!(
                notice
                    .message
                    .first_text_content()
                    .unwrap()
                    .contains(BACKGROUND_PLAN_WRITE_DENIED)
            );
        });
    }

    #[test]
    fn a_cancelled_call_keeps_the_reply_it_ends_with() {
        smol::block_on(async {
            let mut fixture = fixture();
            fixture.ctx.local_tools = Arc::new(HashMap::from([(
                WRAPPER.into(),
                local_tool(ToolAudience::all(), |_, ctx| {
                    Box::pin(async move {
                        ctx.cancel.cancelled().await;
                        smol::future::yield_now().await;
                        Ok(PARTIAL.into())
                    })
                }),
            )]));
            let foreground = dispatch(fixture.ctx.clone(), vec![(WRAPPER, WRAPPER.into())]);
            while !matches!(
                fixture.events.recv_async().await.unwrap().event,
                AgentEvent::ToolStart(_)
            ) {}
            fixture.run.cancel();
            let history = foreground.await;
            assert_eq!(result_text(&history, 0), (PARTIAL, false));
        });
    }

    #[test_case(false; "current_child")]
    #[test_case(true; "child_after_owner_retirement")]
    fn generic_promotion_marks_current_and_late_children(late: bool) {
        smol::block_on(async {
            let fixture = fixture();
            let control = controlled(&fixture.ctx, FIRST);
            let foreground = dispatch(fixture.ctx.clone(), vec![(FIRST, FIRST.into())]);
            let running = control.started.recv_async().await.unwrap();
            let (answer_tx, answer_rx) = flume::unbounded();
            let info = SubagentInfo {
                kind: TaskKind::Agent,
                parent_tool_use_id: SECOND.into(),
                name: SECOND.into(),
                prompt: Some(CHILD_PROMPT.into()),
                model: None,
                detached: false,
                owner_cancel: fixture.ctx.event_tx.owner_cancel(),
                spawn_tool_use_id: Some(FIRST.into()),
                opts: None,
                answer_tx: Some(answer_tx),
                permission_tool_use_id: None,
                inbox: None,
            };
            let make_child = || {
                ChildPresentation::new(
                    Some(fixture.mailbox.session_id()),
                    running.running_tool.as_ref(),
                    info.clone(),
                    running.event_tx.root_sender(),
                )
            };
            let mut child = (!late).then(make_child);
            assert_eq!(fixture.mailbox.background_tools(RUN), 1);
            foreground.await;
            control
                .release
                .send(Ok(ToolOutput::Plain(OUTPUT.into())))
                .unwrap();
            notices(&fixture.mailbox, 1).await;
            if late {
                child = Some(make_child());
            }
            let child = child.unwrap();
            assert!(child.is_detached());
            child.set_initial_prompt(OUTPUT);
            child.detach();
            let detached: Vec<_> = fixture
                .events
                .drain()
                .filter_map(|envelope| match envelope.event {
                    AgentEvent::SubagentDetached { child } => Some((envelope.run_id, child)),
                    _ => None,
                })
                .collect();
            assert_eq!(detached.len(), 1);
            assert_eq!(detached[0].0, DETACHED_RUN_ID);
            assert_eq!(detached[0].1.prompt.as_deref(), Some(CHILD_PROMPT));
            detached[0]
                .1
                .answer_tx
                .as_ref()
                .unwrap()
                .send(OUTPUT.into())
                .unwrap();
            assert_eq!(answer_rx.try_recv().unwrap(), OUTPUT);
            assert!(
                running
                    .running_tool
                    .as_ref()
                    .unwrap()
                    .background_session()
                    .is_some()
            );
        });
    }

    #[test]
    fn promotion_preserves_generic_invocations_and_real_outputs_once() {
        smol::block_on(async {
            let fixture = fixture();
            let first = controlled(&fixture.ctx, FIRST);
            let second = controlled(&fixture.ctx, SECOND);
            let third = controlled(&fixture.ctx, THIRD);
            let foreground = dispatch(
                fixture.ctx.clone(),
                vec![
                    (FIRST, "a".into()),
                    (SECOND, "b".into()),
                    (THIRD, "c".into()),
                ],
            );
            let first_ctx = first.started.recv_async().await.unwrap();
            first_ctx
                .call_instructions
                .record(vec![crate::InstructionBlock {
                    path: INSTRUCTION_PATH.into(),
                    content: INSTRUCTION_TEXT.into(),
                }]);
            second.started.recv_async().await.unwrap();
            third.started.recv_async().await.unwrap();
            first_ctx
                .event_tx
                .send(AgentEvent::ToolOutput {
                    id: "a".into(),
                    content: LIVE.into(),
                })
                .unwrap();
            while !matches!(
                fixture.events.recv_async().await.unwrap().event,
                AgentEvent::ToolOutput { .. }
            ) {}
            let (rejected, delivery) = flume::bounded(1);
            first_ctx
                .result_delivery
                .as_ref()
                .unwrap()
                .on_reject(move || {
                    let _ = rejected.try_send(());
                });
            assert_eq!(fixture.mailbox.background_tools(RUN + 1), 0);
            assert_eq!(fixture.mailbox.background_tools(RUN), 3);
            assert_eq!(fixture.mailbox.background_tools(RUN), 0);
            let history = foreground.await;
            assert_eq!(history.as_slice().len(), 2);
            assert_ne!(task_id(&history, 0), task_id(&history, 1));
            assert!(delivery.is_empty());
            let events: Vec<_> = fixture.events.drain().collect();
            assert!(
                !events.iter().any(|envelope| matches!(
                    envelope.event,
                    AgentEvent::ToolResultsSubmitted { .. }
                ))
            );
            assert!(events.iter().any(|envelope| {
                matches!(&envelope.event, AgentEvent::ToolOutput { content,.. } if content == LIVE)
                    && envelope
                        .subagent
                        .as_ref()
                        .is_some_and(|info| info.kind == TaskKind::Tool)
            }));
            fixture.run.cancel();
            assert!(!first_ctx.cancel.is_cancelled());
            first.release.send(Err(FAILED.into())).unwrap();
            second
                .release
                .send(Ok(ToolOutput::Markdown(TextOutput {
                    text: OUTPUT.into(),
                    loaded_tools: vec![LOADED.into()],
                    ..TextOutput::from("")
                })))
                .unwrap();
            third
                .release
                .send(Ok(ToolOutput::Image {
                    source: ImageSource::new(ImageMediaType::Png, "aW1hZ2U=".into()),
                    text: OUTPUT.into(),
                }))
                .unwrap();
            let completed = notices(&fixture.mailbox, 3).await;
            let mut labels = completed
                .iter()
                .filter_map(|notice| notice.message.display_text.as_deref())
                .collect::<Vec<_>>();
            labels.sort_unstable();
            assert_eq!(labels, [FIRST_FAILED, SECOND_FINISHED, THIRD_FINISHED]);
            assert!(
                first_ctx
                    .loaded_instructions
                    .contains(Path::new(INSTRUCTION_PATH)),
                "the pending notice carries the instruction block"
            );
            assert!(completed.iter().any(|notice| {
                notice
                    .message
                    .first_text_content()
                    .is_some_and(|text| text.contains(INSTRUCTION_TEXT))
            }));
            assert!(completed.iter().any(|notice| {
                notice
                    .message
                    .background_tool
                    .as_ref()
                    .is_some_and(|result| result.is_error)
                    && notice
                        .message
                        .first_text_content()
                        .unwrap()
                        .contains(FAILED)
            }));
            assert!(completed.iter().any(|notice| {
                notice
                    .message
                    .background_tool
                    .as_ref()
                    .is_some_and(|result| result.loaded_tools == [LOADED])
            }));
            assert!(completed.iter().any(|notice| {
                notice
                    .message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Image { .. }))
            }));
            assert!(
                delivery.is_empty(),
                "execution completion cannot acknowledge model delivery"
            );
            drop(completed);
            assert!(
                !first_ctx
                    .loaded_instructions
                    .contains(Path::new(INSTRUCTION_PATH))
            );
            delivery.recv_async().await.unwrap();
            for control in [first, second, third] {
                assert_eq!(control.count.load(Ordering::Relaxed), 1);
                control.ended.recv_async().await.unwrap();
            }
        });
    }

    /// Work a call leaves behind, like a render queued with
    /// `maki.async.run` or a session a plugin kept, holds the call's token:
    /// a stop cancels it, the call ending on its own does not. The run
    /// ending does too, unless the call moved out of the run.
    #[test_case(false, false; "attached_completion")]
    #[test_case(true, false; "moved_completion")]
    #[test_case(false, true; "attached_esc")]
    #[test_case(true, true; "moved_stop")]
    fn only_a_stop_or_its_run_ending_cancels_what_a_call_leaves_running(
        moved: bool,
        stopped: bool,
    ) {
        smol::block_on(async {
            let fixture = fixture();
            let control = controlled(&fixture.ctx, FIRST);
            let foreground = dispatch(fixture.ctx.clone(), vec![(FIRST, FIRST.into())]);
            let running = control.started.recv_async().await.unwrap();
            let complete = || {
                control
                    .release
                    .send(Ok(ToolOutput::Plain(OUTPUT.into())))
                    .unwrap()
            };
            if moved {
                assert_eq!(fixture.mailbox.background_tools(RUN), 1);
                let history = foreground.await;
                if stopped {
                    fixture.ctx.subagent_cancels.cancel(task_id(&history, 0));
                } else {
                    complete();
                }
                notices(&fixture.mailbox, 1).await;
            } else {
                if stopped {
                    fixture.run.cancel();
                } else {
                    complete();
                }
                foreground.await;
            }
            assert_eq!(running.cancel.is_cancelled(), stopped);
            fixture.ctx.subagent_cancels.end_foreground();
            assert_eq!(running.cancel.is_cancelled(), stopped || !moved);
        });
    }

    /// A dropped foreground stops an attached call where it is, so a write
    /// waiting for its file never runs once the lock frees.
    #[test]
    fn a_dropped_foreground_stops_a_call_waiting_for_its_file() {
        smol::block_on(async {
            let fixture = fixture();
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join(FIRST);
            let control = controlled_tool(&fixture.ctx, FIRST, false, Some(path.clone()));
            let lock = fixture
                .ctx
                .file_access
                .acquire(&crate::tools::FileKey::new(&path))
                .await;
            let foreground = dispatch(fixture.ctx.clone(), vec![(FIRST, FIRST.into())]);
            while !matches!(
                fixture.events.recv_async().await.unwrap().event,
                AgentEvent::ToolStart(_)
            ) {}
            assert!(foreground.cancel().await.is_none());
            drop(lock);
            while !matches!(
                fixture.events.recv_async().await.unwrap().event,
                AgentEvent::ToolDone(_)
            ) {}
            assert_eq!(control.count.load(Ordering::Relaxed), 0);
        });
    }

    #[test_case(false ; "explicit_stop")]
    #[test_case(true ; "generation_teardown")]
    fn promoted_work_obeys_its_new_cancel_owner(generation: bool) {
        smol::block_on(async {
            let fixture = fixture();
            let control = controlled(&fixture.ctx, FIRST);
            let foreground = dispatch(fixture.ctx.clone(), vec![(FIRST, "a".into())]);
            let running = control.started.recv_async().await.unwrap();
            assert_eq!(fixture.mailbox.background_tools(RUN), 1);
            let history = foreground.await;
            fixture.run.cancel();
            assert!(!running.cancel.is_cancelled());
            if generation {
                fixture.generation.cancel();
            } else {
                fixture.ctx.subagent_cancels.cancel(task_id(&history, 0));
            }
            control.ended.recv_async().await.unwrap();
            assert!(running.cancel.is_cancelled());
            if generation {
                let messages = loop {
                    if let AgentEvent::SubagentHistory { messages, .. } =
                        fixture.events.recv_async().await.unwrap().event
                    {
                        break messages;
                    }
                };
                assert_eq!(
                    messages[0].user_text(),
                    Some(json!({ "index": 0 }).to_string().as_str()),
                    "the moved call's transcript keeps the input it started with"
                );
                assert!(fixture.mailbox.claim_wake().is_empty());
            } else {
                let completed = notices(&fixture.mailbox, 1).await;
                assert!(
                    completed[0]
                        .message
                        .background_tool
                        .as_ref()
                        .unwrap()
                        .is_error
                );
                assert!(
                    completed[0]
                        .message
                        .first_text_content()
                        .unwrap()
                        .contains(CANCELLED)
                );
                assert_eq!(
                    completed[0].message.display_text.as_deref(),
                    Some(FIRST_CANCELLED)
                );
            }
        });
    }

    #[test]
    fn nested_dispatch_continues_under_the_same_promoted_owner() {
        smol::block_on(async {
            let mut fixture = fixture();
            let inner = controlled(&fixture.ctx, FIRST);
            fixture.ctx.local_tools = Arc::new(HashMap::from([(
                WRAPPER.into(),
                local_tool(ToolAudience::all(), |input, ctx| {
                    Box::pin(async move {
                        let done = tool_dispatch::run(
                            "nested".into(),
                            FIRST,
                            &input,
                            &ctx,
                            CallOrigin::Model,
                        )
                        .await;
                        Ok(done.output.as_text())
                    })
                }),
            )]));
            let foreground = dispatch(fixture.ctx.clone(), vec![(WRAPPER, "outer".into())]);
            let child = inner.started.recv_async().await.unwrap();
            assert_eq!(fixture.mailbox.background_tools(RUN), 1);
            let history = foreground.await;
            fixture.run.cancel();
            assert!(
                child
                    .running_tool
                    .as_ref()
                    .unwrap()
                    .background_session()
                    .is_some()
            );
            assert!(!child.cancel.is_cancelled());
            inner
                .release
                .send(Ok(ToolOutput::Plain(OUTPUT.into())))
                .unwrap();
            let notice = notices(&fixture.mailbox, 1).await.pop().unwrap();
            assert!(
                notice
                    .message
                    .first_text_content()
                    .unwrap()
                    .contains(OUTPUT)
            );
            assert_eq!(
                notice.message.background_tool.as_ref().unwrap().task_id,
                task_id(&history, 0)
            );
            assert_eq!(inner.count.load(Ordering::Relaxed), 1);
        });
    }

    #[test]
    fn completion_before_promotion_keeps_its_original_result() {
        smol::block_on(async {
            let fixture = fixture();
            let first = controlled(&fixture.ctx, FIRST);
            let second = controlled(&fixture.ctx, SECOND);
            let foreground = dispatch(
                fixture.ctx.clone(),
                vec![(FIRST, "a".into()), (SECOND, "b".into())],
            );
            first.started.recv_async().await.unwrap();
            second.started.recv_async().await.unwrap();
            first
                .release
                .send(Ok(ToolOutput::Plain(OUTPUT.into())))
                .unwrap();
            while !matches!(&fixture.events.recv_async().await.unwrap().event,AgentEvent::ToolDone(done) if done.id == "a")
            {
            }
            assert_eq!(fixture.mailbox.background_tools(RUN), 1);
            let history = foreground.await;
            assert!(
                matches!(&history.as_slice()[1].content[0],ContentBlock::ToolResult { tool_use_id,content,.. } if tool_use_id == "a" && content == OUTPUT)
            );
            let submitted: Vec<_> = fixture
                .events
                .drain()
                .filter_map(|envelope| match envelope.event {
                    AgentEvent::ToolResultsSubmitted { message } => Some(message),
                    _ => None,
                })
                .collect();
            assert_eq!(submitted.len(), 1);
            assert!(
                matches!(&submitted[0].content[..],[ContentBlock::ToolResult {tool_use_id,..}] if tool_use_id == "a")
            );
            second
                .release
                .send(Ok(ToolOutput::Plain(OUTPUT.into())))
                .unwrap();
            let notice = notices(&fixture.mailbox, 1).await.pop().unwrap();
            assert_eq!(
                notice.message.background_tool.as_ref().unwrap().tool_use_id,
                "b"
            );
        });
    }

    #[test]
    fn nested_permission_replay_preserves_each_actual_reply_channel() {
        smol::block_on(async {
            let fixture = fixture();
            let control = controlled(&fixture.ctx, FIRST);
            let foreground = dispatch(fixture.ctx.clone(), vec![(FIRST, "outer".into())]);
            let running = control.started.recv_async().await.unwrap();
            let mut replies = Vec::new();
            for child in [FIRST, SECOND] {
                let (answer_tx, answer_rx) = flume::unbounded();
                replies.push(answer_rx);
                running
                    .event_tx
                    .send_envelope(Envelope {
                        event: AgentEvent::PermissionRequest {
                            id: REUSED_ID.into(),
                            tool: maki_config::ToolKey::native(GUARDED_TOOL_NAME),
                            scopes: vec![child.into()],
                            reason: None,
                        },
                        subagent: Some(SubagentInfo {
                            kind: TaskKind::Agent,
                            parent_tool_use_id: child.into(),
                            name: child.into(),
                            prompt: None,
                            model: None,
                            detached: false,
                            owner_cancel: None,
                            spawn_tool_use_id: None,
                            opts: None,
                            answer_tx: Some(answer_tx),
                            permission_tool_use_id: None,
                            inbox: None,
                        }),
                        run_id: RUN,
                    })
                    .unwrap();
            }
            let mut published = Vec::new();
            while published.len() < 2 {
                if let AgentEvent::PermissionRequest { id, .. } =
                    fixture.events.recv_async().await.unwrap().event
                {
                    published.push(id);
                }
            }
            assert_ne!(published[0], published[1]);
            assert_eq!(fixture.mailbox.background_tools(RUN), 1);
            foreground.await;
            let mut replay = Vec::new();
            while replay.len() < 2 {
                let envelope = fixture.events.recv_async().await.unwrap();
                if let AgentEvent::PermissionRequest { id, .. } = envelope.event {
                    replay.push((id, envelope.subagent.unwrap()));
                }
            }
            assert_eq!(replay[0].0, published[0]);
            assert_eq!(replay[1].0, published[1]);
            assert_eq!(replay[0].1.parent_tool_use_id, FIRST);
            assert_eq!(replay[1].1.parent_tool_use_id, SECOND);
            assert!(replay.iter().all(|(_, info)| info.detached));
            let (original, ingress) =
                SessionMailbox::tool_permission(fixture.mailbox.session_id(), &replay[0].0)
                    .unwrap();
            assert_eq!(original, REUSED_ID);
            ingress
                .send(TaggedAnswer::new(&replay[0].0, PermissionAnswer::AllowOnce).encode())
                .unwrap();
            assert_eq!(
                TaggedAnswer::decode(&replies[0].recv_async().await.unwrap()).unwrap(),
                TaggedAnswer::new(REUSED_ID, PermissionAnswer::AllowOnce)
            );
            assert!(replies[1].is_empty());
            replay[1]
                .1
                .answer_tx
                .as_ref()
                .unwrap()
                .send(TaggedAnswer::new(&replay[1].0, PermissionAnswer::AllowOnce).encode())
                .unwrap();
            assert_eq!(
                TaggedAnswer::decode(&replies[1].recv_async().await.unwrap()).unwrap(),
                TaggedAnswer::new(REUSED_ID, PermissionAnswer::AllowOnce)
            );
            control
                .release
                .send(Ok(ToolOutput::Plain(OUTPUT.into())))
                .unwrap();
            notices(&fixture.mailbox, 1).await;
        });
    }

    #[test]
    fn permission_replies_remain_private_when_the_next_run_reuses_ids() {
        smol::block_on(async {
            let mut fixture = fixture();
            fixture
                .ctx
                .registry
                .register(
                    Arc::new(GuardedMock),
                    ToolSource::Lua {
                        plugin: "fixture".into(),
                    },
                )
                .unwrap();
            fixture.ctx.permissions = Arc::new(PermissionManager::new(
                PermissionsConfig::default(),
                PathBuf::from("/tmp"),
                ProjectConfig::for_project(Path::new("/tmp")),
                Arc::default(),
            ));
            let first = dispatch(
                fixture.ctx.clone(),
                vec![(GUARDED_TOOL_NAME, REUSED_ID.into())],
            );
            let first_request = loop {
                if let AgentEvent::PermissionRequest { id, .. } =
                    fixture.events.recv_async().await.unwrap().event
                {
                    break id;
                }
            };
            assert_eq!(fixture.mailbox.background_tools(RUN), 1);
            first.await;
            let detached = loop {
                let event = fixture.events.recv_async().await.unwrap();
                if matches!(event.event, AgentEvent::PermissionRequest { .. }) {
                    break event.subagent.unwrap();
                }
            };
            let mut next_ctx = fixture.ctx.clone();
            next_ctx.event_tx = next_ctx.event_tx.with_run_id(RUN + 1);
            let second = dispatch(next_ctx, vec![(GUARDED_TOOL_NAME, REUSED_ID.into())]);
            let second_request = loop {
                if let AgentEvent::PermissionRequest { id, .. } =
                    fixture.events.recv_async().await.unwrap().event
                {
                    break id;
                }
            };
            assert_ne!(first_request, second_request);
            assert!(SessionMailbox::reply_tool(
                fixture.mailbox.session_id(),
                &TaggedAnswer::new(&second_request, PermissionAnswer::AllowOnce).encode()
            ));
            let completed = second.await;
            assert!(!matches!(
                &completed.as_slice()[1].content[0],
                ContentBlock::ToolResult { is_error: true, .. }
            ));
            assert!(fixture.mailbox.claim_wake().is_empty());
            detached
                .answer_tx
                .unwrap()
                .send(TaggedAnswer::new(&first_request, PermissionAnswer::AllowOnce).encode())
                .unwrap();
            assert_eq!(notices(&fixture.mailbox, 1).await.len(), 1);
            assert!(!SessionMailbox::reply_tool(
                fixture.mailbox.session_id(),
                &TaggedAnswer::new(&first_request, PermissionAnswer::AllowOnce).encode()
            ));
        });
    }

    #[test]
    fn capacity_rejection_is_atomic_and_completion_releases_a_slot() {
        smol::block_on(async {
            let fixture = fixture();
            let first = controlled(&fixture.ctx, FIRST);
            let second = controlled(&fixture.ctx, SECOND);
            let foreground = dispatch(
                fixture.ctx.clone(),
                (0..63).map(|index| (FIRST, index.to_string())).collect(),
            );
            for _ in 0..63 {
                first.started.recv_async().await.unwrap();
            }
            assert_eq!(fixture.mailbox.background_tools(RUN), 63);
            foreground.await;
            let mut next = fixture.ctx.clone();
            next.event_tx = next.event_tx.with_run_id(RUN + 1);
            let waiting = dispatch(
                next,
                vec![(SECOND, "waiting-a".into()), (SECOND, "waiting-b".into())],
            );
            second.started.recv_async().await.unwrap();
            second.started.recv_async().await.unwrap();
            assert_eq!(fixture.mailbox.background_tools(RUN + 1), 0);
            first
                .release
                .send(Ok(ToolOutput::Plain(OUTPUT.into())))
                .unwrap();
            notices(&fixture.mailbox, 1).await;
            assert_eq!(fixture.mailbox.background_tools(RUN + 1), 2);
            waiting.await;
            fixture.generation.cancel();
            for _ in 0..63 {
                first.ended.recv_async().await.unwrap();
            }
            second.ended.recv_async().await.unwrap();
            second.ended.recv_async().await.unwrap();
        });
    }
}
