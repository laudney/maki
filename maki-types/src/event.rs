//! What an agent run reports, and the stream that carries it to a host.

use std::ops::AddAssign;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use flume::{Receiver, Sender};
use maki_config::ToolKey;
use maki_storage::sessions::{QueueReceipt, add_cost};
use serde::{Deserialize, Serialize};
use strum::Display;
use thiserror::Error;

use crate::inbox::{InboxSnapshot, SubagentInbox};
use crate::message::{ContentBlock, ImageSource, Message, Role, TaskEnd};
use crate::snapshot::BufferSnapshot;
use crate::stop_reason::DoneReason;
use crate::thinking::RequestOptions;
use crate::tool::{ToolChild, ToolData, ToolInput, ToolOutput};
use crate::usage::TokenUsage;

/// One spelling for the mark, whether the UI draws it on a row or
/// `maki.agent.call_tool` hands it to a caller's `on_annotation`.
pub const ALLOWED_BY: &str = "allowed by";

const UNKNOWN_TOOL: &str = "unknown";

fn read_tool_key<'de, D: serde::Deserializer<'de>>(reader: D) -> Result<ToolKey, D::Error> {
    ToolKey::parse(&String::deserialize(reader)?).map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolStartEvent {
    pub id: String,
    pub tool: Arc<str>,
    /// The data version of the tool, so a renderer can be chosen before the
    /// result exists.
    pub data_version: u32,
    pub summary: String,
    pub annotation: Option<String>,
    pub input: Option<ToolInput>,
    pub raw_input: Option<serde_json::Value>,
    pub output: Option<ToolOutput>,
}

fn present_value<'de, D: serde::Deserializer<'de>>(
    reader: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(reader).map(Some)
}

/// Live progress a running call's handler published since the previous
/// event, coalesced by dispatch. `ToolDone` supersedes all of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolProgressEvent {
    pub id: String,
    /// The nested call this is about, by invocation index under each parent;
    /// empty for the call `id` names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<u32>,
    /// Text streamed since the previous event, empty when only the state
    /// changed.
    pub append: String,
    /// The latest live state, `None` while unchanged since the previous event.
    #[serde(
        default,
        deserialize_with = "present_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub state: Option<serde_json::Value>,
    /// The nested call's whole record, sent each time it changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child: Option<Box<ToolChild>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDoneEvent {
    pub id: String,
    pub tool: Arc<str>,
    pub output: Arc<ToolOutput>,
    pub is_error: bool,
    pub annotation: Option<String>,
    pub written_path: Option<String>,
    /// Only dispatch fills this and `data`, so an event made up anywhere else
    /// (a doom-loop refusal, a restored transcript) has neither.
    #[serde(skip)]
    pub call: Option<Box<CallRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Arc<ToolData>>,
    #[serde(default)]
    pub backgrounded: bool,
}

#[derive(Debug, Clone)]
pub struct CallRecord {
    /// After every input hook had its say.
    pub input: serde_json::Value,
    pub duration: Duration,
}

impl ToolDoneEvent {
    pub fn error(id: String, message: impl Into<String>) -> Self {
        let message: String = message.into();
        Self {
            id,
            tool: Arc::from(UNKNOWN_TOOL),
            output: Arc::new(ToolOutput::Plain(message.into())),
            is_error: true,
            annotation: None,
            written_path: None,
            call: None,
            data: None,
            backgrounded: false,
        }
    }

    pub fn ran_for(&self) -> Duration {
        self.call
            .as_ref()
            .map_or(Duration::ZERO, |call| call.duration)
    }

    pub fn written_path(&self) -> Option<&str> {
        if self.is_error {
            return None;
        }
        self.written_path
            .as_deref()
            .or_else(|| self.output.written_path())
    }

    pub fn wrote_to(&self, plan_path: &Path) -> bool {
        self.written_path()
            .is_some_and(|wp| Path::new(wp) == plan_path)
    }
}

