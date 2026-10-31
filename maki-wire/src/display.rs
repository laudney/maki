//! The client side of `translate`: turns the session updates an agent sends
//! into the `AgentEvent`s the TUI already draws. ACP carries less than the
//! local event stream, so this is a reduced inverse: it never invents token,
//! model or billing data the agent did not send.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agent_client_protocol_schema::MaybeUndefined;
use agent_client_protocol_schema::v1::{
    CompactionId, CompactionStatus, CompactionUpdate, ContentBlock, Meta, SessionUpdate,
    StopReason, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind, UsageUpdate,
};
use maki_types::event::{AgentEvent, ToolDoneEvent, ToolStartEvent, TurnCompleteEvent};
use maki_types::message::{Message, Role};
use maki_types::stop_reason::DoneReason;
use maki_types::tool::{DEFAULT_DATA_VERSION, ToolOutput};
use maki_types::usage::TokenUsage;
use serde_json::Value;
use tracing::warn;

use crate::commands;
use crate::extensions::Agreed;
use crate::format::{NOTICE_ID_PREFIX, unfenced};
use crate::tool_data;
use crate::usage::{self, SessionUsage};

pub const UNSUPPORTED_CONTENT: &str = "[unsupported content]";
pub const RENDER_FALLBACK: &str = "tool display fell back to plain text";
const UNNAMED_TOOL: &str = "tool";

#[derive(Default)]
pub struct DisplayMapper {
    calls: HashMap<String, Call>,
    context: Option<(u32, u32)>,
    /// Decodes the usage extension. On only when both sides agreed.
    usage: bool,
    /// Decodes the tool data extension. On only when both sides agreed.
    tool_data: bool,
    /// Calls whose tool data failed go on from their standard content for
    /// the whole session, with one notice.
    data_failed: HashSet<String>,
    /// The session usage of the last `UsageUpdate`, until taken.
    session_usage: Option<SessionUsage>,
    /// The run is a `/compact`, which the user asked for and the chat does
    /// not announce, as locally.
    manual_compaction: bool,
    /// Each compaction the agent reported, with the gauge it started at
    /// while it runs. An update is an upsert, so a repeat of one, even a
    /// late one for an older compaction, is not a new transition.
    compactions: HashMap<CompactionId, Option<u32>>,
}

/// ACP update fields are changes: an absent field keeps its last value, so
/// a status-only completion still carries the output streamed before it.
struct Call {
    /// `ToolStart` renames the row, so it carries the name the call was
    /// announced with rather than the summary title of a later update.
    name: Arc<str>,
    summary: Option<String>,
    raw_input: Option<Value>,
    content: Option<Vec<ToolCallContent>>,
    raw_output: Option<Value>,
}

impl Call {
    fn new(name: Arc<str>) -> Self {
        Self {
            name,
            summary: None,
            raw_input: None,
            content: None,
            raw_output: None,
        }
    }

    fn merge(&mut self, fields: &ToolCallUpdateFields) {
        if let Some(title) = &fields.title {
            self.summary = Some(title.clone());
        }
        if let Some(raw_input) = &fields.raw_input {
            self.raw_input = Some(raw_input.clone());
        }
        if let Some(content) = &fields.content {
            self.content = Some(content.clone());
        }
        if let Some(raw_output) = &fields.raw_output {
            self.raw_output = Some(raw_output.clone());
        }
    }

    fn start(&mut self, id: &str) -> AgentEvent {
        let start = ToolStartEvent {
            id: id.to_owned(),
            tool: Arc::clone(&self.name),
            summary: self.summary.clone().unwrap_or_default(),
            annotation: None,
            input: None,
            raw_input: self.raw_input.clone(),
            output: None,
            data_version: DEFAULT_DATA_VERSION,
        };
        AgentEvent::ToolStart(Box::new(start))
    }

