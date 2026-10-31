//! Queue of work handed from the UI to the agent loop.
//!
//! Shutdown rides on `Drop`: when the last [`QueueSender`] goes away, flume
//! closes the notify channel, so the receiver's `recv_notify` wakes with an
//! `Err` and the agent loop falls out of its main loop on its own. That way
//! nobody needs a separate "please stop" flag, and callers can't forget to
//! set it.

use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use maki_types::input::{AgentInput, AgentMode, SessionInput};
use maki_types::message::ImageSource;

use maki_storage::id::MakiId;
use maki_storage::sessions::{QueueReceipt, QueueStatus};
use maki_wire::sanitize::sanitize_display;

pub(crate) use maki_types::input::merge_inputs;

use crate::components::input::Submission;
use crate::components::queue_panel::QueueEntry;
use crate::theme;

const COMPACT_LABEL: &str = "/compact";

type Items = Arc<Mutex<QueueState>>;

#[derive(Default)]
struct QueueState {
    unsent: VecDeque<QueueItem>,
    hosted: Vec<HostedItem>,
    rejected: BTreeMap<u64, QueueItem>,
    cancellations: Vec<MakiId>,
}

struct HostedItem {
    id: MakiId,
    tag: Option<u64>,
    item: QueueItem,
    accepted: bool,
    started: bool,
    cancelled: bool,
}

/// What a message must agree on with its neighbours to share their turn.
type BatchKey = (AgentMode, bool);

pub(crate) struct QueuedMessage {
    pub(crate) text: String,
    pub(crate) images: Vec<ImageSource>,
}

impl From<Submission> for QueuedMessage {
    fn from(sub: Submission) -> Self {
        Self {
            text: sub.text,
            images: sub.images,
        }
    }
}

#[derive(Clone)]
pub(crate) struct QueuedInput {
    pub(crate) text: String,
    pub(crate) input: AgentInput,
    pub(crate) run_id: u64,
    /// `true` when the UI already drew the bubble (immediate dispatch).
    /// The agent then skips `QueueItemConsumed` so we don't draw it twice.
    /// `false` when the user typed while the agent was busy: the UI waits
    /// for `QueueItemConsumed` before drawing.
    pub(crate) displayed: bool,
}

#[derive(Clone)]
pub(crate) struct Compaction {
    pub(crate) run_id: u64,
    pub(crate) instructions: Option<String>,
    /// The session's workflow mode when it was asked for: the frame left
    /// behind must carry the tools the next run sends.
    pub(crate) workflow: bool,
}

impl Compaction {
    fn label(&self) -> Cow<'static, str> {
        match self.instructions {
            Some(ref extra) => Cow::Owned(format!("{COMPACT_LABEL} {extra}")),
            None => Cow::Borrowed(COMPACT_LABEL),
        }
    }
}

#[derive(Clone)]
pub(crate) enum QueueItem {
    Message(QueuedInput),
    Compact(Compaction),
}

/// One turn's worth of work. Plain messages typed back to back under the same
/// mode and workflow travel together, so the whole burst costs one run and one
/// request. Everything else travels alone and keeps its place, since
/// `/compact` rewrites the history the later messages land in.
pub(crate) enum QueueRun {
    Messages(Vec<QueuedInput>),
    Compact(Compaction),
}

impl QueueRun {
    /// Forgets whatever the user cancelled before we got to it, and reports
    /// the id the rest rides on: the newest one, the only run the UI still
    /// considers current.
    pub(crate) fn drop_cancelled(&mut self, min_run_id: u64) -> Option<u64> {
        match self {
            Self::Messages(messages) => {
                messages.retain(|queued| queued.run_id >= min_run_id);
                Some(messages.last()?.run_id)
            }
            Self::Compact(compaction) => {
                (compaction.run_id >= min_run_id).then_some(compaction.run_id)
            }
        }
    }
}

impl QueueItem {
    pub(crate) fn run_id(&self) -> u64 {
        match self {
            Self::Message(item) => item.run_id,
            Self::Compact(item) => item.run_id,
        }
    }

