use std::collections::BTreeMap;
use std::mem;
use std::ops::Bound;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use flume::{Receiver, Sender};
use serde_json::Value;

use super::ToolLive;
use crate::types::ALLOWED_BY;
use crate::{ChildStatus, ToolChild, ToolData, ToolDoneEvent};

/// One call's place in a model call's live progress: the root for the model
/// call itself, and a path below it for each call its handler runs through
/// `maki.agent.call_tool`, grandchildren included. Every scope of one model
/// call writes into the same sink, which dispatch drains on a tick and once
/// more before `ToolDone`, so a handler writes as often as it likes and the
/// stream stays coalesced.
#[derive(Debug, Clone, Default)]
pub struct ProgressScope {
    sink: Arc<ProgressSink>,
    path: Arc<[u32]>,
    /// Shared by every clone of this scope, so concurrent `call_tool`s of one
    /// handler get distinct indices.
    next_child: Arc<AtomicU32>,
}

impl ProgressScope {
    pub fn is_root(&self) -> bool {
        self.path.is_empty()
    }

    /// The scope of the next call this one starts.
    pub fn child(&self) -> Self {
        let index = self.next_child.fetch_add(1, Ordering::Relaxed);
        Self {
            sink: Arc::clone(&self.sink),
            path: self.path.iter().copied().chain([index]).collect(),
            next_child: Arc::default(),
        }
    }

    pub fn stream(&self, text: &str) {
        self.sink.lock().stream(&self.path, text);
    }

    pub fn set_state(&self, state: Value) {
        self.sink.lock().set_state(&self.path, state);
    }

    /// A nested call got past its start: its record runs from here.
    pub fn started(&self, data: &ToolData) {
        if !self.is_root() {
            self.sink.lock().start(&self.path, data);
        }
    }

    /// A live event the nested call sent its caller.
    pub fn live(&self, event: &ToolLive) {
        if !self.is_root() {
            self.sink.lock().live(&self.path, event);
        }
    }

    /// Settles the nested call's record from its result, and gives the
    /// result its own children and every live annotation it got.
    pub fn finished(&self, done: &mut ToolDoneEvent) {
        if !self.is_root() {
            self.sink.lock().finish(&self.path, done);
        }
    }

    /// The nested call was dropped before it ended: it and everything under
    /// it still running are cancelled. `fallback` describes a call dropped
    /// before its start was recorded.
    pub fn abandoned(&self, fallback: impl FnOnce() -> ToolData) {
        if !self.is_root() {
            self.sink.lock().abandon(&self.path, fallback);
        }
    }

    /// Every change to this call and the calls under it from now on, each
    /// as it is recorded, with its path below this one. The sink still
    /// coalesces the same changes for the stream, so watching takes nothing
    /// from it.
    pub fn watch(&self) -> ProgressWatch {
        let (tx, rx) = flume::unbounded();
        self.sink.lock().watchers.insert(self.path.to_vec(), tx);
        ProgressWatch {
            sink: Arc::clone(&self.sink),
            path: Arc::clone(&self.path),
            rx,
        }
    }

    /// Everything pending, the model call's own progress first, then each
    /// nested call's in path order.
    pub fn take(&self) -> Vec<PendingProgress> {
        self.sink.lock().drain()
    }

    /// The last take, at the model call's end: a call still running is
    /// cancelled, every record moves out, and later writes are dropped, so a
    /// task that kept a `ctx` past its call writes into nothing and keeps
    /// nothing alive.
    pub fn close(&self) -> Closed {
        self.sink.lock().close()
    }

    pub(crate) fn checkpoint(&self) -> Closed {
        let mut sink = self.sink.lock();
        Closed {
            progress: sink.drain(),
            children: sink
                .below(&[])
                .filter(|(path, _)| path.len() == 1)
                .map(|(_, entry)| entry.record.clone())
                .collect(),
        }
    }
}