pub fn tool_results(results: Vec<ToolDoneEvent>) -> Message {
    let mut content = Vec::with_capacity(results.len());
    let mut images = Vec::new();
    let mut receipts = Vec::new();
    for r in results {
        if r.backgrounded {
            receipts.push(r.id.clone());
        }
        content.push(ContentBlock::ToolResult {
            tool_use_id: r.id,
            content: r.output.as_text(),
            is_error: r.is_error,
            loaded_tools: r.output.loaded_tools().to_vec(),
        });
        if let ToolOutput::Image { source, .. } = r.output.as_ref() {
            images.push(ContentBlock::Image {
                source: source.clone(),
            });
        }
    }
    // Anthropic wants every tool_result before other content in the user
    // message, so images go after all results.
    content.extend(images);
    Message {
        role: Role::User,
        content,
        receipts,
        ..Default::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteerKind {
    MessageRewritten,
    MessageDropped,
    /// `agent.stop` kept the run going after the model ended its turn.
    Continued,
}

/// Why a session ended, as `SessionEnd` handlers see it in `data.reason`.
/// `Shutdown`, `Reload`, `Replaced`, and `Completed` tear the host down: no
/// UI is left to talk to and every handler shares one grace period.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SessionEndReason {
    /// `/reset` cleared the transcript.
    Reset,
    /// Another session was loaded into this tab.
    Load,
    /// The tab was closed.
    Delete,
    /// The process is exiting.
    Shutdown,
    /// `/reload` is rebuilding the plugin host. The session carries on in the
    /// next generation, so a handler cleaning up for good wants `Shutdown`.
    Reload,
    /// An ACP client started or loaded a session over this one.
    Replaced,
    /// A headless run finished.
    Completed,
}

/// One option a remote agent offered for a permission request. Only the
/// agent knows what it grants, so the client shows `label` and answers `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionChoice {
    pub id: String,
    pub label: String,
    pub allow: bool,
}

/// Spelled the way `maki.task.list()` spells a task's status, so a part reads
/// like any other task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartStatus {
    Working,
    Done,
    Error,
    Cancelled,
}

