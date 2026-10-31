//! Styled lines a tool or plugin draws, and the shared buffer that streams
//! them to the UI.

use std::any::Any;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use flume::{Receiver, Sender};
use serde::{Deserialize, Serialize};

/// Wakes the UI loop so a change made on another thread is painted now, not on
/// the loop's next timed poll. Wakes pile up into one until the loop looks, so
/// a plugin writing in a tight loop costs one frame, not one per write.
#[derive(Clone)]
pub struct UiWaker(Sender<()>);

impl UiWaker {
    pub fn new() -> (Self, Receiver<()>) {
        let (tx, rx) = flume::bounded(1);
        (Self(tx), rx)
    }

    pub fn wake(&self) {
        let _ = self.0.try_send(());
    }
}

/// Append-only buffer for streaming tool output to the UI. Writers append
/// under a Mutex, readers get a cheap Arc clone through a [`BufWatch`].
pub struct SharedBuf {
    committed: Mutex<BufferSnapshot>,
    version: AtomicU64,
    /// Only a buffer shown in a plugin window gets one, since the UI reads
    /// those on a tick. A tool body is repainted by the agent events that
    /// carry it.
    waker: OnceLock<UiWaker>,
    on_change: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Opaque click handler owned by the Lua layer. It lives on the buffer
    /// itself, not on any one handle, so every handle wrapping this buf,
    /// even a foreign wrapper in another task, reaches the same handler.
    click: Mutex<Option<Arc<dyn Any + Send + Sync>>>,
    notifying: AtomicBool,
}

impl SharedBuf {
    pub fn new() -> Self {
        Self {
            committed: Mutex::default(),
            version: AtomicU64::new(0),
            waker: OnceLock::new(),
            on_change: Mutex::new(None),
            click: Mutex::new(None),
            notifying: AtomicBool::new(false),
        }
    }

