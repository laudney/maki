//! Messages and their content blocks, as a session stores them and every
//! provider reads them.
//!
//! `Message.display_text`: `Some("")` marks a message as synthetic (sent to the API but hidden
//! from the UI). `user_text()` returns `None` for these, so system-injected messages
//! (cancel markers, compaction prompts) stay invisible without a separate type.
//! `Message.kind` answers a different question. Synthetic text is ours and
//! trusted, it is just not worth showing. An observation comes from outside,
//! belongs in model context, and must never be mistaken for the user talking.

use std::borrow::Cow;
use std::fmt;
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwap;
use maki_storage::frame::FactsUpdate;
use maki_storage::intern;
use maki_storage::sessions::TitleSource;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use strum::{IntoStaticStr, VariantNames};
use tracing::{debug, warn};

const BLOCK_TYPE_KEY: &str = "type";
/// Every `type` the derived impl parses. Taken from the enum so a new variant
/// is never missing, minus `Unknown`, which is never parsed and so stays last.
const KNOWN_BLOCK_TYPES: &[&str] = match ContentBlock::VARIANTS.split_last() {
    Some((_unknown, known)) => known,
    None => &[],
};
pub const UNTYPED_BLOCK: &str = "a content block must be an object with a string `type`";
pub const CONTEXT_UPDATE_WITHOUT_FACTS: &str = "a context_update message must carry its facts";
/// Facts are only ever written beside a context update, so anywhere else
/// they are a fabricated or corrupted message, not something to drop.
pub const FACTS_WITHOUT_CONTEXT_UPDATE: &str = "only a context_update message carries facts";
pub const UNKNOWN_IMAGE_MEDIA_TYPE: &str = "unknown image media type";
/// Stands in for the text of a message that carries only images, both in model
/// context and in the transcript. One const so the two can never drift apart.
pub const IMAGE_PLACEHOLDER: &str = "[image]";
/// See [`Message::empty_marker`].
pub const EMPTY_RESPONSE_MARKER: &str = "(empty)";
/// Marks a definition the API keeps out of the model's context until a
/// `tool_reference` loads it (see `Model::supports_deferred_tools`).
pub const DEFER_LOADING_KEY: &str = "defer_loading";

/// What a provider has to do with an image's payload before it will take it.
/// Only the providers' image adapter reads or writes it.
#[doc(hidden)]
#[derive(Clone)]
pub enum WireVerdict {
    /// Already wire-safe.
    Keep,
    Replace(ImageSource),
    /// Undecodable here, so no provider will take it either.
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageMediaType {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageMediaType {
    pub const ALL: [Self; 4] = [Self::Png, Self::Jpeg, Self::Gif, Self::Webp];

    /// Single source of truth for media-type strings: serde, data URLs,
    /// wire formats, and the Lua bridge all go through here.
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    pub fn from_mime(mime: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.mime() == mime)
    }
}

impl Serialize for ImageMediaType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.mime())
    }
}

impl<'de> Deserialize<'de> for ImageMediaType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_mime(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("{UNKNOWN_IMAGE_MEDIA_TYPE} '{s}'")))
    }
}

#[derive(Clone)]
pub struct ImageSource {
    pub media_type: ImageMediaType,
    pub data: Arc<str>,
    /// What `adapt_images_for_model` has to do with `data` before a provider
    /// will take it, decided at most once. It describes the payload, so it
    /// rides with it: every clone shares one verdict, and it dies with the
    /// pixels instead of outliving them in a side table.
    verdict: Arc<OnceLock<WireVerdict>>,
}

/// The payload is the largest string a session holds, and a verdict can carry
/// a second one, so neither belongs in a log line.
impl fmt::Debug for ImageSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageSource")
            .field("media_type", &self.media_type)
            .field("base64_len", &self.data.len())
            .finish()
    }
}