/// A piece of a call, like the `bash` child of a `batch`, that gets its own
/// task under the call once the call moves. A subagent child already has a
/// task of its own, so it is never a part.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPart {
    /// Stable across reports, or the part opens a new tab each time.
    pub id: String,
    pub name: String,
    pub tool: Arc<str>,
    pub status: PartStatus,
    pub body: Option<BufferSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    PluginTask {
        plugin: Arc<str>,
        envelope: Box<Envelope>,
    },
    ContextReset {
        context_size: u32,
        context_window: u32,
    },
    TaskQueue {
        id: String,
        snapshot: InboxSnapshot,
    },
    QueueUpdated(QueueReceipt),
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolPending {
        id: String,
        name: String,
    },
    ToolStart(Box<ToolStartEvent>),
    ToolProgress(Box<ToolProgressEvent>),
    /// `content` is the **full accumulated output** so far, not a delta.
    /// Producers must accumulate into a growing buffer and send the whole thing each flush.
    ToolOutput {
        id: String,
        content: String,
    },
    ToolDone(Box<ToolDoneEvent>),
    ToolMovable {
        id: String,
        movable: bool,
    },
    ToolParts {
        id: String,
        parts: Vec<ToolPart>,
    },
    TaskEnded {
        id: String,
        end: TaskEnd,
    },
    TurnComplete(Box<TurnCompleteEvent>),
    ToolResultsSubmitted {
        message: Box<Message>,
    },
    QueueItemConsumed {
        text: String,
        images: Vec<ImageSource>,
    },
    QueueDrained,
    Done {
        usage: TokenUsage,
        /// Billed cost for the whole run, `None` while nothing was priced.
        cost: Option<f64>,
        /// What the run would have billed at the provider's published list
        /// price, un-subsidised. Equals `cost` on an ordinary metered run and
        /// stands beside a `$0` one, so a budget plugin can charge against
        /// either. `None` only when no model in the run had a price table.
        list_cost: Option<f64>,
        context_size: u32,
        context_window: u32,
        num_turns: u32,
        reason: DoneReason,
    },
    /// A compaction starts. `manual` when the user asked for it, so the
    /// automatic line and plugin event stay with automatic ones.
    AutoCompacting {
        context_size: u32,
        context_window: u32,
        #[serde(default)]
        manual: bool,
    },
    CompactionDone {
        context_size_before: u32,
        context_size_after: u32,
        context_window: u32,
        /// So a plugin can check the summary kept what matters.
        summary: String,
    },
    Retry {
        attempt: u32,
        message: String,
        delay_ms: u64,
    },
    Error {
        message: String,
        /// The provider rejected the credentials, so a re-login is what fixes it.
        auth: bool,
        #[serde(default, flatten)]
        totals: RunTotals,
    },
    PermissionRequest {
        id: String,
        #[serde(deserialize_with = "read_tool_key")]
        tool: ToolKey,
        scopes: Vec<String>,
        /// Why a plugin escalated this call to the user, if one did.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// The exact options a remote agent offered, in its order. Empty for
        /// a local ask, whose answers are the local permission rules.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        choices: Vec<PermissionChoice>,
    },
    /// A `permission.prompt` layer allowed a call that would have prompted.
    /// The prompt is where the user learns what the agent is about to do, so
    /// a frontend marks the row instead. `id` is the closest row the user can
    /// see: for a call inside `code_execution` that is the script's row.
    AllowedByPlugin {
        id: String,
        plugin: String,
    },
    AuthRequired,
    Nudge,
    /// One line for the user about something the host did that the transcript
    /// won't show, like telling the model the date changed or rebuilding the
    /// prompt.
    Notice {
        text: String,
    },
    /// The transcript line of an observation the loop just accepted, such as
    /// a background result. History keeps the observation, so a restore
    /// shows the same line.
    ObservationLine {
        text: String,
        #[serde(default)]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end: Option<TaskEnd>,
    },
    /// A plugin changed the run in a way the transcript alone would not show.
    /// `text` is the message as sent, the reason for a drop, or the message
    /// that kept the run going.
    Steered {
        kind: SteerKind,
        text: String,
    },
    SubagentHistory {
        tool_use_id: String,
        messages: Vec<Message>,
    },
    PromptProgress {
        processed: u32,
        total: u32,
        cache: u32,
    },
    /// End of a session's event stream. Emitted only by
    /// [`EventStreamGuard::drop`] and swallowed by [`SessionEvents::next`], so
    /// a consumer sees `None` and never this variant.
    StreamClosed,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TurnCompleteEvent {
    pub message: Message,
    pub usage: TokenUsage,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// What the same turn would have cost at the provider's published list
    /// price, `Some` only when the model is subsidised by a flat
    /// subscription and `cost` is therefore always `$0`. Narrower than the
    /// ledger's `list_cost`, which banks the list price on every model: this
    /// one exists to be shown beside a `$0` bill, so a metered turn has
    /// nothing to add. See `maki_providers::Model::subsidised_list_cost`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subsidised_list_cost: Option<f64>,
    /// The display name of the subscription behind `subsidised_list_cost`,
    /// so a frontend that cannot resolve the model can still name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subsidised_by: Option<Arc<str>>,
    /// Tokens the next request would carry. This is the one context number
    /// the host reports, so `Done` and the compaction trigger agree with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_size: Option<u32>,
    /// The model's context window, so consumers can gauge `context_size`
    /// against the ceiling without resolving the model.
    pub context_window: u32,
}

/// What one run spent, itself and everything it spawned.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunTotals {
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    /// Un-subsidised list price, banked for metered and subsidised models
    /// alike so the total is never a partial sum of whichever turns happened
    /// to be subsidised.
    pub list_cost: Option<f64>,
}