/// What [`ProgressScope::watch`] hands back. Dropping it stops the watch.
#[derive(Debug)]
pub struct ProgressWatch {
    sink: Arc<ProgressSink>,
    path: Arc<[u32]>,
    rx: Receiver<PendingProgress>,
}

impl ProgressWatch {
    pub fn updates(&self) -> &Receiver<PendingProgress> {
        &self.rx
    }
}

impl Drop for ProgressWatch {
    fn drop(&mut self) {
        self.sink.lock().watchers.remove(&*self.path);
    }
}

#[derive(Debug, Default)]
struct ProgressSink(Mutex<Sink>);

impl ProgressSink {
    fn lock(&self) -> MutexGuard<'_, Sink> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A record leaves `children` once its parent settles, moved into the
/// parent's data, so the map holds only what may still change.
#[derive(Debug, Default)]
struct Sink {
    pending: BTreeMap<Vec<u32>, Pending>,
    children: BTreeMap<Vec<u32>, ChildEntry>,
    /// By the path of the call each watches.
    watchers: BTreeMap<Vec<u32>, Sender<PendingProgress>>,
    closed: bool,
}

#[derive(Debug, Default)]
struct Pending {
    append: String,
    state: Option<Value>,
    child: Option<ToolChild>,
}

/// A nested call's invocation index under its parent.
fn child_index(path: &[u32]) -> u32 {
    path.last().copied().unwrap_or_default()
}

#[derive(Debug)]
struct ChildEntry {
    record: ToolChild,
    /// Where the start annotations end and the live ones begin.
    start_annotations: usize,
}

#[derive(Debug, Default)]
pub struct PendingProgress {
    pub path: Vec<u32>,
    /// Text streamed since the last take.
    pub append: String,
    /// The latest state, `None` while unchanged since the last take.
    pub state: Option<Value>,
    /// The nested call's latest record, `None` while unchanged.
    pub child: Option<ToolChild>,
}

/// What [`ProgressScope::close`] hands back.
#[derive(Debug, Default)]
pub struct Closed {
    pub progress: Vec<PendingProgress>,
    /// The model call's own children, for its data.
    pub children: Vec<ToolChild>,
}

impl Sink {
    fn pending(&mut self, path: &[u32]) -> &mut Pending {
        self.pending.entry(path.to_vec()).or_default()
    }

    /// After the end, or under a call that settled, nothing is recorded: its
    /// parent already holds its final children.
    fn rejects(&self, path: &[u32]) -> bool {
        self.closed
            || (1..=path.len()).any(|end| {
                self.children
                    .get(&path[..end])
                    .is_some_and(|entry| entry.record.status != ChildStatus::Running)
            })
    }

    /// Sends the change at {path} to each call above it, or at it, that is
    /// watched, by the path below that call.
    fn tell(&self, path: &[u32], change: impl Fn(Vec<u32>) -> PendingProgress) {
        if self.watchers.is_empty() {
            return;
        }
        for end in 0..=path.len() {
            if let Some(watcher) = self.watchers.get(&path[..end]) {
                let _ = watcher.send(change(path[end..].to_vec()));
            }
        }
    }

    fn stream(&mut self, path: &[u32], text: &str) {
        if text.is_empty() || self.rejects(path) {
            return;
        }
        self.pending(path).append.push_str(text);
        self.tell(path, |path| PendingProgress {
            path,
            append: text.to_owned(),
            ..PendingProgress::default()
        });
    }

    fn set_state(&mut self, path: &[u32], state: Value) {
        if self.rejects(path) {
            return;
        }
        self.tell(path, |path| PendingProgress {
            path,
            state: Some(state.clone()),
            ..PendingProgress::default()
        });
        self.pending(path).state = Some(state);
    }

    fn start(&mut self, path: &[u32], data: &ToolData) {
        if self.rejects(path) {
            return;
        }
        self.insert_running(path, data.clone());
        self.queue(path);
    }