    fn done(self, id: String, is_error: bool) -> AgentEvent {
        let content = self.content.unwrap_or_default();
        let diff = content.iter().find_map(|item| match item {
            ToolCallContent::Diff(diff) => Some(ToolOutput::Diff {
                path: diff.path.display().to_string(),
                before: diff.old_text.clone().unwrap_or_default(),
                after: diff.new_text.clone(),
                summary: String::new(),
            }),
            _ => None,
        });
        let output = diff.unwrap_or_else(|| {
            let text = content_text(&content);
            let text = match (text.is_empty(), self.raw_output) {
                (true, Some(Value::String(raw))) => raw,
                _ => text,
            };
            ToolOutput::Plain(text.into())
        });
        let done = ToolDoneEvent {
            id,
            tool: self.name,
            output: Arc::new(output),
            is_error,
            annotation: None,
            written_path: None,
            call: None,
            data: None,

            backgrounded: false,
        };
        AgentEvent::ToolDone(Box::new(done))
    }
}

impl DisplayMapper {
    pub fn new(agreed: &Agreed) -> Self {
        Self {
            usage: agreed.usage,
            tool_data: agreed.tool_data,
            ..Self::default()
        }
    }

    /// Forgets everything a session told it, for the next session on the
    /// same connection, which agreed on the same extensions.
    pub fn reset(&mut self) {
        *self = Self {
            usage: self.usage,
            tool_data: self.tool_data,
            ..Self::default()
        };
    }

    /// A run ended without `Done`, by an error, a cancel or a rebind. A call
    /// it left open must not take an update of the next run.
    pub fn end_run(&mut self) {
        self.calls.clear();
        self.manual_compaction = false;
    }

    /// The run about to start is a `/compact`: its start is not announced,
    /// and it ends as a compaction, not a turn.
    pub fn start_compaction(&mut self) {
        self.manual_compaction = true;
    }

    /// The session usage the last `UsageUpdate` carried, once.
    pub fn take_usage(&mut self) -> Option<SessionUsage> {
        self.session_usage.take()
    }

    /// The last context gauge the agent reported, used and window.
    pub fn context(&self) -> Option<(u32, u32)> {
        self.context
    }

    /// The name a live call was announced with, which a permission request
    /// for it can show instead of its title.
    pub fn tool_name(&self, id: &str) -> Option<Arc<str>> {
        self.calls.get(id).map(|call| Arc::clone(&call.name))
    }