impl RunTotals {
    /// What `self` holds beyond `earlier`, a smaller total of the same spend.
    /// Every paid round carries tokens, so when none moved nothing was spent,
    /// whatever residue two float sums of the same rounds leave in the costs.
    pub fn since(&self, earlier: &Self) -> Self {
        let usage = TokenUsage {
            input: self.usage.input.saturating_sub(earlier.usage.input),
            output: self.usage.output.saturating_sub(earlier.usage.output),
            cache_creation: self
                .usage
                .cache_creation
                .saturating_sub(earlier.usage.cache_creation),
            cache_read: self
                .usage
                .cache_read
                .saturating_sub(earlier.usage.cache_read),
            cost: None,
        };
        if usage == TokenUsage::default() {
            return Self::default();
        }
        Self {
            usage,
            cost: cost_since(self.cost, earlier.cost),
            list_cost: cost_since(self.list_cost, earlier.list_cost),
        }
    }
}

impl AddAssign for RunTotals {
    fn add_assign(&mut self, rhs: Self) {
        self.usage += rhs.usage;
        add_cost(&mut self.cost, rhs.cost);
        add_cost(&mut self.list_cost, rhs.list_cost);
    }
}

fn cost_since(now: Option<f64>, earlier: Option<f64>) -> Option<f64> {
    now.map(|now| now - earlier.unwrap_or_default())
        .filter(|spent| *spent > 0.0)
}

/// The cost a frontend reports: runs that ended plus the one still streaming.
/// Turns land their cost as they go, but once a run ends its ledger is the
/// truth, since it also counts compaction and what work moved off an earlier
/// turn spent since, which no turn reported.
#[derive(Debug, Clone, Copy, Default)]
pub struct CostTally {
    settled: Option<f64>,
    running: Option<f64>,
}

impl CostTally {
    pub fn starting_at(cost: Option<f64>) -> Self {
        Self {
            settled: cost,
            running: None,
        }
    }

    pub fn turn(&mut self, cost: Option<f64>) {
        add_cost(&mut self.running, cost);
    }

    /// The run ended: `ledger` is the cost its `Done` or `Error` carries.
    pub fn settle(&mut self, ledger: Option<f64>) {
        self.running = None;
        add_cost(&mut self.settled, ledger);
    }

    pub fn total(&self) -> Option<f64> {
        let mut total = self.settled;
        add_cost(&mut total, self.running);
        total
    }
}

/// The reader of an event stream is gone.
#[derive(Debug, Error)]
#[error("channel send failed")]
pub struct ChannelClosed;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentInfo {
    pub parent_tool_use_id: String,
    #[serde(rename = "parent_name")]
    pub name: String,
    #[serde(rename = "parent_prompt", skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "parent_model", skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What the subagent actually runs with, already reconciled against its
    /// model. `None` means unknown (a restore predating it), which reads as
    /// the parent's settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opts: Option<RequestOptions>,
    #[serde(skip)]
    pub answer_tx: Option<Sender<String>>,
    /// Where a host queues messages for this subagent. Its loop drains the
    /// queue between turns, so a message lands as a user interrupt.
    #[serde(skip)]
    pub inbox: Option<Arc<SubagentInbox>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_call: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EventSender {
    tx: Sender<Envelope>,
    run_id: u64,
}

impl EventSender {
    pub fn new(tx: Sender<Envelope>, run_id: u64) -> Self {
        Self { tx, run_id }
    }

    pub fn send(&self, event: impl Into<AgentEvent>) -> Result<(), ChannelClosed> {
        self.tx
            .try_send(Envelope {
                event: event.into(),
                subagent: None,
                run_id: self.run_id,
            })
            .map_err(|_| ChannelClosed)
    }

    pub fn send_envelope(&self, envelope: Envelope) -> Result<(), ChannelClosed> {
        self.tx.try_send(envelope).map_err(|_| ChannelClosed)
    }

    pub fn try_send(&self, event: impl Into<AgentEvent>) {
        let _ = self.tx.try_send(Envelope {
            event: event.into(),
            subagent: None,
            run_id: self.run_id,
        });
    }

    pub fn run_id(&self) -> u64 {
        self.run_id
    }