    fn live(&mut self, path: &[u32], event: &ToolLive) {
        if self.rejects(path) {
            return;
        }
        let Some(record) = self.children.get_mut(path).map(|entry| &mut entry.record) else {
            return;
        };
        match event {
            ToolLive::Annotation(annotation) => record.data.annotations.push(annotation.clone()),
            ToolLive::AllowedBy(plugin) => record
                .data
                .annotations
                .push(format!("{ALLOWED_BY} {plugin}")),
            ToolLive::Usage(usage) => record.usage = Some(usage.clone()),
            ToolLive::Movable(_) | ToolLive::Task(_) => return,
        }
        self.queue(path);
    }

    fn finish(&mut self, path: &[u32], done: &mut ToolDoneEvent) {
        if self.rejects(path) {
            return;
        }
        let Some(data) = done.data.as_mut() else {
            return;
        };
        self.cancel_running_below(path);
        let data = Arc::make_mut(data);
        data.children = self.take_children(path);
        let (at, live, usage) = match self.children.get_mut(path) {
            Some(entry) => (
                entry.start_annotations,
                entry
                    .record
                    .data
                    .annotations
                    .split_off(entry.start_annotations),
                entry.record.usage.take(),
            ),
            None => (0, Vec::new(), None),
        };
        let at = at.min(data.annotations.len());
        data.annotations.splice(at..at, live);
        let record = ToolChild {
            index: child_index(path),
            status: if done.is_error {
                ChildStatus::Error
            } else {
                ChildStatus::Done
            },
            data: data.clone(),
            output: Some(Arc::clone(&done.output)),
            usage,
        };
        self.children.insert(
            path.to_vec(),
            ChildEntry {
                record,
                start_annotations: 0,
            },
        );
        self.queue(path);
    }

    fn abandon(&mut self, path: &[u32], fallback: impl FnOnce() -> ToolData) {
        if self.rejects(path) {
            return;
        }
        if !self.children.contains_key(path) {
            self.insert_running(path, fallback());
        }
        self.cancel_running_below(path);
        self.cancel(path);
    }

    fn close(&mut self) -> Closed {
        if self.closed {
            return Closed::default();
        }
        self.cancel_running_below(&[]);
        let children = self.take_children(&[]);
        self.closed = true;
        Closed {
            progress: self.drain(),
            children,
        }
    }

    fn drain(&mut self) -> Vec<PendingProgress> {
        mem::take(&mut self.pending)
            .into_iter()
            .map(|(path, pending)| PendingProgress {
                path,
                append: pending.append,
                state: pending.state,
                child: pending.child,
            })
            .filter(|p| !p.append.is_empty() || p.state.is_some() || p.child.is_some())
            .collect()
    }

    fn insert_running(&mut self, path: &[u32], data: ToolData) {
        let entry = ChildEntry {
            start_annotations: data.annotations.len(),
            record: ToolChild {
                index: child_index(path),
                status: ChildStatus::Running,
                data,
                output: None,
                usage: None,
            },
        };
        self.children.insert(path.to_vec(), entry);
    }

    fn queue(&mut self, path: &[u32]) {
        let record = self.children.get(path).map(|entry| entry.record.clone());
        if let Some(record) = &record {
            self.tell(path, |path| PendingProgress {
                path,
                child: Some(record.clone()),
                ..PendingProgress::default()
            });
        }
        self.pending(path).child = record;
    }

    /// Every call under `path` in path order: a parent before its children.
    fn below<'a>(
        &'a self,
        path: &'a [u32],
    ) -> impl Iterator<Item = (&'a Vec<u32>, &'a ChildEntry)> {
        self.children
            .range::<[u32], _>((Bound::Excluded(path), Bound::Unbounded))
            .take_while(move |(key, _)| key.starts_with(path))
    }