    pub fn set_click(&self, f: Arc<dyn Any + Send + Sync>) {
        *self.click.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    pub fn click(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.click.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn clear_click(&self) {
        *self.click.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Fires synchronously after every `append`/`set_lines`, on the
    /// mutating thread. The callback must not mutate this buffer; recursive
    /// notifications are silently dropped. One slot only: a second call
    /// replaces the previous watcher.
    pub fn set_on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.on_change.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    }

    /// A watcher keeps everything it captured alive for as long as it is
    /// installed, so owners must clear it once the watching task retires.
    pub fn clear_on_change(&self) {
        *self.on_change.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// There is one waker per process, so the first window to show the buffer
    /// sets it for good.
    pub fn wake_on_change(&self, waker: &UiWaker) {
        let _ = self.waker.set(waker.clone());
    }

    fn notify_change(&self) {
        if let Some(waker) = self.waker.get() {
            waker.wake();
        }
        if self.notifying.swap(true, Ordering::AcqRel) {
            return;
        }
        let cb = self
            .on_change
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(cb) = cb {
            cb();
        }
        self.notifying.store(false, Ordering::Release);
    }

    pub fn append(&self, line: SnapshotLine) {
        let mut guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        Arc::make_mut(&mut guard.lines).push(line);
        drop(guard);
        self.changed();
    }

    pub fn set_lines(&self, lines: Vec<SnapshotLine>) {
        self.set_lines_with_head(lines, 0);
    }

    /// See [`BufferSnapshot::head`].
    pub fn set_lines_with_head(&self, lines: Vec<SnapshotLine>, head: usize) {
        let mut guard = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        *guard = BufferSnapshot {
            lines: Arc::new(lines),
            head,
        };
        drop(guard);
        self.changed();
    }

    fn changed(&self) {
        self.version.fetch_add(1, Ordering::Release);
        self.notify_change();
    }

    pub fn len(&self) -> usize {
        self.committed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lines
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn read(&self) -> Arc<Vec<SnapshotLine>> {
        self.take().lines
    }

    pub fn take(&self) -> BufferSnapshot {
        self.committed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// One reader of a [`SharedBuf`]. A moved call streams into one buf that two
/// places draw, the row it left and its task tab. So every reader keeps its
/// own mark, and one reading never hides a change from the other.
pub struct BufWatch {
    buf: Arc<SharedBuf>,
    seen: u64,
}

impl BufWatch {
    pub fn new(buf: Arc<SharedBuf>) -> Self {
        Self { buf, seen: 0 }
    }

    pub fn buf(&self) -> &Arc<SharedBuf> {
        &self.buf
    }

    /// The buf's lines, if they changed since this watch last looked. The
    /// copy may already hold a later change, which the next look reads again.
    pub fn changed(&mut self) -> Option<BufferSnapshot> {
        let version = self.buf.version.load(Ordering::Acquire);
        (version != self.seen).then(|| {
            self.seen = version;
            self.buf.take()
        })
    }
}

impl Default for SharedBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SharedBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedBuf").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BufferSnapshot {
    pub lines: Arc<Vec<SnapshotLine>>,
    /// How many leading lines say what the buffer is about, like the command
    /// above a bash output. A tool row that moved to the background keeps
    /// only these, because the rest now streams in the task's tab.
    #[serde(default)]
    pub head: usize,
}

impl BufferSnapshot {
    pub fn from_arc(lines: Arc<Vec<SnapshotLine>>) -> Self {
        Self { lines, head: 0 }
    }

    /// The [`Self::head`] lines alone, or `None` when there are none.
    pub fn head_only(&self) -> Option<Self> {
        let head = self.head.min(self.lines.len());
        (head > 0).then(|| Self {
            lines: Arc::new(self.lines[..head].to_vec()),
            head,
        })
    }

    pub fn plain_text(text: String) -> Self {
        Self::from_arc(Arc::new(vec![SnapshotLine::plain(text)]))
    }

    pub fn first_line_text(&self) -> String {
        self.lines
            .first()
            .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect())
            .unwrap_or_default()
    }

    /// Search matches against this, so it must mirror exactly what the UI
    /// renders.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            for span in &line.spans {
                out.push_str(&span.text);
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotLine {
    pub spans: Vec<SnapshotSpan>,
}

impl SnapshotLine {
    pub fn plain(text: String) -> Self {
        Self {
            spans: vec![SnapshotSpan {
                text,
                style: SpanStyle::Default,
            }],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotSpan {
    pub text: String,
    pub style: SpanStyle,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum SpanStyle {
    #[default]
    Default,
    Named(String),
    Inline(InlineStyle),
}

/// A color a plugin can ask for. `Ansi` and `Default` keep terminal-owned
/// colors symbolic all the way to the renderer, so maki never has to guess
/// what a given index looks like on the user's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SpanColor {
    /// Serializes as `[r, g, b]`, matching sessions written before palette
    /// colors existed.
    Rgb((u8, u8, u8)),
    Ansi(u8),
    /// Serializes as the string `"default"`, the same spelling themes use.
    Default(DefaultColor),
}

/// Gives [`SpanColor::Default`] a distinct serialized shape under
/// `#[serde(untagged)]`, which has no room for a unit variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultColor {
    Default,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InlineStyle {
    pub fg: Option<SpanColor>,
    pub bg: Option<SpanColor>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    pub strikethrough: bool,
    pub reversed: bool,
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    fn line(text: &str) -> SnapshotLine {
        SnapshotLine {
            spans: vec![SnapshotSpan {
                text: text.into(),
                style: SpanStyle::Default,
            }],
        }
    }

    #[test]
    fn shared_buf_lifecycle() {
        let buf = Arc::new(SharedBuf::new());
        let mut watch = BufWatch::new(Arc::clone(&buf));

        assert!(buf.is_empty());
        assert!(watch.changed().is_none());

        for i in 0..3 {
            buf.append(line(&format!("l{i}")));
        }
        assert_eq!(buf.len(), 3);

        let snap = watch.changed().expect("dirty after appends");
        assert_eq!(snap.lines.len(), 3);
        assert_eq!(snap.lines[0].spans[0].text, "l0");
        assert!(watch.changed().is_none(), "clean after read");

        buf.append(line("l3"));
        let _ = buf.take();
        assert!(
            watch.changed().is_some(),
            "take must leave the flag for the UI"
        );
    }

    #[test]
    fn every_watch_sees_each_change_once() {
        let buf = Arc::new(SharedBuf::new());
        let mut row = BufWatch::new(Arc::clone(&buf));
        let mut tab = BufWatch::new(Arc::clone(&buf));
        assert!(row.changed().is_none(), "an untouched buf has nothing new");

        buf.append(line("a"));
        buf.append(line("b"));

        for watch in [&mut row, &mut tab] {
            assert_eq!(watch.changed().unwrap().text(), "a\nb");
            assert!(watch.changed().is_none());
        }
    }

    #[test]
    fn shared_buf_arc_snapshot_isolation() {
        let buf = Arc::new(SharedBuf::new());
        let mut watch = BufWatch::new(Arc::clone(&buf));
        buf.append(line("a"));
        buf.append(line("b"));
        let snap = watch.changed().unwrap();
        buf.append(line("c"));
        assert_eq!(snap.lines.len(), 2, "held Arc must not see new appends");
        let snap2 = watch.changed().unwrap();
        assert_eq!(snap2.lines.len(), 3);
    }

    #[test]
    fn shared_buf_poisoned_mutex_recovery() {
        let buf = Arc::new(SharedBuf::new());
        let buf2 = Arc::clone(&buf);
        let h = std::thread::spawn(move || {
            let _guard = buf2.committed.lock().unwrap();
            panic!("intentional poison");
        });
        let _ = h.join();
        buf.append(SnapshotLine { spans: vec![] });
    }

    /// `#[serde(untagged)]` exists purely so sessions written before palette
    /// colors still load, so the wire shapes are pinned rather than left to
    /// whatever the variant order happens to produce.
    #[test_case(SpanColor::Rgb((255, 0, 0)), "[255,0,0]" ; "rgb stays a triple")]
    #[test_case(SpanColor::Ansi(4), "4" ; "palette index is a bare number")]
    #[test_case(SpanColor::Default(DefaultColor::Default), "\"default\"" ; "terminal default is a string")]
    fn span_color_wire_format(color: SpanColor, expected: &str) {
        assert_eq!(serde_json::to_string(&color).unwrap(), expected);
        assert_eq!(
            serde_json::from_str::<SpanColor>(expected).unwrap(),
            color,
            "a stored session must read back as what wrote it"
        );
    }

    #[test]
    fn clear_on_change_stops_notifications() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let buf = Arc::new(SharedBuf::new());
        let fired = Arc::new(AtomicUsize::new(0));
        let f = Arc::clone(&fired);
        buf.set_on_change(move || {
            f.fetch_add(1, Ordering::SeqCst);
        });
        buf.append(SnapshotLine { spans: vec![] });
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        buf.clear_on_change();
        buf.append(SnapshotLine { spans: vec![] });
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    /// Search and the UI's error-dedup guard depend on this exact shape:
    /// spans join bare, lines join with one newline, no trailing newline.
    #[test]
    fn buffer_snapshot_text_joins_spans_and_lines() {
        let snap = BufferSnapshot::from_arc(Arc::new(vec![
            SnapshotLine {
                spans: vec![
                    SnapshotSpan {
                        text: "1 ".into(),
                        style: SpanStyle::Named("line_nr".into()),
                    },
                    SnapshotSpan {
                        text: "print('hi')".into(),
                        style: SpanStyle::Default,
                    },
                ],
            },
            SnapshotLine { spans: vec![] },
            SnapshotLine::plain("out".into()),
        ]));
        assert_eq!(snap.text(), "1 print('hi')\n\nout");
        assert_eq!(BufferSnapshot::from_arc(Arc::new(vec![])).text(), "");
    }
}