    /// Same stream, different run. Lets a session body stamp per-turn ids
    /// without keeping the [`EventStreamGuard`] in scope.
    pub fn with_run_id(&self, run_id: u64) -> Self {
        Self {
            tx: self.tx.clone(),
            run_id,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(flatten)]
    pub event: AgentEvent,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentInfo>,
    pub run_id: u64,
}

/// The only way to create a session event stream.
///
/// Never key a loop off sender disconnect instead: Lua tool contexts retain
/// [`EventSender`] clones until the VM garbage-collects them, which never
/// happens on an idle VM.
pub fn event_stream() -> (EventStreamGuard, SessionEvents) {
    let (tx, rx) = flume::unbounded();
    (EventStreamGuard { tx }, SessionEvents { rx, closed: false })
}

/// Dropping this ends the stream, and it is the only thing that does. Handing
/// out [`EventSender`]s is free, none of them extend it.
///
/// Nothing here bounds *when* the drop happens, that is the owner's job. An
/// owner that parks the guard in a struct (the Lua subagent session does) is
/// promising that the struct dies on a path it controls, not on a garbage
/// collector's schedule.
#[derive(Debug)]
pub struct EventStreamGuard {
    tx: Sender<Envelope>,
}

impl EventStreamGuard {
    pub fn sender(&self, run_id: u64) -> EventSender {
        EventSender::new(self.tx.clone(), run_id)
    }
}

impl Drop for EventStreamGuard {
    fn drop(&mut self) {
        let _ = self.tx.try_send(Envelope {
            event: AgentEvent::StreamClosed,
            subagent: None,
            run_id: 0,
        });
    }
}

/// The single reader of a session's stream. Not `Clone`: two readers would
/// split the terminal item and one of them would wait forever.
#[derive(Debug)]
pub struct SessionEvents {
    rx: Receiver<Envelope>,
    /// Kept instead of dropping `rx`, so a retained [`EventSender`] still
    /// reports success: the stream ends because the marker said so, never
    /// because a sender happened to notice a dead channel.
    closed: bool,
}

impl SessionEvents {
    /// `None` once the stream closed, forever after. The marker rides the same
    /// FIFO as the events, so everything sent before the guard dropped is
    /// delivered first and everything sent after it is lost. That is why a
    /// session drops its guard only once the run returned and history landed.
    ///
    /// Cancel-safe: the only suspension point is flume's `recv_async`, which
    /// leaves a queued envelope in the queue when dropped.
    pub async fn next(&mut self) -> Option<Envelope> {
        if self.closed {
            return None;
        }
        match self.rx.recv_async().await {
            Ok(envelope) if !matches!(envelope.event, AgentEvent::StreamClosed) => Some(envelope),
            _ => {
                self.closed = true;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ImageMediaType;
    use futures_lite::future::{block_on, poll_once};
    use std::pin::pin;
    use test_case::test_case;

    #[test]
    fn tool_results_builds_message_with_tool_result_blocks_and_receipts() {
        let msg = tool_results(vec![
            ToolDoneEvent {
                call: None,
                id: "t1".into(),
                tool: Arc::from("bash"),
                output: Arc::new(ToolOutput::Plain("ok".into())),
                is_error: false,
                annotation: None,
                written_path: None,
                data: None,
                backgrounded: true,
            },
            ToolDoneEvent {
                call: None,
                id: "t2".into(),
                tool: Arc::from("read"),
                output: Arc::new(ToolOutput::Plain("fail".into())),
                is_error: true,
                annotation: None,
                written_path: None,
                data: None,
                backgrounded: false,
            },
        ]);
        assert!(matches!(msg.role, Role::User));
        assert_eq!(msg.content.len(), 2);
        assert_eq!(msg.receipts, ["t1"]);
        assert!(
            matches!(&msg.content[0], ContentBlock::ToolResult { tool_use_id, is_error, .. } if tool_use_id == "t1" && !is_error)
        );
        assert!(
            matches!(&msg.content[1], ContentBlock::ToolResult { tool_use_id, is_error, .. } if tool_use_id == "t2" && *is_error)
        );
    }

    #[test]
    fn tool_results_appends_images_after_all_results() {
        let image = |data: &str| ToolOutput::Image {
            source: ImageSource::new(ImageMediaType::Png, Arc::from(data)),
            text: "[image: pic.png 1KB]".into(),
        };
        let done = |id: &str, output: ToolOutput| ToolDoneEvent {
            call: None,
            id: id.into(),
            tool: Arc::from("t"),
            output: Arc::new(output),
            is_error: false,
            annotation: None,
            written_path: None,
            data: None,
            backgrounded: false,
        };

        let msg = tool_results(vec![
            done("t1", image("aGVsbG8=")),
            done("t2", ToolOutput::Plain("ok".into())),
            done("t3", image("aW1n")),
        ]);
        assert_eq!(msg.content.len(), 5);
        assert!(
            matches!(&msg.content[0], ContentBlock::ToolResult { tool_use_id, content, .. } if tool_use_id == "t1" && content == "[image: pic.png 1KB]")
        );
        assert!(
            matches!(&msg.content[1], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t2")
        );
        assert!(
            matches!(&msg.content[2], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t3")
        );
        assert!(
            matches!(&msg.content[3], ContentBlock::Image { source } if &*source.data == "aGVsbG8=")
        );
        assert!(
            matches!(&msg.content[4], ContentBlock::Image { source } if &*source.data == "aW1n")
        );
    }

    #[test]
    fn wrote_to_checks_path_and_error_flag() {
        let ok_event = ToolDoneEvent {
            call: None,
            id: "id".into(),
            tool: Arc::from("write"),
            output: Arc::new(ToolOutput::Plain("wrote 10 bytes".into())),
            is_error: false,
            annotation: None,
            written_path: Some("/plans/slug.md".into()),
            data: None,
            backgrounded: false,
        };
        assert!(!ok_event.wrote_to(Path::new("/plans/other.md")));

        let err_event = ToolDoneEvent {
            call: None,
            is_error: true,
            ..ok_event
        };
        assert!(!err_event.wrote_to(Path::new("/plans/slug.md")));
    }

    #[test_case(
        ToolOutput::WriteCode { path: "/old/path".into(), byte_count: 10, lines: vec![] },
        Some("/new/path".into()), false, Some("/new/path")
        ; "prefers_field_over_output"
    )]
    #[test_case(
        ToolOutput::Diff { path: "/diff/path".into(), before: String::new(), after: String::new(), summary: String::new() },
        None, false, Some("/diff/path")
        ; "falls_back_to_output"
    )]
    #[test_case(
        ToolOutput::Plain("failed".into()), Some("/some/path".into()), true, None
        ; "none_when_error"
    )]
    fn tool_done_written_path(
        output: ToolOutput,
        written_path: Option<String>,
        is_error: bool,
        expected: Option<&str>,
    ) {
        let event = ToolDoneEvent {
            call: None,
            id: "id".into(),
            tool: Arc::from("tool"),
            output: Arc::new(output),
            is_error,
            annotation: None,
            written_path,
            data: None,
            backgrounded: false,
        };
        assert_eq!(event.written_path(), expected);
    }