    fn as_queue_entry(&self) -> QueueEntry<'static> {
        match self {
            Self::Message(queued) => QueueEntry {
                text: Cow::Owned(queued.text.clone()),
                color: theme::current().foreground,
            },
            Self::Compact(compaction) => QueueEntry {
                text: compaction.label(),
                color: theme::current()
                    .queue
                    .fg
                    .unwrap_or(theme::current().foreground),
            },
        }
    }

    fn into_draft(self) -> (String, Vec<ImageSource>) {
        match self {
            Self::Message(queued) => (queued.text, queued.input.images),
            Self::Compact(compact) => (compact.label().into_owned(), Vec::new()),
        }
    }

    fn batch_key(&self) -> Option<BatchKey> {
        match self {
            Self::Message(queued) => queued.input.batch_key(),
            Self::Compact(_) => None,
        }
    }

    /// Immediate-dispatch messages already sit in the chat, so hiding them
    /// here stops the panel from reserving a row the agent is about to free,
    /// which used to make the bubble hop up by one frame.
    fn visible_in_panel(&self) -> bool {
        match self {
            Self::Message(queued) => !queued.displayed,
            Self::Compact(_) => true,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub(crate) struct QueueSender {
    items: Items,
    notify_tx: flume::Sender<()>,
}

pub(crate) struct QueueReceiver {
    items: Items,
    notify_rx: flume::Receiver<()>,
}

pub(crate) fn queue() -> (QueueSender, QueueReceiver) {
    let (notify_tx, notify_rx) = flume::bounded(1);
    let items: Items = Arc::default();
    (
        QueueSender {
            items: Arc::clone(&items),
            notify_tx,
        },
        QueueReceiver { items, notify_rx },
    )
}

impl QueueSender {
    pub(crate) fn push(&self, entry: QueueItem) {
        lock(&self.items).unsent.push_back(entry);
        let _ = self.notify_tx.try_send(());
    }

    pub(crate) fn remove(&self, index: usize) -> Option<QueueItem> {
        let mut state = lock(&self.items);
        let hosted: Vec<_> = state
            .hosted
            .iter()
            .enumerate()
            .filter(|(_, item)| !item.started && !item.cancelled && item.item.visible_in_panel())
            .map(|(pos, _)| pos)
            .collect();
        let item = if let Some(&pos) = hosted.get(index) {
            let item = &mut state.hosted[pos];
            item.cancelled = true;
            let (id, item) = (item.id, item.item.clone());
            state.cancellations.push(id);
            Some(item)
        } else {
            state.unsent.remove(index.saturating_sub(hosted.len()))
        };
        let _ = self.notify_tx.try_send(());
        item
    }

    pub(crate) fn len(&self) -> usize {
        let state = lock(&self.items);
        state.unsent.len()
            + state
                .hosted
                .iter()
                .filter(|item| !item.started && !item.cancelled)
                .count()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn clear(&self) {
        let mut state = lock(&self.items);
        state.unsent.clear();
        state.rejected.clear();
        let ids: Vec<_> = state
            .hosted
            .iter_mut()
            .filter(|item| !item.started && !item.cancelled)
            .map(|item| {
                item.cancelled = true;
                item.id
            })
            .collect();
        state.cancellations.extend(ids);
        let _ = self.notify_tx.try_send(());
    }

    pub(crate) fn take_messages(&self) -> Vec<(String, Vec<ImageSource>)> {
        let mut state = lock(&self.items);
        let rejected = std::mem::take(&mut state.rejected);
        let mut pending: Vec<_> = state
            .hosted
            .extract_if(.., |item| !item.accepted)
            .map(|item| (item.tag, item.item))
            .chain(rejected.into_iter().map(|(tag, item)| (Some(tag), item)))
            .collect();
        pending.sort_by_key(|(tag, _)| *tag);
        pending
            .into_iter()
            .map(|(_, item)| item)
            .chain(std::mem::take(&mut state.unsent))
            .map(QueueItem::into_draft)
            .collect()
    }

    pub(crate) fn take_rejected(&self) -> Vec<(String, Vec<ImageSource>)> {
        std::mem::take(&mut lock(&self.items).rejected)
            .into_values()
            .map(QueueItem::into_draft)
            .collect()
    }

    pub(crate) fn text_messages(&self) -> Vec<String> {
        let state = lock(&self.items);
        state
            .unsent
            .iter()
            .filter(|item| item.visible_in_panel())
            .filter_map(|item| match item {
                QueueItem::Message(queued) => Some(queued.text.clone()),
                QueueItem::Compact(_) => None,
            })
            .collect()
    }

    pub(crate) fn panel_len(&self) -> usize {
        self.panel_entries().len()
    }

    pub(crate) fn panel_entries(&self) -> Vec<QueueEntry<'static>> {
        let state = lock(&self.items);
        state
            .hosted
            .iter()
            .filter(|item| !item.started && !item.cancelled)
            .map(|item| &item.item)
            .chain(state.unsent.iter())
            .filter(|item| item.visible_in_panel())
            .map(QueueItem::as_queue_entry)
            .collect()
    }
}

impl QueueReceiver {
    /// Takes the next item plus every message queued right behind it that
    /// shares its batch key.
    pub(crate) fn pop_run(&self) -> Option<QueueRun> {
        pop_run_from(&mut lock(&self.items).unsent)
    }

    /// Runs `publish` under the queue lock, so a drain event can never
    /// interleave with a concurrent push.
    pub(crate) fn publish_if_empty(&self, publish: impl FnOnce()) {
        let items = lock(&self.items);
        if items.unsent.is_empty() && items.hosted.is_empty() {
            publish();
        }
    }

    pub(crate) fn submit(&self, tag: u64) -> Option<(MakiId, QueueItem)> {
        let mut state = lock(&self.items);
        let item = state.unsent.pop_front()?;
        let id = MakiId::generate();
        state.hosted.push(HostedItem {
            id,
            tag: Some(tag),
            item: item.clone(),
            accepted: false,
            started: false,
            cancelled: false,
        });
        Some((id, item))
    }

    pub(crate) fn reject(&self, tag: u64) {
        lock(&self.items)
            .hosted
            .retain(|item| item.tag != Some(tag));
    }

    pub(crate) fn submit_if_live<T>(&self, id: MakiId, send: impl FnOnce() -> T) -> Option<T> {
        let state = lock(&self.items);
        state
            .hosted
            .iter()
            .any(|item| item.id == id && !item.cancelled)
            .then(send)
    }

    pub(crate) fn cancelled(&self, id: MakiId) -> bool {
        lock(&self.items)
            .hosted
            .iter()
            .any(|item| item.id == id && item.cancelled)
    }

    pub(crate) fn restore_hosted(&self, id: MakiId, input: SessionInput, run_id: u64) {
        let mut state = lock(&self.items);
        if state.hosted.iter().any(|item| item.id == id) {
            return;
        }
        let item = match input {
            SessionInput::Prompt(mut input) => {
                input.message = sanitize_display(&input.message).into_owned();
                QueueItem::Message(QueuedInput {
                    text: input.message.clone(),
                    input,
                    run_id,
                    displayed: false,
                })
            }
            SessionInput::Compact(input) => QueueItem::Compact(Compaction {
                run_id,
                instructions: input
                    .instructions
                    .map(|text| sanitize_display(&text).into_owned()),
                workflow: input.workflow,
            }),
        };
        state.hosted.push(HostedItem {
            id,
            tag: None,
            item,
            accepted: true,
            started: false,
            cancelled: false,
        });
    }

    pub(crate) fn acknowledge(&self, tag: u64, accepted: bool) -> Option<MakiId> {
        let mut state = lock(&self.items);
        let pos = state.hosted.iter().position(|item| item.tag == Some(tag))?;
        let item = &mut state.hosted[pos];
        item.accepted = accepted;
        let id = item.id;
        if !accepted {
            let item = state.hosted.remove(pos);
            if !item.cancelled {
                state.rejected.insert(tag, item.item);
            }
        }
        Some(id)
    }

    pub(crate) fn receipt(&self, receipt: &QueueReceipt) -> Option<(u64, Option<QueueItem>)> {
        let mut state = lock(&self.items);
        let pos = state.hosted.iter().position(|item| item.id == receipt.id)?;
        let item = &mut state.hosted[pos];
        item.accepted = true;
        let run_id = item.item.run_id();
        let started = matches!(
            receipt.status,
            QueueStatus::Running
                | QueueStatus::Done
                | QueueStatus::Error
                | QueueStatus::Interrupted
        );
        let consumed = (started && !item.started).then(|| item.item.clone());
        item.started |= started;
        if !matches!(receipt.status, QueueStatus::Queued | QueueStatus::Running) {
            state.hosted.remove(pos);
        }
        Some((run_id, consumed))
    }

    pub(crate) fn cancel_hosted(&self, min_run_id: u64) -> Vec<MakiId> {
        let mut state = lock(&self.items);
        let mut ids: Vec<_> = state
            .hosted
            .iter_mut()
            .filter(|item| item.item.run_id() < min_run_id && !item.cancelled)
            .map(|item| {
                item.cancelled = true;
                item.id
            })
            .collect();
        ids.append(&mut state.cancellations);
        ids
    }

    pub(crate) fn has_hosted(&self) -> bool {
        !lock(&self.items).hosted.is_empty()
    }

    pub(crate) async fn recv_notify(&self) -> Result<(), flume::RecvError> {
        self.notify_rx.recv_async().await
    }
}

/// Takes the next item plus every message queued right behind it that
/// shares its batch key, from a queue the caller holds locked.
fn pop_run_from(items: &mut VecDeque<QueueItem>) -> Option<QueueRun> {
    let first = match items.pop_front()? {
        QueueItem::Compact(compaction) => return Some(QueueRun::Compact(compaction)),
        QueueItem::Message(first) => first,
    };
    let key = first.input.batch_key();
    let mut run = vec![first];
    while key.is_some() && items.front().and_then(QueueItem::batch_key) == key {
        let Some(QueueItem::Message(next)) = items.pop_front() else {
            break;
        };
        run.push(next);
    }
    Some(QueueRun::Messages(run))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::Barrier;
    use std::thread;

    use maki_types::input::{InputSource, McpPromptRef};
    use maki_types::message::ImageMediaType;

    use super::*;
    use test_case::test_case;

    const FIRST: &str = "first";
    const SECOND: &str = "second";
    const THIRD: &str = "third";
    const PROMPT_NAME: &str = "server/review";
    const PLAN_PATH: &str = "plan.md";
    const FIRST_TAG: u64 = 1;
    const SECOND_TAG: u64 = 2;

    fn input(message: &str) -> AgentInput {
        AgentInput {
            message: message.into(),
            mode: Default::default(),
            images: Vec::new(),
            preamble: Vec::new(),
            earlier: Vec::new(),
            thinking: Default::default(),
            fast: false,
            workflow: false,
            prompt: None,
            source: InputSource::Tui,
        }
    }

    fn queued(text: &str, run_id: u64) -> QueuedInput {
        QueuedInput {
            text: text.into(),
            input: input(text),
            run_id,
            displayed: false,
        }
    }

    fn message(text: &str) -> QueueItem {
        QueueItem::Message(queued(text, 0))
    }

    fn plan_message(text: &str) -> QueueItem {
        let mut queued = queued(text, 0);
        queued.input.mode = AgentMode::Plan(PLAN_PATH.into());
        QueueItem::Message(queued)
    }

    fn workflow_message(text: &str) -> QueueItem {
        let mut queued = queued(text, 0);
        queued.input.workflow = true;
        QueueItem::Message(queued)
    }

    fn prompt_message(text: &str) -> QueueItem {
        let mut queued = queued(text, 0);
        queued.input.prompt = Some(Box::new(McpPromptRef {
            qualified_name: PROMPT_NAME.into(),
            arguments: Default::default(),
        }));
        QueueItem::Message(queued)
    }

    fn compact(instructions: Option<&str>) -> QueueItem {
        QueueItem::Compact(Compaction {
            run_id: 0,
            instructions: instructions.map(str::to_string),
            workflow: false,
        })
    }

    /// Drains the queue and names every run it hands over: the texts of a
    /// burst of messages, or `/compact` on its own.
    fn runs(rx: &QueueReceiver) -> Vec<Vec<String>> {
        let mut runs = Vec::new();
        while let Some(run) = rx.pop_run() {
            runs.push(match run {
                QueueRun::Messages(messages) => {
                    messages.into_iter().map(|queued| queued.text).collect()
                }
                QueueRun::Compact { .. } => vec![COMPACT_LABEL.into()],
            });
        }
        runs
    }

    #[test]
    fn rejected_messages_keep_submission_order_when_replies_arrive_reversed() {
        let (tx, rx) = queue();
        tx.push(message(FIRST));
        tx.push(message(SECOND));
        rx.submit(FIRST_TAG).unwrap();
        rx.submit(SECOND_TAG).unwrap();
        rx.acknowledge(SECOND_TAG, false);
        rx.acknowledge(FIRST_TAG, false);
        tx.push(message(THIRD));
        let recovered: Vec<_> = tx
            .take_messages()
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        assert_eq!(recovered, [FIRST, SECOND, THIRD]);
    }

    #[test_case(message(FIRST), true  ; "deferred_message_visible")]
    #[test_case(QueueItem::Message(QueuedInput { displayed: true, ..queued(FIRST, 0) }), false ; "displayed_message_hidden")]
    #[test_case(compact(None), true  ; "compact_visible")]
    fn panel_visibility(item: QueueItem, visible: bool) {
        let (tx, _rx) = queue();
        tx.push(item);
        let expected = usize::from(visible);
        assert_eq!(tx.panel_len(), expected);
        assert_eq!(tx.panel_entries().len(), expected);
    }

    #[test]
    fn nonempty_queue_does_not_publish_drain() {
        let (tx, rx) = queue();
        tx.push(message(FIRST));
        let called = Cell::new(false);

        rx.publish_if_empty(|| called.set(true));
        assert!(!called.get());
    }

    #[test]
    fn drain_publication_is_serialized_with_push() {
        let (tx, rx) = queue();
        let barrier = Arc::new(Barrier::new(2));
        let order = Arc::new(Mutex::new(Vec::new()));
        let worker_barrier = Arc::clone(&barrier);
        let worker_order = Arc::clone(&order);
        let worker = thread::spawn(move || {
            worker_barrier.wait();
            tx.push(message(FIRST));
            lock(&worker_order).push("push");
        });

        rx.publish_if_empty(|| {
            barrier.wait();
            lock(&order).push("drain");
        });
        worker.join().unwrap();

        assert_eq!(*lock(&order), ["drain", "push"]);
    }

    #[test_case(vec![message(FIRST), message(SECOND), message(THIRD)], vec![vec![FIRST, SECOND, THIRD]] ; "adjacent_messages_ride_together")]
    #[test_case(vec![message(FIRST), compact(None), message(SECOND)], vec![vec![FIRST], vec![COMPACT_LABEL], vec![SECOND]] ; "compact_splits_the_drain_and_keeps_its_place")]
    #[test_case(vec![message(FIRST), prompt_message(SECOND), message(THIRD)], vec![vec![FIRST], vec![SECOND], vec![THIRD]] ; "mcp_prompt_runs_alone")]
    #[test_case(vec![message(FIRST)], vec![vec![FIRST]] ; "single_message_unchanged")]
    #[test_case(vec![plan_message(FIRST), plan_message(SECOND)], vec![vec![FIRST, SECOND]] ; "one_mode_rides_together")]
    #[test_case(vec![plan_message(FIRST), message(SECOND)], vec![vec![FIRST], vec![SECOND]] ; "mode_change_splits_the_drain")]
    #[test_case(vec![message(FIRST), workflow_message(SECOND), message(THIRD)], vec![vec![FIRST], vec![SECOND], vec![THIRD]] ; "workflow_change_splits_the_drain")]
    fn pop_run_groups_the_queue_into_turns(items: Vec<QueueItem>, expected: Vec<Vec<&str>>) {
        let (tx, rx) = queue();
        for item in items {
            tx.push(item);
        }

        assert_eq!(runs(&rx), expected);
        assert!(tx.is_empty());
    }

    #[test_case(0, Some(2) ; "nothing_cancelled")]
    #[test_case(2, Some(2) ; "cancelled_message_dropped")]
    #[test_case(3, None    ; "whole_run_cancelled")]
    fn drop_cancelled_keeps_the_newest_run_id(min_run_id: u64, expected: Option<u64>) {
        let mut run = QueueRun::Messages(vec![queued(FIRST, 1), queued(SECOND, 2)]);

        assert_eq!(run.drop_cancelled(min_run_id), expected);
    }

    #[test_case(&[FIRST] ; "single_message_stays_alone")]
    #[test_case(&[FIRST, SECOND, THIRD] ; "earlier_messages_stay_messages")]
    fn merge_inputs_keeps_every_message_with_its_own_image(texts: &[&str]) {
        let inputs = texts
            .iter()
            .map(|text| AgentInput {
                images: vec![ImageSource::new(ImageMediaType::Png, Arc::from(*text))],
                ..input(text)
            })
            .collect();

        let merged = merge_inputs(inputs).unwrap();

        let (last, earlier) = texts.split_last().unwrap();
        assert_eq!(merged.message, *last);
        assert_eq!(merged.images.len(), 1);
        assert!(merged.preamble.is_empty());
        let kept: Vec<_> = merged
            .earlier
            .iter()
            .map(|e| (e.message.as_str(), e.images.len()))
            .collect();
        let expected: Vec<_> = earlier.iter().map(|text| (*text, 1)).collect();
        assert_eq!(kept, expected);
    }
}
