//! The one portable form of a session: what `maki session export` prints,
//! `maki session import` reads, and `maki.session.export` / `new` hand to and
//! take from plugins. Every one of them goes through here, so a plugin and a
//! server can never disagree on what a session is.
//!
//! It carries conversation, never authority: no system prompt, no tools, no
//! rules. A fresh frame is built on the first run, and whatever the origin's
//! prompt stated that no longer holds (cwd, date) is told then.

use std::mem;
use std::sync::Arc;

use maki_storage::frame::{LiveFacts, PromptFacts, StoredFrame};
use maki_storage::sessions::StoredMode;
use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::message::{ContentBlock, HistorySnapshot, Message};
use crate::session::{SessionHeader, StoredSession};

/// Versions the envelope only. Messages inside it grow additively forever,
/// so nothing here moves with the session log format.
const FORMAT: u32 = 1;
const CONTENT_KEY: &str = "content";

pub const NEWER_FORMAT: &str = "exported by a newer maki";

/// A session as a document. Unknown top-level fields are ignored, which
/// means dropped: an importer keeps nothing it does not know. Everything but
/// `messages` may be left out, so a converter can hand over just those.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// Reading a higher one fails, since a newer envelope field that must
    /// not be ignored is exactly what bumps it.
    #[serde(default = "current_format", deserialize_with = "known_format")]
    pub format: u32,
    /// Stored as given and resolved when the session opens, like any resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<StoredMode>,
    /// Spelled like the thinking setting everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Always written on export. Left out on import, the host's default holds.
    #[serde(default)]
    pub fast: Option<bool>,
    /// Left out, the title is generated from the history as usual.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The date, working directory, model, instructions and hints the model
    /// was last told. Data, not authority: it only decides what the first run
    /// tells the model has moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<PromptFacts>,
    /// Skipped when empty only so [`SessionExport`] can write its shared
    /// messages under the same key. An empty document still parses.
    #[serde(
        default,
        deserialize_with = "located_messages",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub messages: Vec<Message>,
}

fn current_format() -> u32 {
    FORMAT
}

fn known_format<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let found = u32::deserialize(deserializer)?;
    if found > FORMAT {
        return Err(de::Error::custom(format!(
            "{NEWER_FORMAT} (format {found}, this build reads up to {FORMAT})"
        )));
    }
    Ok(found)
}

/// Where in `messages` something is wrong, as the path a reader of the
/// document would follow: `messages[12].content[2]`.
pub fn location(message: usize, block: Option<usize>) -> String {
    match block {
        Some(block) => format!("messages[{message}].{CONTENT_KEY}[{block}]"),
        None => format!("messages[{message}]"),
    }
}

/// A session to export, its messages still shared with whoever holds them,
/// so reading a long live one copies nothing. Serializes as the
/// [`Transcript`] document.
pub struct SessionExport {
    pub header: SessionHeader,
    pub messages: Arc<Vec<Message>>,
    pub frame: Option<Arc<StoredFrame>>,
}

impl SessionExport {
    pub fn stored(session: StoredSession) -> Self {
        Self {
            header: SessionHeader::of(&session),
            frame: session.frame().cloned(),
            messages: Arc::new(session.take_messages()),
        }
    }

    /// A live session, whose transcript and frame run ahead of what its
    /// stored copy holds.
    pub fn live(header: SessionHeader, snapshot: &HistorySnapshot) -> Self {
        Self {
            header,
            messages: Arc::clone(&snapshot.messages),
            frame: snapshot.frame.clone(),
        }
    }

    /// The document without `messages`, which stay in [`Self::messages`].
    pub fn envelope(&self) -> Transcript {
        let header = &self.header;
        Transcript {
            format: FORMAT,
            model: (!header.model.is_empty()).then(|| header.model.clone()),
            mode: header.mode,
            thinking: header.thinking.map(|thinking| thinking.to_string()),
            fast: Some(header.fast),
            title: Some(header.title.clone()),
            facts: (self.frame.as_deref())
                .and_then(|frame| facts_told(frame, &self.messages).prompt),
            messages: Vec::new(),
        }
    }
}

#[derive(Serialize)]
struct ExportDocument<'a> {
    #[serde(flatten)]
    envelope: Transcript,
    messages: &'a [Message],
}

impl Serialize for SessionExport {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ExportDocument {
            envelope: self.envelope(),
            messages: &self.messages,
        }
        .serialize(serializer)
    }
}

/// `History::told` for a transcript held anywhere, a stored or exported one
/// included.
pub fn facts_told(frame: &StoredFrame, messages: &[Message]) -> LiveFacts {
    let mut facts = frame.facts_at_start();
    for update in messages.iter().filter_map(Message::facts_update) {
        facts.apply(update);
    }
    facts
}

/// Parses each message, and each of its blocks, on its own, so a shape error
/// names the message and block it is in. Going through `Value` copies each
/// text string once more than a plain load does. Image data is not copied
/// again.
fn located_messages<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Message>, D::Error> {
    Vec::<Value>::deserialize(deserializer)?
        .into_iter()
        .enumerate()
        .map(|(index, raw)| parse_message(index, raw))
        .collect::<Result<_, _>>()
        .map_err(de::Error::custom)
}