    /// Moves every record under `path` out of the map and hands back the
    /// direct children. A deeper one is already in its own parent's data,
    /// since a child settles only after everything under it.
    fn take_children(&mut self, path: &[u32]) -> Vec<ToolChild> {
        let keys: Vec<Vec<u32>> = self.below(path).map(|(key, _)| key.clone()).collect();
        keys.into_iter()
            .filter_map(|key| {
                let entry = self.children.remove(&key)?;
                (key.len() == path.len() + 1).then_some(entry.record)
            })
            .collect()
    }

    fn cancel(&mut self, path: &[u32]) {
        let children = self.take_children(path);
        if let Some(entry) = self.children.get_mut(path) {
            entry.record.status = ChildStatus::Cancelled;
            entry.record.data.children = children;
        }
        self.queue(path);
    }

    /// Deepest first, so a cancelled call's children are settled before it
    /// takes them.
    fn cancel_running_below(&mut self, path: &[u32]) {
        let running: Vec<Vec<u32>> = self
            .below(path)
            .filter(|(_, entry)| entry.record.status == ChildStatus::Running)
            .map(|(key, _)| key.clone())
            .collect();
        for key in running.iter().rev() {
            self.cancel(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const FIRST: &str = "one ";
    const SECOND: &str = "two";
    const SUPERSEDED: &str = "superseded";
    const TOOL: &str = "probe";
    const START_ANNOTATION: &str = "start";
    const LIVE_ANNOTATION: &str = "live";
    const DONE_ANNOTATION: &str = "done";
    const PLUGIN: &str = "gate";
    const USAGE: &str = "1k↑";
    const OUTPUT: &str = "out";

    fn data() -> ToolData {
        ToolData::new(Arc::from(TOOL), 1, json!({}), TOOL.to_owned())
    }

    fn done(is_error: bool) -> ToolDoneEvent {
        ToolDoneEvent {
            is_error,
            data: Some(Arc::new(data())),
            ..ToolDoneEvent::error(String::new(), OUTPUT)
        }
    }

    fn paths(progress: &[PendingProgress]) -> Vec<&[u32]> {
        progress.iter().map(|p| p.path.as_slice()).collect()
    }

    fn statuses(children: &[ToolChild]) -> Vec<ChildStatus> {
        children.iter().map(|child| child.status).collect()
    }

    #[test_case(&[FIRST, SECOND], Some(json!({"n": 2})) ; "text_and_state")]
    #[test_case(&[FIRST, SECOND], None ; "text_only")]
    #[test_case(&[], Some(json!(null)) ; "null_state_counts_as_set")]
    fn take_drains_text_and_state(texts: &[&str], state: Option<Value>) {
        let root = ProgressScope::default();
        for text in texts {
            root.stream(text);
        }
        if let Some(state) = state.clone() {
            root.set_state(json!(SUPERSEDED));
            root.set_state(state);
        }
        let taken = root.take();
        assert_eq!(paths(&taken), [&[] as &[u32]]);
        assert_eq!(taken[0].append, texts.concat());
        assert_eq!(taken[0].state, state);
        assert!(root.take().is_empty());
    }

    #[test_case(&[] ; "untouched")]
    #[test_case(&[""] ; "empty_text")]
    fn take_without_progress_is_empty(texts: &[&str]) {
        let root = ProgressScope::default();
        for text in texts {
            root.stream(text);
        }
        assert!(root.take().is_empty());
    }

    #[test]
    fn take_keeps_each_path_apart_root_first() {
        let root = ProgressScope::default();
        let first = root.child();
        let second = root.child();
        let grandchild = first.child();
        second.stream(SECOND);
        grandchild.stream(FIRST);
        root.stream(FIRST);
        first.set_state(json!(1));

        let taken = root.take();

        assert_eq!(paths(&taken), [&[][..], &[0], &[0, 0], &[1]]);
        assert_eq!(
            taken.iter().map(|p| p.append.as_str()).collect::<Vec<_>>(),
            [FIRST, "", FIRST, SECOND]
        );
        assert_eq!(taken[1].state, Some(json!(1)));
    }

    #[test]
    fn child_records_its_start_live_updates_and_end() {
        let root = ProgressScope::default();
        let child = root.child();
        let mut start = data();
        start.annotations.push(START_ANNOTATION.to_owned());
        child.started(&start);
        let running = root.take().remove(0).child.expect("start is queued");
        assert_eq!(running.status, ChildStatus::Running);

        child.live(&ToolLive::Annotation(LIVE_ANNOTATION.to_owned()));
        child.live(&ToolLive::AllowedBy(Arc::from(PLUGIN)));
        child.live(&ToolLive::Usage(USAGE.to_owned()));
        let updated = root.take().remove(0).child.expect("updates are queued");
        assert_eq!(updated.usage.as_deref(), Some(USAGE));

        let mut result = done(false);
        if let Some(data) = result.data.as_mut() {
            Arc::make_mut(data)
                .annotations
                .extend([START_ANNOTATION.to_owned(), DONE_ANNOTATION.to_owned()]);
        }
        child.finished(&mut result);

        let allowed = format!("{ALLOWED_BY} {PLUGIN}");
        let expected = [START_ANNOTATION, LIVE_ANNOTATION, &allowed, DONE_ANNOTATION];
        assert_eq!(result.data.expect("has data").annotations, expected);
        let taken = root.take();
        assert_eq!(paths(&taken), [&[0][..]]);
        let record = taken[0].child.as_ref().expect("the end is queued");
        assert_eq!(record.status, ChildStatus::Done);
        assert_eq!(record.data.annotations, expected);
        assert_eq!(record.usage.as_deref(), Some(USAGE));
        assert_eq!(
            record
                .output
                .as_ref()
                .map(|output| output.as_text())
                .as_deref(),
            Some(OUTPUT)
        );
    }

    #[test_case(false, ChildStatus::Done  ; "done")]
    #[test_case(true,  ChildStatus::Error ; "error")]
    fn a_child_that_never_started_still_gets_a_record(is_error: bool, expected: ChildStatus) {
        let root = ProgressScope::default();
        root.child().finished(&mut done(is_error));
        assert_eq!(statuses(&root.close().children), [expected]);
    }

    #[test]
    fn a_finished_child_carries_its_own_children() {
        let root = ProgressScope::default();
        let child = root.child();
        let grandchild = child.child();
        child.started(&data());
        grandchild.started(&data());
        grandchild.finished(&mut done(false));
        let mut result = done(false);
        child.finished(&mut result);

        assert_eq!(
            statuses(&result.data.expect("has data").children),
            [ChildStatus::Done]
        );
        let closed = root.close();
        assert_eq!(statuses(&closed.children), [ChildStatus::Done]);
        assert_eq!(
            statuses(&closed.children[0].data.children),
            [ChildStatus::Done]
        );
    }

    #[test]
    fn close_cancels_running_calls_and_drops_later_writes() {
        let root = ProgressScope::default();
        let done_child = root.child();
        let running = root.child();
        let running_grandchild = running.child();
        done_child.started(&data());
        done_child.finished(&mut done(false));
        running.started(&data());
        running_grandchild.started(&data());
        root.stream(FIRST);
        root.take();

        let closed = root.close();

        assert_eq!(
            statuses(&closed.children),
            [ChildStatus::Done, ChildStatus::Cancelled]
        );
        assert_eq!(
            statuses(&closed.children[1].data.children),
            [ChildStatus::Cancelled]
        );
        assert_eq!(paths(&closed.progress), [&[1][..], &[1, 0]]);
        assert!(closed.progress.iter().all(|p| {
            p.child
                .as_ref()
                .is_some_and(|child| child.status == ChildStatus::Cancelled)
        }));

        root.stream(SECOND);
        running.set_state(json!(SUPERSEDED));
        running.finished(&mut done(false));
        assert!(root.take().is_empty());
        assert!(root.close().children.is_empty());
    }

    #[test]
    fn a_child_ended_after_its_parent_stays_cancelled() {
        let root = ProgressScope::default();
        let child = root.child();
        let grandchild = child.child();
        child.started(&data());
        grandchild.started(&data());
        child.finished(&mut done(false));
        grandchild.finished(&mut done(false));

        let closed = root.close();
        assert_eq!(
            statuses(&closed.children[0].data.children),
            [ChildStatus::Cancelled]
        );
    }

    #[test]
    fn a_root_scope_records_nothing() {
        let root = ProgressScope::default();
        root.started(&data());
        root.live(&ToolLive::Usage(USAGE.to_owned()));
        let mut result = done(false);
        root.finished(&mut result);
        assert!(root.take().is_empty());
        assert!(result.data.expect("has data").children.is_empty());
    }

    const FALLBACK_TOOL: &str = "dropped";

    fn fallback() -> ToolData {
        ToolData::new(
            Arc::from(FALLBACK_TOOL),
            1,
            json!({}),
            FALLBACK_TOOL.to_owned(),
        )
    }

    /// A `ctx` kept past its call, or a task it spawned, can no longer change
    /// what its parent already took.
    #[test_case(false ; "done")]
    #[test_case(true  ; "error")]
    fn a_settled_child_takes_no_more_writes(is_error: bool) {
        let root = ProgressScope::default();
        let child = root.child();
        child.started(&data());
        child.finished(&mut done(is_error));
        root.take();

        child.stream(FIRST);
        child.set_state(json!(SUPERSEDED));
        child.live(&ToolLive::Usage(USAGE.to_owned()));
        let late = child.child();
        late.started(&data());
        late.stream(SECOND);
        child.child().finished(&mut done(false));
        child.child().abandoned(fallback);

        assert!(root.take().is_empty());
        let closed = root.close();
        assert!(closed.progress.is_empty());
        assert!(closed.children[0].data.children.is_empty());
        assert!(closed.children[0].usage.is_none());
    }

    #[test]
    fn an_abandoned_child_is_cancelled_with_everything_under_it() {
        let root = ProgressScope::default();
        let child = root.child();
        let settled = child.child();
        let running = child.child();
        child.started(&data());
        settled.started(&data());
        settled.finished(&mut done(false));
        running.started(&data());
        root.take();

        child.abandoned(fallback);

        let taken = root.take();
        assert_eq!(paths(&taken), [&[0][..], &[0, 1]]);
        let record = taken[0].child.as_ref().expect("the cancel is queued");
        assert_eq!(record.status, ChildStatus::Cancelled);
        assert_eq!(record.data.tool.as_ref(), TOOL);
        assert_eq!(
            statuses(&record.data.children),
            [ChildStatus::Done, ChildStatus::Cancelled]
        );
        assert_eq!(statuses(&root.close().children), [ChildStatus::Cancelled]);
    }

    fn indices(progress: &[PendingProgress]) -> Vec<u32> {
        progress
            .iter()
            .filter_map(|p| p.child.as_ref().map(|child| child.index))
            .collect()
    }

    /// Every record carries the last part of its path, whichever sibling
    /// sends first: start, live update, end, fallback and cancel alike.
    #[test]
    fn each_record_carries_its_invocation_index() {
        let root = ProgressScope::default();
        let first = root.child();
        let second = root.child();
        let third = root.child();
        let grandchild = second.child();
        second.started(&data());
        assert_eq!(indices(&root.take()), [1]);
        second.live(&ToolLive::Usage(USAGE.to_owned()));
        grandchild.started(&data());
        assert_eq!(indices(&root.take()), [1, 0]);
        second.finished(&mut done(false));
        let ended = root.take();
        assert_eq!(paths(&ended), [&[1][..], &[1, 0]]);
        assert_eq!(indices(&ended), [1, 0]);
        let record = ended[0].child.as_ref().expect("the end is queued");
        assert_eq!(indices_of(&record.data.children), [0]);
        third.abandoned(fallback);
        first.started(&data());
        assert_eq!(indices(&root.take()), [0, 2]);
        assert_eq!(indices_of(&root.close().children), [0, 1, 2]);
    }

    fn indices_of(children: &[ToolChild]) -> Vec<u32> {
        children.iter().map(|child| child.index).collect()
    }

    #[test]
    fn a_child_dropped_before_its_start_gets_the_fallback() {
        let root = ProgressScope::default();
        root.child().abandoned(fallback);
        let closed = root.close();
        assert_eq!(statuses(&closed.children), [ChildStatus::Cancelled]);
        assert_eq!(closed.children[0].data.tool.as_ref(), FALLBACK_TOOL);
    }

    #[test]
    fn abandoning_a_finished_child_changes_nothing() {
        let root = ProgressScope::default();
        let child = root.child();
        child.started(&data());
        child.finished(&mut done(false));
        root.take();
        child.abandoned(fallback);
        assert!(root.take().is_empty());
        assert_eq!(statuses(&root.close().children), [ChildStatus::Done]);
    }

    type Seen = (Vec<u32>, String, Option<Value>, Option<ChildStatus>);

    fn seen(path: &[u32], append: &str, state: Option<Value>, status: Option<ChildStatus>) -> Seen {
        (path.to_vec(), append.to_owned(), state, status)
    }

    /// A watch gets each change under its call as it is recorded, by the
    /// path below the call, a sibling's left out, and the stream still gets
    /// all of them coalesced.
    #[test]
    fn a_watch_sees_its_subtree_in_order_and_the_stream_keeps_it() {
        let root = ProgressScope::default();
        let sibling = root.child();
        let child = root.child();
        let watch = child.watch();
        let grandchild = child.child();
        child.started(&data());
        sibling.stream(SECOND);
        child.stream(FIRST);
        child.set_state(json!(1));
        grandchild.started(&data());
        grandchild.stream(SECOND);
        child.finished(&mut done(false));

        let updates: Vec<Seen> = watch
            .updates()
            .drain()
            .map(|p| (p.path, p.append, p.state, p.child.map(|c| c.status)))
            .collect();
        assert_eq!(
            updates,
            [
                seen(&[], "", None, Some(ChildStatus::Running)),
                seen(&[], FIRST, None, None),
                seen(&[], "", Some(json!(1)), None),
                seen(&[0], "", None, Some(ChildStatus::Running)),
                seen(&[0], SECOND, None, None),
                seen(&[0], "", None, Some(ChildStatus::Cancelled)),
                seen(&[], "", None, Some(ChildStatus::Done)),
            ]
        );
        assert_eq!(paths(&root.take()), [&[0][..], &[1], &[1, 0]]);
        drop(watch);
        assert!(root.sink.lock().watchers.is_empty());
    }

    /// A settled call's records move into its parent, and close moves the
    /// rest out, so a retained scope keeps no payload alive.
    #[test]
    fn records_leave_the_sink_once_taken() {
        let root = ProgressScope::default();
        let child = root.child();
        let grandchild = child.child();
        let running = root.child();
        child.started(&data());
        grandchild.started(&data());
        grandchild.finished(&mut done(false));
        child.finished(&mut done(false));
        running.started(&data());
        assert_eq!(
            root.sink.lock().children.keys().collect::<Vec<_>>(),
            [&vec![0], &vec![1]]
        );

        let closed = root.close();

        assert_eq!(closed.children.len(), 2);
        let sink = root.sink.lock();
        assert!(
            sink.children.is_empty() && sink.pending.is_empty(),
            "{sink:?}"
        );
    }
}