    pub fn map(&mut self, update: SessionUpdate) -> Vec<AgentEvent> {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => vec![AgentEvent::TextDelta {
                text: chunk_text(chunk.content),
            }],
            SessionUpdate::AgentThoughtChunk(chunk) => vec![AgentEvent::ThinkingDelta {
                text: chunk_text(chunk.content),
            }],
            SessionUpdate::ToolCall(call) => self.tool_call(call),
            SessionUpdate::ToolCallUpdate(update) => self.tool_update(update),
            SessionUpdate::UsageUpdate(update) => self.usage_update(update),
            SessionUpdate::CompactionUpdate(update) => {
                self.compaction(update).into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    /// A failed or cancelled compaction is told by its run's end. The agent
    /// sends the gauge after the compaction before it completes it.
    fn compaction(&mut self, update: CompactionUpdate) -> Option<AgentEvent> {
        let (used, window) = self.context.unwrap_or_default();
        let running = self.compactions.get(&update.compaction_id).copied();
        match update.status {
            CompactionStatus::InProgress => {
                if running.is_some() {
                    return None;
                }
                let marked = matches!(&update.meta, MaybeUndefined::Value(meta) if commands::is_manual(meta));
                self.compactions.insert(update.compaction_id, Some(used));
                Some(AgentEvent::AutoCompacting {
                    context_size: used,
                    context_window: window,
                    manual: self.manual_compaction || marked,
                })
            }
            CompactionStatus::Completed => {
                if running == Some(None) {
                    return None;
                }
                self.compactions.insert(update.compaction_id, None);
                let summary = match update.summary {
                    MaybeUndefined::Value(blocks) => blocks.into_iter().map(chunk_text).collect(),
                    _ => String::new(),
                };
                Some(AgentEvent::CompactionDone {
                    context_size_before: running.flatten().unwrap_or(used),
                    context_size_after: used,
                    context_window: window,
                    summary,
                })
            }
            _ => {
                self.compactions.insert(update.compaction_id, None);
                None
            }
        }
    }

    fn usage_update(&mut self, update: UsageUpdate) -> Vec<AgentEvent> {
        let wire = self
            .usage
            .then(|| usage::read(update.meta.as_ref()))
            .flatten()
            .and_then(|wire| {
                wire.inspect_err(|error| warn!(%error, "ignoring the usage extension"))
                    .ok()
            });
        let session = SessionUsage::from_update(&update, wire.as_ref());
        self.context = Some((session.used, session.window));
        // The agent's own boundary: inferring it from the calls a turn
        // announced breaks on a retried stream, whose abandoned call never
        // finishes.
        let submitted = wire
            .as_ref()
            .is_some_and(|wire| wire.results_submitted)
            .then(results_submitted);
        let turn = wire.and_then(|wire| wire.turn).map(|turn| {
            AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
                message: Message {
                    role: Role::Assistant,
                    ..Message::default()
                },
                usage: turn.usage.into(),
                model: turn.model,
                cost: turn.usage.cost,
                subsidised_list_cost: turn.usage.subsidised_list_cost,
                subsidised_by: session.subsidised_by.as_deref().map(Arc::from),
                context_size: Some(session.used),
                context_window: session.window,
            }))
        });
        self.session_usage = Some(session);
        turn.into_iter().chain(submitted).collect()
    }

    /// The prompt response is what ends a run over ACP, so the caller builds
    /// `Done` from its stop reason once the response arrives. A call the
    /// turn left unfinished is forgotten with it.
    pub fn done(&mut self, stop_reason: StopReason) -> AgentEvent {
        let reason = match done_reason(stop_reason) {
            DoneReason::EndTurn if self.manual_compaction => DoneReason::Compact,
            reason => reason,
        };
        self.end_run();
        let (context_size, context_window) = self.context.unwrap_or_default();
        AgentEvent::Done {
            usage: TokenUsage::default(),
            cost: None,
            list_cost: None,
            context_size,
            context_window,
            num_turns: 0,
            reason,
        }
    }

    fn tool_call(&mut self, call: ToolCall) -> Vec<AgentEvent> {
        let id = call.tool_call_id.to_string();
        if id.starts_with(NOTICE_ID_PREFIX) {
            let notice = call
                .meta
                .as_ref()
                .and_then(|meta| meta.get("maki"))
                .and_then(|meta| meta.get("notice"));
            return vec![AgentEvent::ObservationLine {
                text: call.title,
                detail: notice
                    .and_then(|value| value.get("detail"))
                    .and_then(|value| value.as_str())
                    .map(str::to_owned),
                end: notice
                    .and_then(|value| value.get("end"))
                    .and_then(|value| serde_json::from_value(value.clone()).ok()),
            }];
        }
        let name: Arc<str> = call.name.as_deref().unwrap_or(&call.title).into();
        let mut events = vec![AgentEvent::ToolPending {
            id: id.clone(),
            name: name.to_string(),
        }];
        let mut fields = ToolCallUpdateFields::new()
            .title(call.title)
            .raw_input(call.raw_input)
            .raw_output(call.raw_output);
        if !call.content.is_empty() {
            fields = fields.content(call.content);
        }
        let mut state = Call::new(name);
        state.merge(&fields);
        if call.status != ToolCallStatus::Pending {
            self.calls.insert(id, state);
            fields.status = Some(call.status);
            let update = ToolCallUpdate::new(call.tool_call_id, fields).meta(call.meta);
            events.extend(self.tool_update(update));
            return events;
        }
        let (_, failure) = self.take_data(&id, call.meta.as_ref());
        self.calls.insert(id, state);
        events.extend(failure);
        events
    }

    fn tool_update(&mut self, update: ToolCallUpdate) -> Vec<AgentEvent> {
        let id = update.tool_call_id.to_string();
        let (data, failure) = self.take_data(&id, update.meta.as_ref());
        let mut events = self.apply_update(id, update, data);
        events.extend(failure);
        events
    }

    /// The tool data an update carries, `None` when the extension is off,
    /// this call's data failed already, or there is none. A failure comes
    /// back as its notice, and the call goes on without data.
    fn take_data(
        &mut self,
        id: &str,
        meta: Option<&Meta>,
    ) -> (Option<AgentEvent>, Option<AgentEvent>) {
        if !self.tool_data || self.data_failed.contains(id) {
            return (None, None);
        }
        match tool_data::read(meta, id) {
            Some(Ok(data)) => (Some(data), None),
            Some(Err(reason)) => {
                warn!(tool_call_id = %id, %reason, "ignoring the tool data extension");
                self.data_failed.insert(id.to_owned());
                (None, Some(fallback_notice(&reason)))
            }
            None => (None, None),
        }
    }

    fn apply_update(
        &mut self,
        id: String,
        update: ToolCallUpdate,
        data: Option<AgentEvent>,
    ) -> Vec<AgentEvent> {
        let fields = update.fields;
        if !has_standard_field(&fields) {
            return match data {
                Some(event) => vec![event],
                None => Vec::new(),
            };
        }
        let mut events = Vec::new();
        let call = self.calls.entry(id.clone()).or_insert_with(|| {
            let name = unknown_tool_name(&fields);
            events.push(AgentEvent::ToolPending {
                id: id.clone(),
                name: name.to_string(),
            });
            Call::new(name)
        });
        call.merge(&fields);
        match fields.status {
            Some(status @ (ToolCallStatus::Completed | ToolCallStatus::Failed)) => {
                let Some(call) = self.calls.remove(&id) else {
                    return events;
                };
                let is_error = status == ToolCallStatus::Failed;
                events.push(data.unwrap_or_else(|| call.done(id, is_error)));
            }
            status => {
                let started = status == Some(ToolCallStatus::InProgress);
                if started {
                    events.push(data.unwrap_or_else(|| call.start(&id)));
                }
                if let Some(content) = &fields.content {
                    events.push(AgentEvent::ToolOutput {
                        id: id.clone(),
                        content: content_text(content),
                    });
                }
            }
        }
        events
    }
}

