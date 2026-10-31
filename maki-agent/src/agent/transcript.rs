//! Opening a [`Transcript`] as a session: what a resume repairs is repaired,
//! and the rest is refused.

use std::collections::HashSet;
use std::sync::Arc;

use maki_providers::{ContentBlock, Message, Role, TokenUsage};
use maki_storage::frame::StoredFrame;
use maki_storage::sessions::{StoredThinking, ThinkingParseError, normalize_title};
use maki_types::transcript::{Transcript, location};

use super::history::{sanitize_restored, strip_thinking};
use crate::session::StoredSession;

const NO_TOOL_RESULT: &str = "has no tool_result in the next user message";
const MISPLACED_BLOCK: &str = "block is not allowed in a message with role";
const DUPLICATE_TOOL_ID: &str = "duplicate tool id";
const MALFORMED_CONTEXT_UPDATE: &str =
    "a context_update must be a user message with one text block and no display_text";
const INVALID_THINKING: &str = "invalid thinking setting";

/// What resume cannot repair. Message shape errors (facts on the wrong
/// message) are caught earlier, by deserializing, and are located the same
/// way.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("{at}: tool_use {id:?} {NO_TOOL_RESULT}")]
    MissingToolResult { at: String, id: String },
    #[error("{at}: {block} {MISPLACED_BLOCK} {role}")]
    MisplacedBlock {
        at: String,
        block: &'static str,
        role: &'static str,
    },
    #[error("{at}: {DUPLICATE_TOOL_ID} {id:?}")]
    DuplicateToolId { at: String, id: String },
    #[error("{at}: {MALFORMED_CONTEXT_UPDATE}")]
    MalformedContextUpdate { at: String },
    /// A block a strict reader cannot take, though a load keeps it: one of
    /// a known type that does not parse, or one without a type.
    #[error("{at}: {detail}")]
    UnreadableBlock { at: String, detail: String },
    #[error("{INVALID_THINKING}: {0}")]
    Thinking(#[from] ThinkingParseError),
}

/// Lays the document over `base`, the blank session its host would open
/// (cwd, settings that outlive a turn), but does not store it. Whatever
/// the document leaves out keeps the base's value. Spend comes from
/// neither. A transcript carries no authority, and a host's standing
/// permissions belong to the sessions the user opened, so a document
/// with conversation drops the base's. One without is just a new
/// session, so it keeps them the way a new tab does.
///
/// Repairs exactly what a resume repairs and refuses the rest, so a plugin
/// bug fails loudly instead of changing what the model saw. Thinking goes:
/// it is bound to the origin's prefix, which no fresh frame can match.
pub fn transcript_into_session(
    transcript: Transcript,
    mut base: StoredSession,
) -> Result<StoredSession, TranscriptError> {
    let thinking = transcript
        .thinking
        .as_deref()
        .map(StoredThinking::parse_setting)
        .transpose()?;
    validate(&transcript.messages)?;
    let mut messages = transcript.messages;
    sanitize_restored(&mut messages);
    strip_thinking(&mut messages);

    if !messages.is_empty() {
        base.meta.yolo = None;
        base.meta.session_rules.clear();
    }
    base.replace_messages(messages);
    base.set_frame(
        transcript
            .facts
            .map(|facts| Arc::new(StoredFrame::facts_only(facts))),
    );
    if let Some(model) = transcript.model.filter(|model| !model.trim().is_empty()) {
        base.set_model(model);
    }
    let meta = &mut base.meta;
    meta.mode = transcript.mode.or(meta.mode);
    meta.thinking = thinking.or(meta.thinking);
    meta.fast = transcript.fast.unwrap_or(meta.fast);
    base.token_usage = TokenUsage::default();
    match transcript.title.map(|title| normalize_title(&title)) {
        Some(title) if !title.is_empty() => base.set_title(title),
        _ => base.update_title_if_default(),
    }
    Ok(base)
}