    const STREAM_RUN_IDS: [u64; 3] = [1, 2, 3];
    const STILL_PENDING: &str = "an empty stream must not resolve `next()`";
    const NO_LATE_EVENTS: &str = "events queued after the marker must stay invisible";

    const FIRST_COST: f64 = 0.5;
    const SECOND_COST: f64 = 0.25;
    const FIRST_INPUT: u32 = 10;
    const SECOND_INPUT: u32 = 5;
    const FLOAT_RESIDUE: f64 = 1e-12;

    fn usage(input: u32) -> TokenUsage {
        TokenUsage {
            input,
            ..Default::default()
        }
    }

    #[test_case(Some(SECOND_COST), FIRST_COST + SECOND_COST ; "a_priced_ledger_replaces_its_turns")]
    #[test_case(None, FIRST_COST ; "an_unpriced_ledger_adds_nothing")]
    fn cost_tally_settles_a_run(ledger: Option<f64>, expected: f64) {
        let mut tally = CostTally::starting_at(Some(FIRST_COST));
        tally.turn(Some(FIRST_COST));
        assert_eq!(tally.total(), Some(FIRST_COST * 2.0));

        tally.settle(ledger);
        tally.settle(None);
        assert_eq!(tally.total(), Some(expected));
    }

    #[test_case(SECOND_INPUT, Some(SECOND_COST) ; "spent_rounds_are_the_difference")]
    #[test_case(0, None ; "no_tokens_means_no_spend_despite_cost_residue")]
    fn totals_since_an_earlier_reading(input: u32, expected_cost: Option<f64>) {
        let earlier = RunTotals {
            usage: usage(FIRST_INPUT),
            cost: Some(FIRST_COST),
            list_cost: None,
        };
        let now = RunTotals {
            usage: usage(FIRST_INPUT + input),
            cost: Some(FIRST_COST + expected_cost.unwrap_or(FLOAT_RESIDUE)),
            list_cost: None,
        };

        let spent = now.since(&earlier);
        assert_eq!(spent.usage.input, input);
        assert_eq!(spent.cost, expected_cost);
    }