impl<'de> Deserialize<'de> for ImageSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Borrowed so a block read from its buffered `Value` copies the
        // payload once, into the final `Arc`. A reader that cannot lend
        // strings hands over an owned one instead.
        #[derive(Deserialize)]
        struct Wire<'a> {
            media_type: ImageMediaType,
            #[serde(borrow)]
            data: Cow<'a, str>,
        }
        let wire = Wire::deserialize(deserializer)?;
        // Base64 image payloads are the largest strings a session holds, and a
        // load decodes each one once per record that carries it.
        Ok(Self::new(wire.media_type, intern::shared_str(&wire.data)))
    }
}

impl Serialize for ImageSource {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("ImageSource", 3)?;
        state.serialize_field("type", "base64")?;
        state.serialize_field("media_type", &self.media_type)?;
        state.serialize_field("data", &self.data)?;
        state.end()
    }
}

/// The verdict is derived from the payload, so two sources with the same
/// pixels are the same source.
impl PartialEq for ImageSource {
    fn eq(&self, other: &Self) -> bool {
        self.media_type == other.media_type && self.data == other.data
    }
}

impl ImageSource {
    pub fn new(media_type: ImageMediaType, data: Arc<str>) -> Self {
        Self {
            media_type,
            data,
            verdict: Arc::default(),
        }
    }

    pub fn to_data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type.mime(), self.data)
    }

    #[doc(hidden)]
    pub fn wire_verdict(&self) -> &OnceLock<WireVerdict> {
        &self.verdict
    }
}

pub fn is_deferred_tool(tool: &Value) -> bool {
    tool[DEFER_LOADING_KEY].as_bool() == Some(true)
}

/// A new variant is not skew-safe: an older build drops the whole record, so
/// it needs a `LOG_FORMAT_VERSION` bump.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, IntoStaticStr)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Role {
    #[default]
    User,
    Assistant,
}

impl Role {
    fn is_user(&self) -> bool {
        matches!(self, Self::User)
    }
}

/// Part of the frozen session surface (disk, exports, Lua), so it only grows:
///
/// - Fields and variants are only added, fields always with a default.
///   Neither needs a log format bump.
/// - An older build keeps a variant it does not know as [`Self::Unknown`],
///   but drops a field it does not know on its next rewrite. So data an older
///   build must not lose goes in a new variant, never in a new field. A field
///   is for something whose loss falls back to a default (`loaded_tools`, a
///   flag).
/// - A known block holding a value an older build cannot read (a new media
///   type, a new image source) is kept whole as [`Self::Unknown`] too, so
///   loading never fails on a block. Strict readers ask [`Self::strict_error`].
/// - A new variant may sit beside `ToolUse`/`ToolResult` but never replace
///   them. An older build treats it as inert, so a call answered only by a
///   new variant would be left unanswered there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, VariantNames, IntoStaticStr)]
#[serde(remote = "Self", tag = "type", rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
        /// Wire names of deferred tools this result loaded. A provider with
        /// API-side deferral replays them as `tool_reference` blocks, so the
        /// definitions expand here instead of rewriting the cached tools
        /// prefix; everyone else ignores them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        loaded_tools: Vec<String>,
    },
    Image {
        source: ImageSource,
    },
    /// A block this build cannot read, most likely written by a newer one.
    /// Kept as raw JSON and written back unchanged, never sent to a provider.
    /// Must stay the last variant (see `KNOWN_BLOCK_TYPES`).
    #[serde(skip)]
    Unknown(Value),
}

fn block_type(raw: &Value) -> Option<&str> {
    raw.get(BLOCK_TYPE_KEY)?.as_str()
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        match block_type(&raw) {
            None => warn!("keeping a content block without a string type as unknown"),
            Some(known) if KNOWN_BLOCK_TYPES.contains(&known) => match Self::deserialize(&raw) {
                Ok(block) => return Ok(block),
                // Expected under version skew and nothing the user can fix,
                // and loads are frequent. A strict import reports it loudly.
                Err(error) => debug!(
                    block_type = known,
                    %error,
                    "keeping a content block this build cannot read as unknown"
                ),
            },
            Some(newer) => debug!(
                block_type = newer,
                "keeping a content block of a type this build does not know"
            ),
        }
        Ok(Self::Unknown(raw))
    }
}