fn validate(messages: &[Message]) -> Result<(), TranscriptError> {
    let mut tool_uses = HashSet::new();
    let mut tool_results = HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        if message.is_context_update() && !message.is_host_context_update() {
            return Err(TranscriptError::MalformedContextUpdate {
                at: location(index, None),
            });
        }
        let next = messages.get(index + 1);
        for (block_index, block) in message.content.iter().enumerate() {
            let at = location(index, Some(block_index));
            if let Some(detail) = block.strict_error() {
                return Err(TranscriptError::UnreadableBlock { at, detail });
            }
            if required_role(block).is_some_and(|role| message.role != role) {
                return Err(TranscriptError::MisplacedBlock {
                    at,
                    block: block.into(),
                    role: (&message.role).into(),
                });
            }
            let (seen, id) = match block {
                ContentBlock::ToolUse { id, .. } => (&mut tool_uses, id),
                ContentBlock::ToolResult { tool_use_id, .. } => (&mut tool_results, tool_use_id),
                _ => continue,
            };
            if !seen.insert(id.as_str()) {
                return Err(TranscriptError::DuplicateToolId { at, id: id.clone() });
            }
            // A trailing call is closed like on resume.
            if matches!(block, ContentBlock::ToolUse { .. })
                && next.is_some_and(|next| !answers(next, id))
            {
                return Err(TranscriptError::MissingToolResult { at, id: id.clone() });
            }
        }
    }
    Ok(())
}

fn answers(message: &Message, id: &str) -> bool {
    message.role == Role::User
        && message.content.iter().any(
            |block| matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
        )
}