fn parse_message(index: usize, mut raw: Value) -> Result<Message, String> {
    let blocks = match raw.get_mut(CONTENT_KEY) {
        Some(Value::Array(blocks)) => mem::take(blocks),
        _ => Vec::new(),
    };
    let mut message =
        Message::deserialize(raw).map_err(|e| format!("{}: {e}", location(index, None)))?;
    message.content = blocks
        .into_iter()
        .enumerate()
        .map(|(block, raw)| {
            <ContentBlock as Deserialize>::deserialize(raw)
                .map_err(|e| format!("{}: {e}", location(index, Some(block))))
        })
        .collect::<Result<_, _>>()?;
    Ok(message)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use maki_storage::frame::FactsUpdate;
    use maki_storage::sessions::StoredThinking;
    use serde_json::json;

    use super::*;
    use crate::message::Role;
    use crate::usage::TokenUsage;

    const CWD: &str = "/here";
    const ORIGIN_CWD: &str = "/origin";
    const MODEL: &str = "anthropic/claude-opus-5";
    const TITLE: &str = "moved session";
    const PROMPT: &str = "fix the bug";
    const ANSWER: &str = "done";
    const THINKING: &str = "high";
    const DATE: &str = "2026-10-02";
    const STALE_DATE: &str = "2026-09-30";
    const ORIGIN_SYSTEM: &str = "the origin's system prompt";
    const ORIGIN_FINGERPRINT: &str = "0123456789abcdef";
    const UNKNOWN_FIELD: &str = "origin_host";
    const UPDATE_TEXT: &str = "<context-update>Date is now 2026-10-02.</context-update>";
    /// What `maki session export` printed before the types left maki-agent.
    const EXPORT_DOCUMENT: &str = concat!(
        r#"{"format":1,"model":"anthropic/claude-opus-5","mode":"plan","thinking":"high","#,
        r#""fast":true,"title":"moved session","facts":{"date":"2026-10-02","cwd":"/origin","#,
        r#""model":"anthropic/claude-opus-5","instructions":"be nice","#,
        r#""hints":{"memory/after_instructions":"tags: a"}},"messages":["#,
        r#"{"role":"user","content":[{"type":"text","text":"fix the bug"}]},"#,
        r#"{"role":"user","content":[{"type":"text","#,
        r#""text":"<context-update>Date is now 2026-10-02.</context-update>"}],"#,
        r#""kind":"context_update","facts":{"date":"2026-10-02"}},"#,
        r#"{"role":"assistant","content":[{"type":"text","text":"done"}]}]}"#,
    );

    fn user(text: &str) -> Message {
        Message::user(text.into())
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

    fn export() -> SessionExport {
        let date_update = Message::context_update(
            UPDATE_TEXT.into(),
            FactsUpdate {
                date: Some(DATE.into()),
                ..FactsUpdate::default()
            },
        );
        let answer = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: ANSWER.into(),
            }],
            ..Default::default()
        };
        SessionExport {
            header: SessionHeader {
                cwd: ORIGIN_CWD.into(),
                model: MODEL.into(),
                title: TITLE.into(),
                mode: Some(StoredMode::Plan),
                thinking: Some(StoredThinking::parse_setting(THINKING).unwrap()),
                fast: true,
                usage: TokenUsage {
                    input: 1000,
                    output: 100,
                    ..TokenUsage::default()
                },
                context_size: 2000,
                updated_at: 1_790_000_000,
                working: true,
            },
            messages: Arc::new(vec![user(PROMPT), date_update, answer]),
            frame: Some(Arc::new(StoredFrame::new(
                ORIGIN_SYSTEM.into(),
                ORIGIN_FINGERPRINT.into(),
                Some(facts()),
                Vec::new(),
            ))),
        }
    }

    fn messages_only(messages: Vec<Message>) -> Transcript {
        Transcript {
            messages,
            ..Transcript::default()
        }
    }

    /// The document is public API: export writes it byte for byte, and a
    /// transcript read from it writes it again unchanged.
    #[test]
    fn export_document_round_trips_unchanged() {
        let document = serde_json::to_string(&export()).unwrap();
        assert_eq!(document, EXPORT_DOCUMENT);
        let read: Transcript = serde_json::from_str(&document).unwrap();
        assert_eq!(serde_json::to_string(&read).unwrap(), EXPORT_DOCUMENT);
    }

    #[test]
    fn newer_format_is_refused() {
        let document = json!({ "format": FORMAT + 1, "messages": [] });
        let error = serde_json::from_value::<Transcript>(document).unwrap_err();
        assert!(error.to_string().contains(NEWER_FORMAT), "{error}");
    }

    /// A converter may hand over just the messages, and an importer keeps
    /// nothing it does not know.
    #[test]
    fn messages_alone_are_a_transcript_and_unknown_fields_are_dropped() {
        let document = json!({
            "messages": [{ "role": "user", "content": [{ "type": "text", "text": PROMPT }] }],
            UNKNOWN_FIELD: { "host": CWD },
        });
        let read: Transcript = serde_json::from_value(document).unwrap();
        let expected = Transcript {
            format: FORMAT,
            ..messages_only(vec![user(PROMPT)])
        };
        assert_eq!(read, expected);
    }
}