impl Serialize for ContentBlock {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unknown(raw) => raw.serialize(serializer),
            known => Self::serialize(known, serializer),
        }
    }
}

impl ContentBlock {
    pub fn is_thinking(&self) -> bool {
        matches!(self, Self::Thinking { .. } | Self::RedactedThinking { .. })
    }

    /// Why a strict reader (transcript import) must refuse this block, or
    /// `None` if it may keep it. An [`Self::Unknown`] of a type this build
    /// does not know is a newer build's block and passes. One without a
    /// string `type` gets [`UNTYPED_BLOCK`], and one of a known type gets the
    /// serde error that kept it from parsing.
    pub fn strict_error(&self) -> Option<String> {
        let Self::Unknown(raw) = self else {
            return None;
        };
        match block_type(raw) {
            None => Some(UNTYPED_BLOCK.to_owned()),
            Some(known) if KNOWN_BLOCK_TYPES.contains(&known) => {
                Self::deserialize(raw).err().map(|error| error.to_string())
            }
            Some(_) => None,
        }
    }
}

/// Who a message came from, which `role` cannot say. Providers only
/// accept user and assistant, so anything the host wants to report has to
/// travel as a user message, and without this there is no way to tell it
/// apart from the user actually typing. A prefix in the text would not do:
/// a log line can print one.
///
/// `Deserialize` reads a bare tag, or the nested `{"context_update": facts}`
/// that logs held before [`Message`] went flat. Nothing writes that anymore.
///
/// A new variant is not skew-safe: an older build drops the whole record, so
/// it needs a `LOG_FORMAT_VERSION` bump.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum MessageKind {
    /// Someone said this, the user or the model.
    #[default]
    Turn,
    /// The host noticed it and passed it to the model. It stays in session
    /// history for conversation order but is hidden from user-facing views.
    Observation,
    /// Something the system prompt said has changed (date, model, plan
    /// mode...). Editing the prompt would void the cache and every thinking
    /// block bound to it, so we append this instead. Views hide it, since it
    /// mostly repeats what the user just did (`/cd`, a model switch).
    ///
    /// It carries the facts it told, so what the model holds true can be read
    /// off the transcript, even after a resume or a rewind.
    ContextUpdate(Arc<FactsUpdate>),
}

impl MessageKind {
    fn is_turn(&self) -> bool {
        matches!(self, Self::Turn)
    }
}

impl ContentBlock {
    pub fn tool_use(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
            thought_signature: None,
        }
    }

    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error,
            loaded_tools: Vec::new(),
        }
    }
}

/// How a background task ended, as the notice that settled its receipt tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskEnd {
    Finished,
    Failed,
    /// Stopped by the user or `task_stop`. Only the transcript line says so:
    /// the model reads `finished` or `failed`, and the output names the stop.
    Cancelled,
    Lost,
}

/// The receipt a background notice answers and how its task ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSettled {
    pub task: String,
    pub end: TaskEnd,
}

/// Part of the frozen session surface (disk, exports, Lua), written flat:
/// `{"role", "content", "display_text"?, "kind"?, "facts"?, "receipts"?,
/// "settles"?}`, with `kind` as a bare tag and `facts` beside a
/// `context_update`. Older logs nest it as
/// `{"kind": {"context_update": facts}}`. Those still load, and are not
/// rewritten just for that.
///
/// It only grows, by the same rules as [`ContentBlock`].
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(try_from = "RawMessage")]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub display_text: Option<String>,
    pub kind: MessageKind,
    /// Calls whose results here are background receipts: each moved, and a
    /// later notice [`settles`](Self::settles) it.
    pub receipts: Vec<String>,
    /// Set on the notice that answers a receipt.
    pub settles: Option<TaskSettled>,
}

/// [`Message`] as stored, before `kind` and `facts` are paired up.
#[derive(Deserialize)]
struct RawMessage {
    role: Role,
    content: Vec<ContentBlock>,
    #[serde(default)]
    display_text: Option<String>,
    #[serde(default)]
    kind: Option<RawKind>,
    #[serde(default)]
    facts: Option<Arc<FactsUpdate>>,
    #[serde(default)]
    receipts: Vec<String>,
    #[serde(default, deserialize_with = "settles_or_none")]
    settles: Option<TaskSettled>,
}

