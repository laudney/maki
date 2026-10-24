use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use futures_lite::FutureExt;
use futures_lite::future::{or, pending, race, zip};
use maki_providers::{BackgroundToolResult, ContentBlock, DeliveryReceipt, Message, Role};
use maki_storage::id::MakiId;
use serde_json::{Value, json};

use super::CallInstructions;
use super::tool_dispatch;
use crate::cancel::{CancelMap, CancelSlot, CancelToken, CancelTrigger};
use crate::permissions::TaggedAnswer;
use crate::tools::{CallOrigin, ToolContext};
use crate::{
    AgentEvent, DETACHED_RUN_ID, Envelope, EventSender, Notice, ResultDelivery, SessionMailbox,
    SubagentInfo, SubagentVerdict, TaskKind, ToolDoneEvent, ToolOutput, ToolStartEvent,
    event_stream,
};

const CANCELLED: &str = "cancelled";

/// A nested dispatch reads the same ownership decision as its invocation.
#[derive(Clone)]
pub struct RunningTool {
    batch: Weak<ForegroundTools>,
    index: usize,
}

impl RunningTool {
    pub fn background_session(&self) -> Option<MakiId> {
        let batch = self.batch.upgrade()?;
        let detached = batch.calls()[self.index].phase == Phase::Detached;
        detached.then(|| batch.mailbox.session_id())
    }
}

#[derive(PartialEq, Eq)]
enum Phase {
    Attached,
    Detached,
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
            if call.phase == Phase::Attached {
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

    pub(crate) fn promote(&self) -> usize {
        let mut calls = self.calls();
        if self.run_cancel.is_cancelled() || !self.mailbox.owns_generation(&self.owner) {
            return 0;
        }
        let unfinished: Vec<_> = calls
            .iter()
            .enumerate()
            .filter(|(_, call)| call.phase == Phase::Attached && !call.cancel.is_cancelled())
            .map(|(index, _)| index)
            .collect();
        let count = unfinished.len();
        if count == 0 || !self.mailbox.reserve_tools(count) {
            return 0;
        }
        for index in unfinished {
            let call = &mut calls[index];
            call.phase = Phase::Detached;
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
                output: Arc::new(ToolOutput::Plain(json!({
                    "task_id": call.task_id,
                    "status": "running",
                    "tool": call.name,
                    "notification": "The existing invocation continues in the background. Its final result will arrive as a completion notification.",
                }).to_string().into())),
                is_error: false,
                annotation: None,
                written_path: None,
                call: None,
            };
            self.event_tx
                .try_send(AgentEvent::ToolBackgrounded(Box::new(done.clone())));
            let _ = call.result_tx.try_send(ManagedResult {
                done,
                promoted: true,
                delivery: None,
            });
        }
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
        if call.phase == Phase::Detached {
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
        self.calls()[index].phase == Phase::Attached
    }

    fn cancel_attached(&self, index: usize) {
        let mut calls = self.calls();
        let call = &mut calls[index];
        if call.phase == Phase::Attached {
            call.trigger.take();
        }
    }

    fn finish(self: &Arc<Self>, index: usize, done: ToolDoneEvent) {
        let mut calls = self.calls();
        let call = &mut calls[index];
        let detached = call.phase == Phase::Detached;
        call.phase = Phase::Finished;
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
            self.event_tx
                .try_send(AgentEvent::ToolDone(Box::new(done.clone())));
            let _ = call.result_tx.try_send(ManagedResult {
                done,
                promoted: false,
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
        let (trigger, cancel) = owner.child();
        let (result_tx, result_rx) = flume::bounded(1);
        let (answer_tx, answer_rx) = flume::unbounded();
        let (private_tx, private_rx) = flume::unbounded();
        let can_answer = ctx.user_response_rx.is_some();
        let delivery = ResultDelivery::default();
        let (stream, events) = event_stream();
        let tool_ctx = ToolContext {
            tool_use_id: Some(id.clone()),
            call_instructions: CallInstructions::default(),
            cancel: cancel.clone(),
            event_tx: ctx.event_tx.tool_sender(&stream),
            user_response_rx: can_answer.then(|| Arc::new(smol::lock::Mutex::new(private_rx))),
            result_delivery: Some(delivery.clone()),
            running_tool: Some(RunningTool {
                batch: Arc::downgrade(&batch),
                index,
            }),
            ..ctx.clone()
        };
        batch.calls().push(Call {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            task_id: format!("tool-{}", MakiId::generate()),
            phase: Phase::Attached,
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
            name.clone(),
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
    for (index, (id, name, input, tool_ctx, stream, mut events, answers)) in
        workers.into_iter().enumerate()
    {
        let batch = Arc::clone(&batch);
        let waiter_gone = waiter_gone.clone();
        smol::spawn(async move {
            let execution = async {
                let run = AssertUnwindSafe(tool_dispatch::run(
                    id.clone(),
                    &name,
                    &input,
                    &tool_ctx,
                    CallOrigin::Model,
                ))
                .catch_unwind();
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
    const INSTRUCTION_TEXT: &str = "subdirectory rule";
    const PARTIAL: &str = "partial output before the stop";

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
                match released {
                    Some(reply) => reply.unwrap().into(),
                    None => Err(CANCELLED.to_owned()).into(),
                }
            })
        }
    }

    impl Tool for ControlledTool {
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
        controlled_tool(ctx, name, None)
    }

    fn controlled_tool(ctx: &ToolContext, name: &'static str, path: Option<PathBuf>) -> Control {
        let (started, starts) = flume::unbounded();
        let (release, releases) = flume::unbounded();
        let (ended, ends) = flume::unbounded();
        let count = Arc::new(AtomicUsize::new(0));
        ctx.registry
            .register(
                Arc::new(ControlledTool {
                    name,
                    started,
                    release: releases,
                    ended,
                    count: Arc::clone(&count),
                    path,
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

    #[test]
    fn a_call_moved_while_it_waits_for_its_file_rechecks_the_write_gate() {
        smol::block_on(async {
            let fixture = fixture();
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join(FIRST);
            let control = controlled_tool(&fixture.ctx, FIRST, Some(path.clone()));
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
            let ContentBlock::ToolResult {
                content, is_error, ..
            } = &history.as_slice()[1].content[0]
            else {
                panic!("expected a tool result");
            };
            assert_eq!((content.as_str(), *is_error), (PARTIAL, false));
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
            let control = controlled_tool(&fixture.ctx, FIRST, Some(path.clone()));
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
                while !matches!(
                    fixture.events.recv_async().await.unwrap().event,
                    AgentEvent::SubagentHistory { .. }
                ) {}
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