    fn drain(events: &mut SessionEvents) -> Vec<u64> {
        block_on(async {
            let mut seen = Vec::new();
            while let Some(envelope) = events.next().await {
                seen.push(envelope.run_id);
            }
            seen
        })
    }

    /// The ordering the whole design rests on: the marker rides the same FIFO
    /// as the events, so nothing queued before it is lost. The first `next()`
    /// is polled on an empty queue and then dropped, the way a `select!` arm
    /// in the SDK pump does, and it must not swallow what lands afterwards.
    #[test]
    fn queued_events_arrive_in_order_before_the_close() {
        let (guard, mut events) = event_stream();
        block_on(async {
            let mut pending = pin!(events.next());
            assert!(
                poll_once(pending.as_mut()).await.is_none(),
                "{STILL_PENDING}"
            );
            for run_id in STREAM_RUN_IDS {
                guard.sender(run_id).send(AgentEvent::Nudge).unwrap();
            }
        });
        drop(guard);
        assert_eq!(drain(&mut events), STREAM_RUN_IDS);
    }

    /// The marker is a point of no return, even for a reader that has not
    /// polled yet. A Lua tool context parks a sender on an idle VM forever, so
    /// its late send has to look fine to it and stay invisible to the reader.
    #[test]
    fn events_sent_after_the_close_are_never_observed() {
        let (guard, mut events) = event_stream();
        let retained = guard.sender(STREAM_RUN_IDS[0]);
        retained.send(AgentEvent::Nudge).unwrap();
        drop(guard);
        retained.send(AgentEvent::Nudge).unwrap();
        assert_eq!(drain(&mut events), [STREAM_RUN_IDS[0]], "{NO_LATE_EVENTS}");
        assert!(block_on(events.next()).is_none(), "{NO_LATE_EVENTS}");
    }

    /// A consumer that exits first (SDK stdout closed, ACP client gone) leaves
    /// the guard sending the marker into a dead channel, often while unwinding.
    #[test]
    fn dropping_the_guard_after_the_reader_is_harmless() {
        let (guard, events) = event_stream();
        let retained = guard.sender(STREAM_RUN_IDS[0]);
        drop(events);
        drop(guard);
        assert!(matches!(
            retained.send(AgentEvent::Nudge),
            Err(ChannelClosed)
        ));
    }

    const PROGRESS_ID: &str = "t1";
    const PROGRESS_TEXT: &str = "chunk";

    /// An unchanged state and a state set to null must stay apart on the
    /// wire, so only the first is left out.
    #[test_case(None,                           r#"{"id":"t1","append":"chunk"}"#              ; "unchanged_state_is_omitted")]
    #[test_case(Some(serde_json::Value::Null),  r#"{"id":"t1","append":"chunk","state":null}"# ; "null_state_is_kept")]
    fn progress_state_serializes_apart_from_null(state: Option<serde_json::Value>, expected: &str) {
        let event = ToolProgressEvent {
            id: PROGRESS_ID.to_owned(),
            path: Vec::new(),
            append: PROGRESS_TEXT.to_owned(),
            state,
            child: None,
        };
        assert_eq!(serde_json::to_string(&event).unwrap(), expected);
    }
}