/// A newer build may write a [`TaskEnd`] this one does not know. Only the
/// mark is dropped then, never the message, and the task reads as cancelled.
fn settles_or_none<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<TaskSettled>, D::Error> {
    let Some(raw) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    Ok(TaskSettled::deserialize(raw)
        .inspect_err(|error| debug!(%error, "dropping a task mark this build cannot read"))
        .ok())
}

/// [`MessageKind`] reads every kind but the flat `context_update` tag, whose
/// facts sit beside it.
#[derive(Deserialize)]
#[serde(
    untagged,
    expecting = "a message kind: a tag, or a nested context_update"
)]
enum RawKind {
    Kind(MessageKind),
    ContextUpdateTag(ContextUpdateTag),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ContextUpdateTag {
    ContextUpdate,
}

impl TryFrom<RawMessage> for Message {
    type Error = &'static str;

    fn try_from(raw: RawMessage) -> Result<Self, Self::Error> {
        let kind = match (raw.kind, raw.facts) {
            (Some(RawKind::ContextUpdateTag(_)), Some(facts)) => MessageKind::ContextUpdate(facts),
            (Some(RawKind::ContextUpdateTag(_)), None) => return Err(CONTEXT_UPDATE_WITHOUT_FACTS),
            (_, Some(_)) => return Err(FACTS_WITHOUT_CONTEXT_UPDATE),
            (Some(RawKind::Kind(kind)), None) => kind,
            (None, None) => MessageKind::Turn,
        };
        Ok(Self {
            role: raw.role,
            content: raw.content,
            display_text: raw.display_text,
            kind,
            receipts: raw.receipts,
            settles: raw.settles,
        })
    }
}

/// By hand rather than `into = "RawMessage"`, which clones the message first,
/// and every rewrite serializes the whole history.
impl Serialize for Message {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let facts = self.facts_update();
        let len = 2
            + usize::from(self.display_text.is_some())
            + usize::from(!self.kind.is_turn())
            + usize::from(facts.is_some())
            + usize::from(!self.receipts.is_empty())
            + usize::from(self.settles.is_some());
        let mut state = serializer.serialize_struct("Message", len)?;
        state.serialize_field("role", &self.role)?;
        state.serialize_field("content", &self.content)?;
        if let Some(display_text) = &self.display_text {
            state.serialize_field("display_text", display_text)?;
        }
        if !self.kind.is_turn() {
            state.serialize_field("kind", <&'static str>::from(&self.kind))?;
        }
        if let Some(facts) = facts {
            state.serialize_field("facts", facts)?;
        }
        if !self.receipts.is_empty() {
            state.serialize_field("receipts", &self.receipts)?;
        }
        if let Some(settles) = &self.settles {
            state.serialize_field("settles", settles)?;
        }
        state.end()
    }
}

impl Message {
    /// Stands in for an assistant turn with no text, thinking alone or empty,
    /// which providers reject as the trailing message. Never a real response:
    /// readers mining history for model text must skip it.
    pub fn empty_marker() -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: EMPTY_RESPONSE_MARKER.into(),
            }],
            ..Default::default()
        }
    }

    /// Something the host saw, reported to the model without pretending
    /// the user said it.
    pub fn observation(text: String) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text }],
            kind: MessageKind::Observation,
            ..Default::default()
        }
    }

    pub fn is_observation(&self) -> bool {
        self.kind == MessageKind::Observation
    }

    pub fn context_update(text: String, update: FactsUpdate) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text }],
            kind: MessageKind::ContextUpdate(Arc::new(update)),
            ..Default::default()
        }
    }

    pub fn is_context_update(&self) -> bool {
        self.facts_update().is_some()
    }

    /// Exactly what [`Self::context_update`] builds. Anything else tagged as
    /// one is not the host's, and a compaction would drop real content with it.
    pub fn is_host_context_update(&self) -> bool {
        self.is_context_update()
            && self.role == Role::User
            && self.display_text.is_none()
            && matches!(&self.content[..], [ContentBlock::Text { .. }])
    }

    pub fn facts_update(&self) -> Option<&FactsUpdate> {
        match &self.kind {
            MessageKind::ContextUpdate(update) => Some(update),
            _ => None,
        }
    }

    pub fn is_from_host(&self) -> bool {
        !self.kind.is_turn()
    }

    pub fn user(text: String) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text }],
            ..Default::default()
        }
    }

    pub fn user_display(ai_text: String, display: String) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: ai_text }],
            display_text: Some(display),
            ..Default::default()
        }
    }

    pub fn user_with_images(text: String, images: Vec<ImageSource>) -> Self {
        let mut content: Vec<ContentBlock> = images
            .into_iter()
            .map(|source| ContentBlock::Image { source })
            .collect();
        if !text.is_empty() {
            content.push(ContentBlock::Text { text });
        }
        Self {
            role: Role::User,
            content,
            ..Default::default()
        }
    }

    pub fn synthetic(text: String) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text }],
            display_text: Some(String::new()),
            ..Default::default()
        }
    }

    pub fn user_text(&self) -> Option<&str> {
        match &self.display_text {
            Some(t) if t.is_empty() => None,
            Some(t) => Some(t),
            None => self.first_text_content(),
        }
    }

    pub fn first_text_content(&self) -> Option<&str> {
        self.content.iter().find_map(|b| match b {
            ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
    }

    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }

    pub fn has_tool_calls(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    }
}