fn has_standard_field(fields: &ToolCallUpdateFields) -> bool {
    fields.status.is_some()
        || fields.title.is_some()
        || fields.content.is_some()
        || fields.raw_input.is_some()
        || fields.raw_output.is_some()
        || fields.kind.is_some()
        || fields.name.is_some()
        || fields.locations.is_some()
}

fn fallback_notice(reason: &str) -> AgentEvent {
    AgentEvent::Notice {
        text: format!("{RENDER_FALLBACK}: {reason}"),
    }
}

/// An update for a call this client never saw announced, which a standard
/// agent may send, still needs a row with some name on it.
fn unknown_tool_name(fields: &ToolCallUpdateFields) -> Arc<str> {
    if let Some(name) = &fields.name {
        return name.as_str().into();
    }
    match fields.kind {
        Some(ToolKind::Other) | None => UNNAMED_TOOL.into(),
        Some(kind) => match serde_json::to_value(kind) {
            Ok(Value::String(kind)) => kind.into(),
            _ => UNNAMED_TOOL.into(),
        },
    }
}

fn chunk_text(block: ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text,
        _ => UNSUPPORTED_CONTENT.to_owned(),
    }
}

fn content_text(content: &[ToolCallContent]) -> String {
    let parts: Vec<&str> = content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Content(c) => match &c.content {
                ContentBlock::Text(text) => Some(unfenced(&text.text)),
                _ => None,
            },
            _ => None,
        })
        .collect();
    parts.join("\n")
}

fn done_reason(stop_reason: StopReason) -> DoneReason {
    match stop_reason {
        StopReason::MaxTokens => DoneReason::MaxTokens,
        StopReason::MaxTurnRequests => DoneReason::MaxTurns,
        StopReason::Cancelled => DoneReason::Cancelled,
        StopReason::Refusal => DoneReason::Dropped,
        _ => DoneReason::EndTurn,
    }
}