/// `None` for blocks either role may carry, unknown ones included.
fn required_role(block: &ContentBlock) -> Option<Role> {
    match block {
        ContentBlock::ToolUse { .. }
        | ContentBlock::Thinking { .. }
        | ContentBlock::RedactedThinking { .. } => Some(Role::Assistant),
        ContentBlock::ToolResult { .. } => Some(Role::User),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fmt::Display;

    use maki_providers::{ImageMediaType, ImageSource};
    use maki_storage::frame::{FactsUpdate, PromptFacts};
    use maki_storage::sessions::{StoredEffect, StoredMode, StoredRule};
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;
    use crate::agent::{History, SessionExport};

    const CWD: &str = "/here";
    const ORIGIN_CWD: &str = "/origin";
    const MODEL: &str = "anthropic/claude-opus-5";
    const UNKNOWN_MODEL: &str = "nowhere/model-from-2024";
    const TITLE: &str = "moved session";
    const MULTILINE_TITLE: &str = "moved\n  session";
    const LONG_TITLE: &str =
        "a title far longer than any picker row can show without cutting it somewhere";
    const PROMPT: &str = "fix the bug";
    const ANSWER: &str = "done";
    const THINKING: &str = "high";
    const OTHER_THINKING: &str = "off";
    const HOST_CWD: &str = "/host/project";
    const SIGNATURE: &str = "sig";
    const TOOL_ID: &str = "t1";
    const TOOL_NAME: &str = "read";
    const DATE: &str = "2026-10-02";
    const STALE_DATE: &str = "2026-09-30";
    const ORIGIN_SYSTEM: &str = "the origin's system prompt";
    const ORIGIN_FINGERPRINT: &str = "0123456789abcdef";
    const MCP_TOOL: &str = "srv__tool";
    const PLAN_PATH: &str = "/plans/origin.md";
    const UNKNOWN_BLOCK_TYPE: &str = "server_tool_use";
    const UPDATE_TEXT: &str = "<context-update>Date is now 2026-10-02.</context-update>";
    const NOT_CARRIED: &str = "an import must not carry what belongs to the origin machine";
    const PERMISSIONS_MSG: &str =
        "only a document without conversation keeps the tab's permissions";

    fn user(text: &str) -> Message {
        Message::user(text.into())
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        }
    }

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }

    fn thinking() -> ContentBlock {
        ContentBlock::Thinking {
            thinking: PROMPT.into(),
            signature: Some(SIGNATURE.into()),
        }
    }

    fn signed_call(id: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: TOOL_NAME.into(),
            input: json!({ "path": null }),
            thought_signature: Some(SIGNATURE.into()),
        }
    }

    fn call(id: &str) -> Message {
        assistant(vec![ContentBlock::tool_use(id, TOOL_NAME, json!({}))])
    }

    fn result(id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::tool_result(id, ANSWER, false)],
            display_text: Some(String::new()),
            ..Default::default()
        }
    }

    fn unknown_block() -> ContentBlock {
        ContentBlock::Unknown(json!({ "type": UNKNOWN_BLOCK_TYPE, "id": TOOL_ID }))
    }

    fn image() -> ContentBlock {
        ContentBlock::Image {
            source: ImageSource::new(ImageMediaType::Png, Arc::from("aGk=")),
        }
    }

    fn date_update(date: &str) -> Message {
        Message::context_update(
            UPDATE_TEXT.into(),
            FactsUpdate {
                date: Some(date.into()),
                ..FactsUpdate::default()
            },
        )
    }

    fn facts() -> PromptFacts {
        PromptFacts {
            date: STALE_DATE.into(),
            cwd: ORIGIN_CWD.into(),
            model: MODEL.into(),
            instructions: "be nice".into(),
            hints: BTreeMap::from([("memory/after_instructions".into(), "tags: a".into())]),
        }
    }

    fn conversation() -> Vec<Message> {
        vec![
            user(PROMPT),
            assistant(vec![thinking(), signed_call(TOOL_ID)]),
            Message {
                content: vec![
                    ContentBlock::tool_result(TOOL_ID, ANSWER, false),
                    image(),
                    unknown_block(),
                ],
                ..result(TOOL_ID)
            },
            date_update(DATE),
            assistant(vec![
                ContentBlock::RedactedThinking {
                    data: SIGNATURE.into(),
                },
                text(ANSWER),
                unknown_block(),
            ]),
        ]
    }

    /// A session the way the origin machine holds it, with everything that
    /// must stay behind set to something an import would notice.
    fn origin_session() -> StoredSession {
        let mut session = StoredSession::new(MODEL, ORIGIN_CWD);
        session.replace_messages(conversation());
        session.set_frame(Some(Arc::new(StoredFrame::new(
            ORIGIN_SYSTEM.into(),
            ORIGIN_FINGERPRINT.into(),
            Some(facts()),
            vec![json!({ "name": MCP_TOOL })],
        ))));
        session.set_title(TITLE.into());
        session.meta.mode = Some(StoredMode::Plan);
        session.meta.plan_path = Some(PLAN_PATH.into());
        session.meta.plan_written = true;
        session.meta.thinking = Some(StoredThinking::parse_setting(THINKING).unwrap());
        session.meta.fast = true;
        session.meta.workflow = true;
        session.meta.yolo = Some(true);
        session.meta.session_rules = rules();
        session.token_usage = TokenUsage {
            input: 1000,
            output: 100,
            ..TokenUsage::default()
        };
        session
    }

    fn rules() -> Vec<StoredRule> {
        vec![StoredRule {
            tool: TOOL_NAME.into(),
            scope: None,
            effect: StoredEffect::Allow,
        }]
    }

    fn blank() -> StoredSession {
        StoredSession::new("", CWD)
    }

    fn import(transcript: Transcript) -> StoredSession {
        transcript_into_session(transcript, blank()).unwrap()
    }

    /// A focused tab's blank session: its settings, and permissions a
    /// plugin-made session must not pick up.
    fn host_base() -> StoredSession {
        let mut base = StoredSession::new(MODEL, HOST_CWD);
        base.meta.mode = Some(StoredMode::Plan);
        base.meta.thinking = Some(StoredThinking::parse_setting(THINKING).unwrap());
        base.meta.fast = true;
        base.meta.workflow = true;
        base.meta.yolo = Some(true);
        base.meta.session_rules = rules();
        base
    }

    /// What `maki session export` prints, read back.
    fn exported(session: StoredSession) -> Transcript {
        let document = serde_json::to_value(SessionExport::stored(session)).unwrap();
        serde_json::from_value(document).unwrap()
    }

    fn messages_only(messages: Vec<Message>) -> Transcript {
        Transcript {
            messages,
            ..Transcript::default()
        }
    }

    #[track_caller]
    fn assert_refused_at(error: impl Display, at: &str, problem: &str) {
        let error = error.to_string();
        assert!(error.starts_with(&format!("{at}: ")), "{error}");
        assert!(error.contains(problem), "{error}");
    }

    /// Fork stays the trusted path. Export and import agree with it on
    /// everything but what a fresh frame cannot keep.
    #[test]
    fn round_trip_loses_only_thinking() {
        let original = exported(origin_session());
        let reexported = exported(import(original.clone()));

        let mut expected = original;
        strip_thinking(&mut expected.messages);
        assert_eq!(reexported, expected);
    }

    /// The CLI prints the shared form, so reading it back as a transcript
    /// and writing that again must give the same document.
    #[test]
    fn a_session_export_serializes_as_its_transcript() {
        let shared = serde_json::to_value(SessionExport::stored(origin_session())).unwrap();
        let owned = serde_json::to_value(exported(origin_session())).unwrap();
        assert_eq!(shared, owned);
    }

    /// The origin's prompt is not carried, only what it said, with the
    /// transcript's updates already applied.
    #[test]
    fn export_carries_what_the_model_last_held() {
        assert_eq!(
            exported(origin_session()).facts,
            Some(PromptFacts {
                date: DATE.into(),
                ..facts()
            })
        );
    }

    #[test_case(Some(facts()) ; "facts")]
    #[test_case(None ; "no_facts")]
    fn facts_become_a_frame_without_a_prefix(facts: Option<PromptFacts>) {
        let session = import(Transcript {
            facts: facts.clone(),
            ..messages_only(vec![user(PROMPT)])
        });
        assert_eq!(
            session.frame().map(Arc::as_ref),
            facts.map(StoredFrame::facts_only).as_ref()
        );
    }

    #[test_case(|s: &StoredSession| s.frame().unwrap().prefix().is_none() && s.frame().unwrap().mcp_tools.is_empty() ; "frame")]
    #[test_case(|s: &StoredSession| s.meta.session_rules.is_empty() ; "rules")]
    #[test_case(|s: &StoredSession| s.meta.yolo.is_none() ; "yolo")]
    #[test_case(|s: &StoredSession| !s.meta.workflow ; "workflow")]
    #[test_case(|s: &StoredSession| s.token_usage == TokenUsage::default() ; "usage")]
    #[test_case(|s: &StoredSession| s.meta.plan_path.is_none() && !s.meta.plan_written ; "plan_file")]
    #[test_case(|s: &StoredSession| !s.messages().iter().flat_map(|m| &m.content).any(ContentBlock::is_thinking) ; "thinking")]
    #[test_case(|s: &StoredSession| !s.messages().iter().flat_map(|m| &m.content).any(|b| matches!(b, ContentBlock::ToolUse { thought_signature: Some(_), .. })) ; "signatures")]
    fn not_carried(left_behind: fn(&StoredSession) -> bool) {
        let session = import(exported(origin_session()));
        assert!(left_behind(&session), "{NOT_CARRIED}");
    }

    #[test]
    fn settings_and_title_are_carried() {
        let session = import(exported(origin_session()));
        assert_eq!(session.title, TITLE);
        assert_eq!(session.cwd, CWD);
        assert_eq!(session.meta.mode, Some(StoredMode::Plan));
        assert_eq!(session.meta.thinking.unwrap().to_string(), THINKING);
        assert!(session.meta.fast);
    }

    #[test]
    fn what_the_document_leaves_out_comes_from_the_base_but_permissions() {
        let session =
            transcript_into_session(messages_only(vec![user(PROMPT)]), host_base()).unwrap();
        assert_eq!(session.cwd, HOST_CWD);
        assert_eq!(session.model, MODEL);
        assert_eq!(session.meta.mode, Some(StoredMode::Plan));
        assert_eq!(session.meta.thinking.unwrap().to_string(), THINKING);
        assert!(session.meta.fast);
        assert!(session.meta.workflow);
        assert!(session.meta.yolo.is_none(), "{NOT_CARRIED}");
        assert!(session.meta.session_rules.is_empty(), "{NOT_CARRIED}");
    }

    /// A blank document is a new session like a new tab, so the tab's
    /// standing permissions hold. One that repairs to nothing is blank too.
    #[test_case(vec![user(PROMPT)], false ; "conversation_drops_them")]
    #[test_case(Vec::new(), true ; "blank_keeps_them")]
    #[test_case(vec![Message { content: Vec::new(), ..user(PROMPT) }], true ; "emptied_by_repair_keeps_them")]
    fn permissions_stay_only_with_a_blank_document(messages: Vec<Message>, kept: bool) {
        let session = transcript_into_session(messages_only(messages), host_base()).unwrap();
        assert_eq!(session.meta.yolo.is_some(), kept, "{PERMISSIONS_MSG}");
        assert_eq!(
            !session.meta.session_rules.is_empty(),
            kept,
            "{PERMISSIONS_MSG}"
        );
    }

    /// A model this build does not know is stored as given, and resolved
    /// when the session opens like on any resume.
    #[test]
    fn document_settings_win_over_the_base() {
        let transcript = Transcript {
            model: Some(UNKNOWN_MODEL.into()),
            mode: Some(StoredMode::Build),
            thinking: Some(OTHER_THINKING.into()),
            fast: Some(false),
            ..messages_only(vec![user(PROMPT)])
        };
        let session = transcript_into_session(transcript, host_base()).unwrap();
        assert_eq!(session.cwd, HOST_CWD);
        assert_eq!(session.model, UNKNOWN_MODEL);
        assert_eq!(session.meta.mode, Some(StoredMode::Build));
        assert_eq!(session.meta.thinking.unwrap().to_string(), OTHER_THINKING);
        assert!(!session.meta.fast);
        assert!(session.meta.workflow);
    }

    /// A title given is kept the way `set_title` keeps one, on one line. A
    /// blank one is no title at all, so one is generated from the prompt.
    #[test_case(None, PROMPT ; "absent_is_generated")]
    #[test_case(Some(""), PROMPT ; "empty_is_generated")]
    #[test_case(Some(" \n\t "), PROMPT ; "blank_is_generated")]
    #[test_case(Some(MULTILINE_TITLE), TITLE ; "multiline_is_one_line")]
    #[test_case(Some(LONG_TITLE), LONG_TITLE ; "long_is_kept")]
    fn import_title_is_normalized(title: Option<&str>, expected: &str) {
        let session = import(Transcript {
            title: title.map(String::from),
            ..messages_only(vec![user(PROMPT)])
        });
        assert_eq!(session.title, expected);
    }

    #[test_case(None, MODEL ; "absent_inherits")]
    #[test_case(Some(""), MODEL ; "empty_inherits")]
    #[test_case(Some("  "), MODEL ; "blank_inherits")]
    #[test_case(Some(UNKNOWN_MODEL), UNKNOWN_MODEL ; "given_is_kept")]
    fn import_model_falls_back_to_the_base_when_blank(model: Option<&str>, expected: &str) {
        let transcript = Transcript {
            model: model.map(String::from),
            ..messages_only(vec![user(PROMPT)])
        };
        let session = transcript_into_session(transcript, host_base()).unwrap();
        assert_eq!(session.model, expected);
    }

    #[test_case(vec![user(PROMPT), call(TOOL_ID)] ; "pending_tool_use")]
    #[test_case(vec![user(PROMPT), result(TOOL_ID), user(PROMPT)] ; "orphan_result")]
    #[test_case(vec![user(PROMPT), call(TOOL_ID), Message { content: vec![ContentBlock::tool_result(TOOL_ID, ANSWER, false), ContentBlock::tool_result("orphan", ANSWER, false)], ..result(TOOL_ID) }] ; "partial_orphan")]
    fn sanitized_like_resume(messages: Vec<Message>) {
        let resumed = History::restored(messages.clone());
        let imported = import(messages_only(messages));
        assert_eq!(imported.messages(), resumed.as_slice());
    }

    #[test_case(vec![user(PROMPT), call(TOOL_ID), user(PROMPT), assistant(vec![text(ANSWER)])], NO_TOOL_RESULT, "messages[1].content[0]" ; "tool_use_without_result")]
    #[test_case(vec![user(PROMPT), call(TOOL_ID), call("t2"), result(TOOL_ID)], NO_TOOL_RESULT, "messages[1].content[0]" ; "result_not_in_the_next_message")]
    #[test_case(vec![Message { content: vec![ContentBlock::tool_use(TOOL_ID, TOOL_NAME, json!({}))], ..user(PROMPT) }], MISPLACED_BLOCK, "messages[0].content[0]" ; "tool_use_from_user")]
    #[test_case(vec![Message { content: vec![text(PROMPT), thinking()], ..user(PROMPT) }], MISPLACED_BLOCK, "messages[0].content[1]" ; "thinking_from_user")]
    #[test_case(vec![user(PROMPT), assistant(vec![ContentBlock::tool_result(TOOL_ID, ANSWER, false)])], MISPLACED_BLOCK, "messages[1].content[0]" ; "tool_result_from_assistant")]
    #[test_case(vec![user(PROMPT), call(TOOL_ID), result(TOOL_ID), call(TOOL_ID), result(TOOL_ID)], DUPLICATE_TOOL_ID, "messages[3].content[0]" ; "duplicate_tool_id")]
    #[test_case(vec![user(PROMPT), Message { role: Role::Assistant, ..date_update(DATE) }], MALFORMED_CONTEXT_UPDATE, "messages[1]" ; "context_update_from_assistant")]
    fn invalid_history_is_refused(messages: Vec<Message>, problem: &str, at: &str) {
        let error = transcript_into_session(messages_only(messages), blank()).unwrap_err();
        assert_refused_at(error, at, problem);
    }

    /// A newer build's block loads as unknown, and either role may carry it.
    #[test]
    fn unknown_block_types_are_imported_in_either_role() {
        let block = json!({ "type": UNKNOWN_BLOCK_TYPE, "id": TOOL_ID });
        let document = json!({ "messages": [
            { "role": "user", "content": [block] },
            { "role": "assistant", "content": [block] },
        ] });
        let session = import(serde_json::from_value(document).unwrap());

        let unknown = [ContentBlock::Unknown(block)];
        assert_eq!(session.messages()[0].content, unknown);
        assert_eq!(session.messages()[1].content, unknown);
    }

    #[test]
    fn bad_thinking_setting_is_refused() {
        let transcript = Transcript {
            thinking: Some(PROMPT.into()),
            ..messages_only(vec![user(PROMPT)])
        };
        let error = transcript_into_session(transcript, blank()).unwrap_err();
        assert!(error.to_string().starts_with(INVALID_THINKING));
    }

    /// What a message or block must hold is the provider types' rule, tested
    /// there. Here only that the refusal says where it is.
    #[test_case(json!({ "role": "user", "content": [], "kind": "context_update" }), "messages[0]" ; "malformed_message")]
    #[test_case(json!({ "role": "user", "content": [{ "type": "text", "text": PROMPT }, { "text": PROMPT }] }), "messages[0].content[1]" ; "unreadable_block")]
    fn a_refusal_names_where_it_is(message: Value, at: &str) {
        let document = json!({ "messages": [message] });
        let error = serde_json::from_value::<Transcript>(document)
            .map_err(|e| e.to_string())
            .and_then(|transcript| {
                (transcript_into_session(transcript, blank()).map(drop)).map_err(|e| e.to_string())
            })
            .unwrap_err();
        assert!(error.starts_with(&format!("{at}: ")), "{error}");
    }
}