impl TitleSource for Message {
    fn first_user_text(&self) -> Option<&str> {
        if !self.role.is_user() || self.is_from_host() {
            return None;
        }
        self.user_text()
    }
}

pub type HistorySnapshot = maki_storage::sessions::HistorySnapshot<Message>;
pub type SharedMessages = Arc<ArcSwap<HistorySnapshot>>;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use maki_storage::frame::PlanChange;
    use maki_storage::sessions::generate_title;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const INTERNED_DATA: &str = "aW50ZXJuZWQtcGF5bG9hZA==";
    const OTHER_DATA: &str = "b3RoZXItcGF5bG9hZA==";
    const TASK_ID: &str = "toolu_moved";
    const PROMPT: &str = "fix the flaky login test";
    const UPDATE_TEXT: &str = "<context-update>plan mode is on</context-update>";
    const UPDATE_DATE: &str = "2026-10-02";
    const UPDATE_PLAN: &str = "/project/.maki/plan.md";
    const UPDATE_HINT_SLOT: &str = "memory/tags";
    const GONE_HINT_SLOT: &str = "todo/after_instructions";
    const UPDATE_HINT: &str = "rust, cache";
    const UNKNOWN_BLOCK_TYPE: &str = "server_tool_use";
    const NEWER_MEDIA_TYPE: &str = "image/heic";
    const UNKNOWN_VARIANT_NAME: &str = "unknown";
    const SERDE_UNKNOWN_VARIANT: &str = "unknown variant";
    const SERDE_INVALID_TYPE: &str = "invalid type";
    const SERDE_MISSING_ID: &str = "missing field `id`";

    #[test]
    fn user_with_images_text_and_images() {
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc123"));
        let msg = Message::user_with_images("hello".into(), vec![source]);
        assert_eq!(msg.content.len(), 2);
        assert!(matches!(&msg.content[0], ContentBlock::Image { .. }));
        assert!(matches!(&msg.content[1], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn user_with_images_empty_text_only_images() {
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc123"));
        let msg = Message::user_with_images(String::new(), vec![source]);
        assert_eq!(msg.content.len(), 1);
        assert!(matches!(&msg.content[0], ContentBlock::Image { .. }));
    }

    /// A resume reads how each background task ended off these marks.
    #[test]
    fn task_marks_write_the_flat_shape() {
        let stored = json!({
            "role": "user",
            "content": [],
            "receipts": [TASK_ID],
            "settles": { "task": TASK_ID, "end": "cancelled" }
        });
        let message = Message {
            receipts: vec![TASK_ID.into()],
            settles: Some(TaskSettled {
                task: TASK_ID.into(),
                end: TaskEnd::Cancelled,
            }),
            ..Default::default()
        };
        assert_eq!(serde_json::to_value(&message).unwrap(), stored);
        assert_eq!(serde_json::from_value::<Message>(stored).unwrap(), message);
    }

    #[test]
    fn an_unknown_task_end_drops_only_the_mark() {
        let stored = json!({
            "role": "user",
            "content": [{ "type": "text", "text": PROMPT }],
            "settles": { "task": TASK_ID, "end": "timed_out" }
        });
        let loaded: Message = serde_json::from_value(stored).unwrap();
        assert_eq!(loaded, Message::user(PROMPT.into()));
    }

    #[test]
    fn message_kind_is_backward_compatible() {
        let old: Message = serde_json::from_value(json!({
            "role": "user",
            "content": [{ "type": "text", "text": "hello" }]
        }))
        .unwrap();
        assert_eq!(old.kind, MessageKind::Turn);

        let turn = serde_json::to_value(Message::user("hello".into())).unwrap();
        assert!(turn.get("kind").is_none());

        let observation = Message::observation("built".into());
        assert_eq!(observation.first_user_text(), None);
        let observation = serde_json::to_value(observation).unwrap();
        assert_eq!(observation["kind"], "observation");
        let loaded: Message = serde_json::from_value(observation).unwrap();
        assert_eq!(loaded.kind, MessageKind::Observation);
    }

    fn plan_mode_facts() -> FactsUpdate {
        FactsUpdate {
            date: Some(UPDATE_DATE.into()),
            plan: Some(PlanChange::Entered(UPDATE_PLAN.into())),
            hints: BTreeMap::from([
                (UPDATE_HINT_SLOT.into(), Some(UPDATE_HINT.into())),
                (GONE_HINT_SLOT.into(), None),
            ]),
            ..FactsUpdate::default()
        }
    }

    fn plan_mode_facts_json() -> Value {
        json!({
            "date": UPDATE_DATE,
            "plan": { "entered": UPDATE_PLAN },
            "hints": { UPDATE_HINT_SLOT: UPDATE_HINT, GONE_HINT_SLOT: null }
        })
    }

    /// A resume reads what the model holds true from the stored updates, so
    /// the format is pinned. If it drifts, a resumed session forgets plan mode
    /// or the hints.
    #[test]
    fn context_update_writes_the_flat_shape() {
        let stored = json!({
            "role": "user",
            "content": [{ "type": "text", "text": UPDATE_TEXT }],
            "kind": "context_update",
            "facts": plan_mode_facts_json()
        });
        let update = Message::context_update(UPDATE_TEXT.into(), plan_mode_facts());
        assert_eq!(serde_json::to_value(&update).unwrap(), stored);

        let loaded: Message = serde_json::from_value(stored).unwrap();
        assert_eq!(loaded, update);
    }

    #[test]
    fn context_update_reads_the_old_nested_shape() {
        let stored = json!({
            "role": "user",
            "content": [{ "type": "text", "text": UPDATE_TEXT }],
            "kind": { "context_update": plan_mode_facts_json() }
        });
        let loaded: Message = serde_json::from_value(stored).unwrap();
        assert_eq!(
            loaded,
            Message::context_update(UPDATE_TEXT.into(), plan_mode_facts())
        );
    }

    #[test_case(json!({ "role": "user", "content": [], "kind": "context_update" }), CONTEXT_UPDATE_WITHOUT_FACTS ; "tag_without_facts")]
    #[test_case(json!({ "role": "user", "content": [], "facts": plan_mode_facts_json() }), FACTS_WITHOUT_CONTEXT_UPDATE ; "facts_without_tag")]
    fn unpaired_context_update_is_an_error(stored: Value, expected: &str) {
        let err = serde_json::from_value::<Message>(stored).unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
    }

    #[test_case(|_: &mut Message| {}, true ; "as_built")]
    #[test_case(|m: &mut Message| m.role = Role::Assistant, false ; "from_assistant")]
    #[test_case(|m: &mut Message| m.display_text = Some(PROMPT.into()), false ; "visible")]
    #[test_case(|m: &mut Message| m.content = vec![ContentBlock::tool_result("t1", PROMPT, false)], false ; "tool_result")]
    #[test_case(|m: &mut Message| m.content.push(ContentBlock::Text { text: PROMPT.into() }), false ; "two_blocks")]
    #[test_case(|m: &mut Message| m.kind = MessageKind::Turn, false ; "not_an_update")]
    fn host_context_update_is_the_constructed_shape(change: fn(&mut Message), expected: bool) {
        let mut message = Message::context_update(UPDATE_TEXT.into(), plan_mode_facts());
        change(&mut message);
        assert_eq!(message.is_host_context_update(), expected);
    }

    /// A block this build cannot read may hold a newer build's data, so it
    /// loads and writes back untouched. A strict import refuses it with the
    /// detail, unless only its type is new.
    #[test_case(json!({ "type": UNKNOWN_BLOCK_TYPE, "id": "srvtoolu_1" }), None                 ; "newer_type")]
    #[test_case(json!({ "type": "text", "text": 42 }), Some(SERDE_INVALID_TYPE)                     ; "text_wrong_type")]
    #[test_case(json!({ "type": "tool_use", "name": "read", "input": {} }), Some(SERDE_MISSING_ID) ; "tool_use_missing_id")]
    #[test_case(json!({ "type": "image", "source": { "type": "base64", "media_type": NEWER_MEDIA_TYPE, "data": INTERNED_DATA } }), Some(UNKNOWN_IMAGE_MEDIA_TYPE) ; "image_newer_media_type")]
    #[test_case(json!({ "text": PROMPT }), Some(UNTYPED_BLOCK)                                      ; "missing_type")]
    #[test_case(json!({ "type": 7, "text": PROMPT }), Some(UNTYPED_BLOCK)                           ; "non_string_type")]
    #[test_case(json!(PROMPT), Some(UNTYPED_BLOCK)                                                  ; "not_an_object")]
    fn unreadable_block_is_kept_as_written(stored: Value, strict_detail: Option<&str>) {
        let loaded: ContentBlock = serde_json::from_value(stored.clone()).unwrap();
        assert!(matches!(&loaded, ContentBlock::Unknown(raw) if *raw == stored));
        assert_eq!(serde_json::to_value(&loaded).unwrap(), stored);
        match (loaded.strict_error(), strict_detail) {
            (None, None) => {}
            (Some(error), Some(detail)) => assert!(error.contains(detail), "{error}"),
            (error, detail) => panic!("strict error {error:?}, expected {detail:?}"),
        }
    }

    /// A name strum derives but serde does not parse would turn every block
    /// of that type into `Unknown`. Such a block may fail on its fields,
    /// never on the tag.
    #[test]
    fn every_derived_block_type_is_a_serde_tag() {
        assert_eq!(ContentBlock::VARIANTS.last(), Some(&UNKNOWN_VARIANT_NAME));
        for &name in KNOWN_BLOCK_TYPES {
            if let Err(error) = ContentBlock::deserialize(json!({ BLOCK_TYPE_KEY: name })) {
                let error = error.to_string();
                assert!(!error.contains(SERDE_UNKNOWN_VARIANT), "{name}: {error}");
            }
        }
    }

    /// A first run in plan mode tells the model before the user's prompt, and
    /// the session must still be named after what the user typed.
    #[test_case(Message::context_update(UPDATE_TEXT.into(), plan_mode_facts()) ; "context_update")]
    #[test_case(Message::observation(UPDATE_TEXT.into())                       ; "observation")]
    fn title_skips_what_the_host_wrote(host: Message) {
        assert_eq!(
            generate_title(&[host, Message::user(PROMPT.into())]),
            PROMPT
        );
    }

    /// A result that loaded nothing must serialize to the same bytes as
    /// before, or every provider's cached transcript breaks on upgrade.
    #[test]
    fn tool_result_loaded_tools_is_backward_compatible() {
        let stored = json!({ "type": "tool_result", "tool_use_id": "t1", "content": "ok" });
        let old: ContentBlock = serde_json::from_value(stored.clone()).unwrap();
        assert!(
            matches!(&old, ContentBlock::ToolResult { loaded_tools, .. } if loaded_tools.is_empty())
        );
        assert_eq!(serde_json::to_value(&old).unwrap(), stored);
    }

    #[test_case(ImageMediaType::Png,  "image/png"  ; "png")]
    #[test_case(ImageMediaType::Jpeg, "image/jpeg" ; "jpeg")]
    #[test_case(ImageMediaType::Gif,  "image/gif"  ; "gif")]
    #[test_case(ImageMediaType::Webp, "image/webp" ; "webp")]
    fn image_source_data_url(media: ImageMediaType, mime: &str) {
        let source = ImageSource::new(media, Arc::from("dGVzdA=="));
        assert_eq!(source.to_data_url(), format!("data:{mime};base64,dGVzdA=="));
    }

    #[test_case("image/png",  Some(ImageMediaType::Png)  ; "png")]
    #[test_case("image/webp", Some(ImageMediaType::Webp) ; "webp")]
    #[test_case("image/bmp",  None                       ; "unsupported")]
    fn media_type_from_mime(mime: &str, expected: Option<ImageMediaType>) {
        assert_eq!(ImageMediaType::from_mime(mime), expected);
    }

    #[test]
    fn image_source_serde_injects_type_base64() {
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc123"));
        let json = serde_json::to_value(&source).unwrap();
        assert_eq!(json["type"], "base64");
        assert_eq!(json["media_type"], "image/png");
        assert_eq!(json["data"], "abc123");
        let deserialized: ImageSource = serde_json::from_value(json).unwrap();
        assert_eq!(deserialized.media_type, ImageMediaType::Png);
        assert_eq!(&*deserialized.data, "abc123");
    }

    fn image_block_source(wire: &[u8]) -> ImageSource {
        match serde_json::from_slice(wire).unwrap() {
            ContentBlock::Image { source } => source,
            other => panic!("expected an image, got {other:?}"),
        }
    }

    fn image_source_from_reader(wire: &[u8]) -> ImageSource {
        serde_json::from_reader(wire).unwrap()
    }

    /// A block borrows the payload from its buffered `Value`, while a reader
    /// cannot lend strings at all. Both must yield the same image.
    #[test_case(json!({ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": INTERNED_DATA } }), image_block_source ; "block_borrowed_from_value")]
    #[test_case(json!({ "type": "base64", "media_type": "image/png", "data": INTERNED_DATA }), image_source_from_reader ; "source_owned_from_reader")]
    fn image_source_deserializes_borrowed_or_owned(stored: Value, read: fn(&[u8]) -> ImageSource) {
        let source = read(stored.to_string().as_bytes());
        assert_eq!(source.media_type, ImageMediaType::Png);
        assert_eq!(&*source.data, INTERNED_DATA);
    }

    /// The payloads a session repeats across records collapse to one
    /// allocation, and only while a load is in flight.
    #[test]
    fn image_source_deserialize_shares_identical_payloads_within_a_load() {
        let wire = |data: &str| json!({"media_type": "image/png", "data": data}).to_string();
        let read = |data: &str| serde_json::from_str::<ImageSource>(&wire(data)).unwrap();

        let (first, other) = {
            let _scope = intern::Scope::enter();
            let first = read(INTERNED_DATA);
            assert!(Arc::ptr_eq(&first.data, &read(INTERNED_DATA).data));
            (first, read(OTHER_DATA))
        };

        assert!(!Arc::ptr_eq(&first.data, &other.data));
        assert_eq!(&*first.data, INTERNED_DATA);
        assert!(!Arc::ptr_eq(&first.data, &read(INTERNED_DATA).data));
    }
}