/// The tool results went back. The App only reads the boundary, not the
/// message, which the client never sees.
fn results_submitted() -> AgentEvent {
    AgentEvent::ToolResultsSubmitted {
        message: Box::new(Message::default()),
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::{ContentChunk, ImageContent, TextContent, UsageUpdate};
    use maki_storage::sessions::StoredTokenUsage;
    use test_case::test_case;

    use super::*;
    use crate::extensions::META_KEY;

    const CALL_ID: &str = "call-1";
    const TOOL: &str = "bash";
    const SUMMARY: &str = "ls -la";
    const OUTPUT: &str = "total 0\n```inner```\n\tend";
    const TEXT: &str = "hello";
    const USED: u64 = 1200;
    const SIZE: u64 = 200_000;
    fn mapped(mapper: &mut DisplayMapper, update: SessionUpdate) -> Vec<serde_json::Value> {
        mapper
            .map(update)
            .iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect()
    }

    fn json(event: AgentEvent) -> serde_json::Value {
        serde_json::to_value(event).unwrap()
    }

    /// A standard agent can announce a call that already finished, which the
    /// TUI can only draw if a row exists before the completion lands.
    #[test]
    fn completed_first_call_gets_a_row_first() {
        let mut mapper = DisplayMapper::default();
        let call = ToolCall::new(CALL_ID, TOOL)
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::from(OUTPUT)]);
        let events = mapper.map(SessionUpdate::ToolCall(call));
        let [
            AgentEvent::ToolPending { id, name },
            AgentEvent::ToolDone(done),
        ] = &events[..]
        else {
            panic!("expected pending then completion");
        };
        assert_eq!(id, CALL_ID);
        assert_eq!(&**name, TOOL);
        assert_eq!(done.output.as_text(), OUTPUT);
        assert!(!done.is_error);
    }

    fn event_types(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| json_type(&serde_json::to_value(event).unwrap()))
            .collect()
    }

    fn json_type(event: &serde_json::Value) -> String {
        event["type"].as_str().unwrap().to_owned()
    }

    #[test_case(Some(ToolKind::Read), Some(ToolCallStatus::InProgress), false, &["tool_pending", "tool_start"] ; "in_progress")]
    #[test_case(None, None, true, &["tool_pending", "tool_output"] ; "content_only")]
    #[test_case(None, Some(ToolCallStatus::Completed), true, &["tool_pending", "tool_done"] ; "completed")]
    #[test_case(None, Some(ToolCallStatus::Failed), false, &["tool_pending", "tool_done"] ; "failed")]
    #[test_case(None, Some(ToolCallStatus::InProgress), true, &["tool_pending", "tool_start", "tool_output"] ; "in_progress_with_content")]
    fn every_unknown_update_gets_a_row_then_all_fields(
        kind: Option<ToolKind>,
        status: Option<ToolCallStatus>,
        with_content: bool,
        expected: &[&str],
    ) {
        let mut mapper = DisplayMapper::default();
        let mut fields = ToolCallUpdateFields::new();
        fields.status = status;
        fields.kind = kind;
        if with_content {
            fields.content = Some(vec![ToolCallContent::from(OUTPUT)]);
        }
        let events = mapper.map(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            CALL_ID, fields,
        )));
        assert_eq!(event_types(&events), expected);
        let AgentEvent::ToolPending { name, .. } = &events[0] else {
            unreachable!()
        };
        assert_eq!(&**name, if kind.is_some() { "read" } else { UNNAMED_TOOL });
    }

    #[test]
    fn start_keeps_the_announced_name_and_takes_the_summary() {
        const NAME: &str = "read";
        const TITLE: &str = "Read a.rs";
        let mut mapper = DisplayMapper::default();
        mapper.map(SessionUpdate::ToolCall(
            ToolCall::new(CALL_ID, TITLE).name(NAME.to_owned()),
        ));
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::InProgress)
            .title(SUMMARY);
        let events = mapper.map(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            CALL_ID, fields,
        )));
        let [AgentEvent::ToolStart(start)] = events.as_slice() else {
            panic!("expected one ToolStart");
        };
        assert_eq!(&*start.tool, NAME);
        assert_eq!(start.summary, SUMMARY);
    }

    const RETITLED: &str = "retitled";

    /// A malformed extension leaves the standard ACP fields available.
    #[test]
    fn unreadable_tool_data_falls_back_to_the_standard_fields() {
        let mut mapper = DisplayMapper::new(&Agreed {
            tool_data: true,
            ..Agreed::default()
        });
        let told = |data: Value| {
            Meta::from_iter([(
                META_KEY.to_owned(),
                serde_json::json!({ tool_data::TOOL_DATA_KEY: data }),
            )])
        };
        let pending = ToolCall::new(CALL_ID, TOOL);
        let rejected = ToolCallUpdate::new(
            CALL_ID,
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::InProgress)
                .title(RETITLED),
        )
        .meta(told(Value::String(RETITLED.into())));

        mapper.map(SessionUpdate::ToolCall(pending));
        let events = mapper.map(SessionUpdate::ToolCallUpdate(rejected));

        let started = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolStart(start) => Some(start),
                _ => None,
            })
            .expect("the update starts the call");
        assert_eq!(
            (started.summary.as_str(), started.data_version),
            (RETITLED, DEFAULT_DATA_VERSION)
        );
    }

    const COMPACTION_ID: &str = "compaction-1";
    const LATER_COMPACTION_ID: &str = "compaction-2";
    const COMPACTION_SUMMARY: &str = "we renamed foo to bar";
    const USED_AFTER: u64 = 300;

    fn compaction(status: CompactionStatus) -> SessionUpdate {
        compaction_of(COMPACTION_ID, status)
    }

    fn compaction_of(id: &str, status: CompactionStatus) -> SessionUpdate {
        SessionUpdate::CompactionUpdate(CompactionUpdate::new(id.to_owned(), status))
    }

    /// Only the automatic compaction gets the chat line, as locally.
    #[test_case(false ; "automatic")]
    #[test_case(true ; "manual")]
    fn a_compaction_start_is_announced_only_when_automatic(manual: bool) {
        let mut mapper = DisplayMapper::default();
        mapper.map(SessionUpdate::UsageUpdate(UsageUpdate::new(USED, SIZE)));
        if manual {
            mapper.start_compaction();
        }
        let events = mapper.map(compaction(CompactionStatus::InProgress));
        assert!(
            matches!(&events[..], [AgentEvent::AutoCompacting { manual: marked, .. }] if *marked == manual),
            "the start of a /compact is not announced as automatic"
        );
    }

    /// A compaction queued into an ordinary run says it is manual on the
    /// wire, so it draws no automatic line though the run is no `/compact`.
    #[test]
    fn a_start_marked_manual_is_not_announced_as_automatic() {
        let mut mapper = DisplayMapper::default();
        let marked = CompactionUpdate::new(COMPACTION_ID, CompactionStatus::InProgress)
            .meta(commands::manual_meta());
        let events = mapper.map(SessionUpdate::CompactionUpdate(marked));
        assert!(matches!(
            &events[..],
            [AgentEvent::AutoCompacting { manual: true, .. }]
        ));
        let AgentEvent::Done { reason, .. } = mapper.done(StopReason::EndTurn) else {
            panic!("expected Done");
        };
        assert_eq!(reason, DoneReason::EndTurn, "the run is still a turn");
    }

    /// The agent sends the gauge after the compaction before it completes
    /// it, and a manual run ends as a compaction.
    #[test]
    fn a_completed_compaction_carries_its_sizes_and_summary() {
        let mut mapper = DisplayMapper::default();
        mapper.start_compaction();
        mapper.map(SessionUpdate::UsageUpdate(UsageUpdate::new(USED, SIZE)));
        mapper.map(compaction(CompactionStatus::InProgress));
        mapper.map(SessionUpdate::UsageUpdate(UsageUpdate::new(
            USED_AFTER, SIZE,
        )));
        let summary = vec![ContentBlock::Text(TextContent::new(COMPACTION_SUMMARY))];
        let events = mapper.map(SessionUpdate::CompactionUpdate(
            CompactionUpdate::new(COMPACTION_ID, CompactionStatus::Completed).summary(summary),
        ));

        assert_eq!(
            events.into_iter().map(json).collect::<Vec<_>>(),
            vec![json(AgentEvent::CompactionDone {
                context_size_before: USED as u32,
                context_size_after: USED_AFTER as u32,
                context_window: SIZE as u32,
                summary: COMPACTION_SUMMARY.into(),
            })]
        );
        let AgentEvent::Done { reason, .. } = mapper.done(StopReason::EndTurn) else {
            panic!("expected Done");
        };
        assert_eq!(reason, DoneReason::Compact);
    }

    /// An update is an upsert, so a repeat of the same state draws nothing,
    /// also a late one for an older compaction.
    #[test]
    fn a_repeated_compaction_update_is_not_a_new_transition() {
        let mut mapper = DisplayMapper::default();
        for (id, status, events) in [
            (COMPACTION_ID, CompactionStatus::InProgress, 1),
            (COMPACTION_ID, CompactionStatus::InProgress, 0),
            (COMPACTION_ID, CompactionStatus::Completed, 1),
            (LATER_COMPACTION_ID, CompactionStatus::InProgress, 1),
            (COMPACTION_ID, CompactionStatus::Completed, 0),
            (LATER_COMPACTION_ID, CompactionStatus::InProgress, 0),
            (LATER_COMPACTION_ID, CompactionStatus::Completed, 1),
        ] {
            assert_eq!(mapper.map(compaction_of(id, status)).len(), events);
        }
    }

    #[test_case(CompactionStatus::Failed ; "failed")]
    #[test_case(CompactionStatus::Cancelled ; "cancelled")]
    #[test_case(CompactionStatus::Other("_skipped".into()) ; "skipped")]
    fn a_compaction_that_did_not_complete_is_told_by_the_run(status: CompactionStatus) {
        let mut mapper = DisplayMapper::default();
        assert!(mapper.map(compaction(status)).is_empty());
    }

    #[test]
    fn raw_output_is_never_unfenced() {
        let fenced_raw = "```\nkept\n```";
        let mut mapper = DisplayMapper::default();
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(serde_json::Value::String(fenced_raw.into()));
        let events = mapper.map(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            CALL_ID, fields,
        )));
        let Some(AgentEvent::ToolDone(done)) = events.last() else {
            panic!("expected ToolDone");
        };
        assert_eq!(done.output.as_text(), fenced_raw);
    }

    #[test]
    fn non_text_chunk_is_marked_unsupported() {
        let mut mapper = DisplayMapper::default();
        let image = ContentBlock::Image(ImageContent::new("", "image/png"));
        let events = mapped(
            &mut mapper,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(image)),
        );
        assert_eq!(
            events,
            vec![json(AgentEvent::TextDelta {
                text: UNSUPPORTED_CONTENT.into(),
            })]
        );
    }

    #[test]
    fn text_and_thinking_chunks_map() {
        let mut mapper = DisplayMapper::default();
        let text = || ContentChunk::new(ContentBlock::Text(TextContent::new(TEXT)));
        assert_eq!(
            mapped(&mut mapper, SessionUpdate::AgentMessageChunk(text())),
            vec![json(AgentEvent::TextDelta { text: TEXT.into() })]
        );
        assert_eq!(
            mapped(&mut mapper, SessionUpdate::AgentThoughtChunk(text())),
            vec![json(AgentEvent::ThinkingDelta { text: TEXT.into() })]
        );
    }

    #[test_case(StopReason::EndTurn, DoneReason::EndTurn ; "end_turn")]
    #[test_case(StopReason::MaxTokens, DoneReason::MaxTokens ; "max_tokens")]
    #[test_case(StopReason::MaxTurnRequests, DoneReason::MaxTurns ; "max_turns")]
    #[test_case(StopReason::Cancelled, DoneReason::Cancelled ; "cancelled")]
    #[test_case(StopReason::Refusal, DoneReason::Dropped ; "refusal")]
    fn done_inverts_the_stop_reason(stop: StopReason, reason: DoneReason) {
        let mut mapper = DisplayMapper::default();
        mapper.map(SessionUpdate::UsageUpdate(UsageUpdate::new(USED, SIZE)));
        let AgentEvent::Done {
            reason: got,
            context_size,
            context_window,
            cost,
            ..
        } = mapper.done(stop)
        else {
            panic!("done must build a Done event");
        };
        assert_eq!(got, reason);
        assert_eq!((context_size, context_window), (USED as u32, SIZE as u32));
        assert_eq!(cost, None);
    }

    const TURN_MODEL: &str = "claude-sonnet-4";
    const TURN_COST: f64 = 0.25;

    fn usage_mapper() -> DisplayMapper {
        DisplayMapper::new(&Agreed {
            usage: true,
            ..Agreed::default()
        })
    }

    fn turn_usage() -> SessionUpdate {
        let wire = usage::WireUsage {
            turn: Some(usage::WireTurn {
                model: TURN_MODEL.into(),
                usage: StoredTokenUsage {
                    output: 7,
                    cost: Some(TURN_COST),
                    ..StoredTokenUsage::default()
                },
            }),
            ..usage::WireUsage::default()
        };
        usage::attach(
            SessionUpdate::UsageUpdate(UsageUpdate::new(USED, SIZE)),
            &wire,
        )
    }

    #[test]
    fn a_turn_usage_maps_to_a_turn_complete() {
        let mut mapper = usage_mapper();
        let events = mapper.map(turn_usage());
        let [AgentEvent::TurnComplete(turn)] = events.as_slice() else {
            panic!("expected the turn: {:?}", event_types(&events));
        };
        assert_eq!(turn.model, TURN_MODEL);
        assert_eq!(turn.usage.output, 7);
        assert_eq!(turn.cost, Some(TURN_COST));
        assert_eq!(
            (turn.context_size, turn.context_window),
            (Some(USED as u32), SIZE as u32)
        );
        let usage = mapper.take_usage().expect("the session usage is kept");
        assert_eq!(usage.used, USED as u32);
        assert!(mapper.take_usage().is_none(), "taken once");
    }

    #[test]
    fn without_the_extension_a_turn_usage_is_only_the_gauge() {
        let mut mapper = DisplayMapper::default();
        assert!(mapper.map(turn_usage()).is_empty());
        assert!(mapper.take_usage().is_some());
    }

    /// The agent's own boundary, so a retried stream's abandoned call cannot
    /// hold the line back or take it.
    #[test]
    fn the_results_boundary_maps_to_tool_results_submitted() {
        let mut mapper = usage_mapper();
        let wire = usage::WireUsage {
            results_submitted: true,
            ..usage::WireUsage::default()
        };
        let update = usage::attach(
            SessionUpdate::UsageUpdate(UsageUpdate::new(USED, SIZE)),
            &wire,
        );
        assert_eq!(event_types(&mapper.map(update)), ["tool_results_submitted"]);
        assert_eq!(event_types(&mapper.map(turn_usage())), ["turn_complete"]);
    }

    #[test]
    fn an_unreadable_extension_keeps_the_standard_gauge() {
        let mut mapper = usage_mapper();
        let meta = Meta::from_iter([(
            META_KEY.to_owned(),
            serde_json::json!({ usage::USAGE_KEY: { "turn": "not a turn" } }),
        )]);
        let update = UsageUpdate::new(USED, SIZE).meta(meta);
        assert!(mapper.map(SessionUpdate::UsageUpdate(update)).is_empty());
        assert_eq!(mapper.take_usage().unwrap().window, SIZE as u32);
    }
}
