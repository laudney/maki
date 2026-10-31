//! Types for provider communication. The message types they carry live in
//! `maki_types::message`.

use std::borrow::Cow;

pub use maki_storage::sessions::Effort;
use maki_types::message::WireVerdict;
pub use maki_types::message::{
    CONTEXT_UPDATE_WITHOUT_FACTS, ContentBlock, DEFER_LOADING_KEY, EMPTY_RESPONSE_MARKER,
    FACTS_WITHOUT_CONTEXT_UPDATE, IMAGE_PLACEHOLDER, ImageMediaType, ImageSource, Message,
    MessageKind, Role, TaskEnd, TaskSettled, UNKNOWN_IMAGE_MEDIA_TYPE, UNTYPED_BLOCK,
    is_deferred_tool,
};
pub use maki_types::stop_reason::StopReason;
use maki_types::thinking::Budgeted;
pub(crate) use maki_types::thinking::FALLBACK_MAX_THINKING_BUDGET;
pub use maki_types::thinking::{RequestOptions, THINKING_USAGE, ThinkingConfig};
pub use maki_types::usage::{ModelUsageRow, ProviderUsage, UsageLimit};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::TokenUsage;
use crate::image::{IMAGE_EVICTION_STEP, MAX_IMAGES, fix_for_wire};
use crate::model::Model;

const LOCAL_BUDGET_FIELD: &str = "thinking_budget_tokens";
pub const IMAGE_OMITTED_NOTE: &str =
    "[image omitted: the current model does not support image input]";
/// A broken payload has to leave the request, or every turn after it fails.
pub const IMAGE_UNUSABLE_NOTE: &str = "[image omitted: the image could not be decoded]";
/// Past [`MAX_IMAGES`] the request itself is refused, so the oldest pixels
/// make way rather than taking the whole session down.
pub const IMAGE_EVICTED_NOTE: &str = "[image omitted: too many images in this conversation]";

/// The last stop before the wire for every image in a request, whatever put
/// it there. For models without vision, image blocks become a text note
/// instead of a block the API would reject, and history keeps the pixels, so
/// switching back to a vision-capable model restores them. For the rest,
/// oversized payloads are rewritten to fit provider limits, because one image
/// a provider refuses would otherwise fail every later request in the session
/// too.
pub async fn adapt_images_for_model<'a>(
    model: &Model,
    messages: &'a [Message],
) -> Cow<'a, [Message]> {
    // Newest first: the stale screenshots are the ones a long session can
    // spare once the request runs out of room for them. Collected rather than
    // walked lazily, since a borrow of `messages` held across the await below
    // leaves callers unable to prove their own futures `Send`.
    let images: Vec<(usize, usize, ImageSource)> = messages
        .iter()
        .enumerate()
        .rev()
        .flat_map(|(m, message)| {
            message
                .content
                .iter()
                .enumerate()
                .rev()
                .filter_map(move |(b, block)| match block {
                    ContentBlock::Image { source } => Some((m, b, source.clone())),
                    _ => None,
                })
        })
        .collect();
    let note = |text: &str| ContentBlock::Text { text: text.into() };
    let vision = model.supports_vision();
    let kept = images_kept(images.len());
    let mut edits: Vec<(usize, usize, ContentBlock)> = Vec::new();
    // Nothing past the cap is decoded at all.
    for (newest_first, (m, b, source)) in images.into_iter().enumerate() {
        if !vision {
            edits.push((m, b, note(IMAGE_OMITTED_NOTE)));
        } else if newest_first >= kept {
            edits.push((m, b, note(IMAGE_EVICTED_NOTE)));
        } else {
            match fix_for_wire(&source).await {
                WireVerdict::Keep => {}
                WireVerdict::Replace(source) => edits.push((m, b, ContentBlock::Image { source })),
                WireVerdict::Drop => edits.push((m, b, note(IMAGE_UNUSABLE_NOTE))),
            }
        }
    }
    if edits.is_empty() {
        return Cow::Borrowed(messages);
    }
    // A settled image hands its cached verdict back, so once anything here
    // needed repairing `edits` is non-empty on every later turn too. The owned
    // slice a provider takes has to carry every message, so the untouched ones
    // are still cloned, but only a message an edit lands on is rebuilt, and it
    // is rebuilt out of the blocks it keeps rather than cloned whole and then
    // written over. History keeps the originals on purpose: see above.
    // The walk above ran newest-first, so reversing the edits lines them up
    // with a single forward pass.
    let mut edits = edits.into_iter().rev().peekable();
    let mut adapted = Vec::with_capacity(messages.len());
    for (m, message) in messages.iter().enumerate() {
        if edits.peek().is_none_or(|&(edited, ..)| edited != m) {
            adapted.push(message.clone());
            continue;
        }
        let mut content = Vec::with_capacity(message.content.len());
        for (b, block) in message.content.iter().enumerate() {
            match edits.next_if(|&(edited, at, _)| (edited, at) == (m, b)) {
                Some((.., replacement)) => content.push(replacement),
                None => content.push(block.clone()),
            }
        }
        adapted.push(Message {
            role: message.role.clone(),
            content,
            display_text: message.display_text.clone(),
            kind: message.kind.clone(),
            receipts: message.receipts.clone(),
            settles: message.settles.clone(),
        });
    }
    Cow::Owned(adapted)
}

/// How many of the newest `total` images a request keeps. Past the cap a
/// whole [`IMAGE_EVICTION_STEP`] of the oldest goes at once, and new images
/// fill the gap one by one. It depends on the count alone, never on which
/// images decode, so which ones are evicted only moves once per step. An
/// unreadable image therefore holds its slot until it ages out.
fn images_kept(total: usize) -> usize {
    let over = total.saturating_sub(MAX_IMAGES);
    total - over.div_ceil(IMAGE_EVICTION_STEP) * IMAGE_EVICTION_STEP
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ProviderEvent {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolUseStart {
        id: String,
        name: String,
    },
    PromptProgress {
        processed: u32,
        total: u32,
        cache: u32,
    },
}

/// First Claude version that speaks adaptive thinking. Opus got there a
/// generation early, at 4.7; the other families joined at 5.
const ADAPTIVE_SINCE: (u32, u32) = (5, 0);
const ADAPTIVE_SINCE_OPUS: (u32, u32) = (4, 7);
const GEMINI_LEVEL_SINCE: u32 = 3;
const OPUS: &str = "opus";
const CLAUDE: &str = "claude";
const GEMINI: &str = "gemini";
const LATEST: &str = "latest";
const SNAPSHOT_DATE_LEN: usize = "20250514".len();

/// `claude-opus-4.7` -> `("opus", (4, 7))`, `claude-opus-5-1m` -> `("opus", (5, 0))`.
/// Every provider dresses the id up its own way. Copilot writes `claude-opus-4.7`,
/// Bedrock `us.anthropic.claude-opus-4-7-v1:0`, Vertex `claude-opus-4-1@20250805`
/// and OpenRouter `anthropic/claude-opus-4.7:thinking`. So any char that is not
/// a letter or digit splits the id, and we read what follows the `claude` word.
/// Old ids put the version first (`claude-3-5-sonnet`), and a numeric family
/// tells us there is no modern version to read.
fn claude_version(model_id: &str) -> Option<(&str, (u32, u32))> {
    let mut tokens = model_id
        .split(|c: char| !c.is_ascii_alphanumeric())
        .skip_while(|token| *token != CLAUDE)
        .skip(1);
    let family = tokens.next().filter(|f| f.parse::<u32>().is_err())?;
    let major = tokens.next().and_then(version_number)?;
    let minor = tokens.next().and_then(version_number).unwrap_or(0);
    Some((family, (major, minor)))
}

/// Snapshot dates can sit right where the minor goes (`claude-opus-4-20250514`).
/// We once read that date as the minor, and Opus 4 looked newer than 4.7.
fn version_number(token: &str) -> Option<u32> {
    token
        .parse()
        .ok()
        .filter(|_| token.len() < SNAPSHOT_DATE_LEN)
}

/// Models from [`ADAPTIVE_SINCE`] on reject `type: "enabled"` with a 400. A
/// version check, not an allowlist, so future releases and new families work
/// automatically. Reads the id, not the provider, so a gateway serving Claude
/// is covered too.
fn requires_adaptive(model_id: &str) -> bool {
    claude_version(model_id).is_some_and(|(family, version)| {
        let since = if family == OPUS {
            ADAPTIVE_SINCE_OPUS
        } else {
            ADAPTIVE_SINCE
        };
        version >= since
    })
}

/// The releases that made thinking adaptive-only also reject any sampling
/// parameter (`top_p`, `temperature`) with a 400, even with thinking off.
pub(crate) fn rejects_sampling(model_id: &str) -> bool {
    requires_adaptive(model_id)
}

/// Gemini 3+ and the `-latest` aliases take `thinkingLevel` (`low`, `medium`,
/// `high`) in place of `thinkingBudget`. Only the 2.x series speaks budgets;
/// `minimal` is omitted from the dialect because Pro and Gemini 4 reject it
/// with a 400.
fn uses_google_thinking_level(model_id: &str) -> bool {
    let mut tokens = model_id
        .split(|c: char| !c.is_ascii_alphanumeric())
        .skip_while(|token| *token != GEMINI)
        .skip(1);
    let Some(first) = tokens.next() else {
        return false;
    };
    if let Some(major) = version_number(first) {
        major >= GEMINI_LEVEL_SINCE
    } else {
        tokens.any(|token| token == LATEST)
    }
}

/// How a provider's effort knob speaks: which levels its API accepts, what
/// `adaptive` means there, and whether "off" needs an explicit string.
/// New providers add a const in [`dialect`]; providers with dynamic model
/// listings build one from the model's declared levels (see OpenRouter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortDialect<'a> {
    /// Wire name of a declared dialect; `None` for one built at runtime.
    pub name: Option<&'static str>,
    /// Accepted levels, non-empty and ascending (checked by test).
    pub supported: &'a [Effort],
    /// What `Adaptive` maps to. `None` means the API has its own adaptive or
    /// default behavior: send nothing and let it decide.
    pub adaptive: Option<Effort>,
    /// Explicit opt-out string, e.g. GLM `"none"`.
    pub off: Option<&'static str>,
}

impl EffortDialect<'_> {
    /// The level `Adaptive` goes out as, `None` where nothing is sent.
    pub fn adaptive_effort(&self) -> Option<Effort> {
        self.adaptive.map(|level| level.snap(self.supported))
    }

    /// Mirrors [`ModelThinking::effort_str`], so the badge can name what the
    /// request will carry.
    pub fn sends(&self, choice: ThinkingConfig) -> ThinkingConfig {
        match choice {
            ThinkingConfig::Adaptive => self
                .adaptive_effort()
                .map_or(choice, ThinkingConfig::Effort),
            ThinkingConfig::Effort(level) => ThinkingConfig::Effort(level.snap(self.supported)),
            ThinkingConfig::Off | ThinkingConfig::Budget(_) => choice,
        }
    }

    /// Discovery may list other levels than the dialect knows. The wire and
    /// the badge both read them through here, so the two never disagree.
    fn as_discovered<R>(&self, model: &Model, render: impl FnOnce(&EffortDialect) -> R) -> R {
        match model.discovered_effort() {
            Some(effort) => render(&effort.refine(self)),
            None => render(self),
        }
    }
}

/// The model's own spelling of each thinking mode, parsed and validated by
/// maki-config. Turning one into wire bytes is this crate's job, see
/// [`declared_fragment`].
pub use maki_config::providers::ThinkingFields;

fn declared_effort(fields: &ThinkingFields, level: Effort) -> Option<Effort> {
    fields
        .levels
        .range(..=level)
        .next_back()
        .or_else(|| fields.levels.first_key_value())
        .map(|(&level, _)| level)
}

/// What a request says when the model's fields do not spell the mode asked
/// for. The fragment merge is identical everywhere, only this last step
/// differs, so paths hand it in rather than each growing their own apply fn.
#[derive(Debug, Clone, Copy)]
pub enum ThinkingFallback {
    /// Nothing. A generic openai-compat gateway takes whatever its upstream
    /// takes, and guessing would 400 the ones that are strict.
    None,
    /// An effort string, for thinking-capable models only.
    Dialect(&'static EffortDialect<'static>),
    /// llama.cpp's token budget field.
    BudgetField,
}

impl ThinkingFallback {
    /// What [`ModelThinking::apply_thinking`] sends `choice` as on `model`.
    /// A model that spells `Adaptive` itself keeps it, and an endpoint left
    /// to decide gets every choice as it is.
    pub(crate) fn sends(self, model: &Model, choice: ThinkingConfig) -> ThinkingConfig {
        let spelled = choice == ThinkingConfig::Adaptive
            && model
                .effective_thinking_fields()
                .is_some_and(|(fields, _)| fields.adaptive.is_some());
        match self {
            Self::Dialect(dialect) if !spelled => {
                dialect.as_discovered(model, |dialect| dialect.sends(choice))
            }
            Self::None | Self::Dialect(_) | Self::BudgetField => choice,
        }
    }
}

/// The setting the native fragment sends. Budgets survive only where the
/// adapter can forward them alongside a toggle.
fn declared_thinking(
    fields: &ThinkingFields,
    thinking: ThinkingConfig,
    model: &Model,
    fallback: ThinkingFallback,
) -> Option<ThinkingConfig> {
    let level = match thinking {
        ThinkingConfig::Off => return fields.off.as_ref().map(|_| thinking),
        ThinkingConfig::Adaptive => return fields.adaptive.as_ref().map(|_| thinking),
        ThinkingConfig::Effort(level) => level,
        ThinkingConfig::Budget(n) => {
            if fields.levels.is_empty() {
                return fields.adaptive.as_ref().map(|_| match fallback {
                    ThinkingFallback::BudgetField => thinking,
                    ThinkingFallback::None | ThinkingFallback::Dialect(_) => {
                        ThinkingConfig::Adaptive
                    }
                });
            }
            Effort::from_budget(
                n,
                model
                    .max_thinking_budget()
                    .unwrap_or(FALLBACK_MAX_THINKING_BUDGET),
            )
        }
    };
    declared_effort(fields, level)
        .map(ThinkingConfig::Effort)
        .or_else(|| fields.adaptive.as_ref().map(|_| ThinkingConfig::Adaptive))
}

fn declared_fragment<'a>(
    fields: &'a ThinkingFields,
    thinking: ThinkingConfig,
    model: &Model,
    fallback: ThinkingFallback,
) -> Option<(&'a Map<String, Value>, bool)> {
    let resolved = declared_thinking(fields, thinking, model, fallback)?;
    let fragment = match resolved {
        ThinkingConfig::Off => fields.off.as_ref(),
        ThinkingConfig::Adaptive | ThinkingConfig::Budget(_) => fields.adaptive.as_ref(),
        ThinkingConfig::Effort(level) => fields.levels.get(&level),
    }?;
    Some((fragment, matches!(resolved, ThinkingConfig::Budget(_))))
}

pub(crate) fn merge_body(body: &mut Map<String, Value>, fragment: &Map<String, Value>) {
    for (key, value) in fragment {
        match (body.get_mut(key), value.as_object()) {
            (Some(Value::Object(target)), Some(source)) => merge_body(target, source),
            _ => {
                body.insert(key.clone(), value.clone());
            }
        }
    }
}

/// One table, and every view of it is read off that table: the consts, the
/// wire names, and the lookup in both directions. Kept by hand they drift, and
/// a dialect missing from `by_name` is a declaration that stops loading.
macro_rules! dialects {
    ($(
        $(#[$attr:meta])+
        $konst:ident $name:literal {
            supported: [$($level:ident),+ $(,)?],
            adaptive: $adaptive:expr,
            off: $off:expr $(,)?
        }
    ),+ $(,)?) => {
        $(
            $(#[$attr])+
            pub const $konst: EffortDialect = EffortDialect {
                name: Some($name),
                supported: &[$($level),+],
                adaptive: $adaptive,
                off: $off,
            };
        )+

        /// Every dialect name, in declaration order.
        pub const NAMES: &[&str] = &[$($name),+];

        /// Resolve a dialect by its wire name, as listed in [`NAMES`].
        pub fn by_name(name: &str) -> Option<&'static EffortDialect<'static>> {
            match name {
                $($name => Some(&$konst),)+
                _ => None,
            }
        }

        /// The name [`by_name`] answers this dialect to, so a declaration can
        /// serialise the dialect it holds instead of keeping the name twice.
        /// The macro wrote both from the same literal, so the two cannot
        /// drift. `None` for a dialect built at runtime out of a model's
        /// declared levels (see OpenRouter), which no declaration can name.
        pub fn name_of(dialect: &EffortDialect) -> Option<&'static str> {
            dialect.name
        }
    };
}

pub mod dialect {
    use super::EffortDialect;
    use maki_storage::sessions::Effort::{High, Low, Max, Medium, Minimal, XHigh};

    /// Wire string that disables reasoning, for APIs that need an explicit
    /// opt-out.
    pub const OFF: &str = "none";

    dialects! {
        /// OpenAI platform, synthetic.
        STANDARD "standard" {
            supported: [Minimal, Low, Medium, High],
            adaptive: Some(Medium),
            off: None
        },
        /// OpenAI Responses API models whose highest effort is `xhigh`.
        CODEX "codex" {
            supported: [Low, Medium, High, XHigh],
            adaptive: Some(Medium),
            off: None
        },
        /// OpenAI GPT-5.1 Codex Responses API models.
        CODEX_5_1 "codex-5-1" {
            supported: [Low, Medium, High],
            adaptive: Some(Medium),
            off: None
        },
        /// OpenAI Coding Plan models that aren't Codex. They keep `minimal`, and
        /// the Responses API opts out of reasoning with an explicit "none".
        CODING_PLAN "coding-plan" {
            supported: [Minimal, Low, Medium, High, XHigh],
            adaptive: Some(Medium),
            off: Some(OFF)
        },
        /// OpenAI GPT-5.6 Coding Plan models (Luna, Terra, Sol), which also take
        /// `max`.
        GPT_5_6 "gpt-5-6" {
            supported: [Minimal, Low, Medium, High, XHigh, Max],
            adaptive: Some(Medium),
            off: Some(OFF)
        },
        /// OpenAI GPT-6 (Astra): `low` through `max`, no `minimal` and no
        /// explicit opt-out, so Off omits the reasoning field.
        GPT_6 "gpt-6" {
            supported: [Low, Medium, High, XHigh, Max],
            adaptive: Some(Medium),
            off: None
        },
        /// opencode chat-completions, openrouter (static fallback).
        PREFER_HIGH "prefer-high" {
            supported: [Low, Medium, High],
            adaptive: Some(High),
            off: None
        },
        /// Mistral.
        HIGH_ONLY "high-only" {
            supported: [High],
            adaptive: Some(High),
            off: None
        },
        /// Z.AI. GLM reasons by default, so Off sends "none" explicitly.
        /// Only use behind `Model::supports_thinking`.
        GLM "glm" {
            supported: [High, XHigh],
            adaptive: Some(High),
            off: Some(OFF)
        },
        /// DeepSeek accepts only "max"; Adaptive keeps the model's own default
        /// reasoning depth by sending no effort at all.
        DEEPSEEK "deepseek" {
            supported: [Max],
            adaptive: None,
            off: None
        },
        /// `output_config.effort` on Anthropic adaptive-thinking models, all of
        /// which take `xhigh` and `max`. The API has native adaptive mode, so
        /// Adaptive sends no effort.
        ANTHROPIC_ADAPTIVE "anthropic-adaptive" {
            supported: [Low, Medium, High, XHigh, Max],
            adaptive: None,
            off: None
        },
        /// Gemini 3+ `thinkingLevel`. Pro and Gemini 4 reject `minimal`.
        GEMINI "gemini" {
            supported: [Low, Medium, High],
            adaptive: None,
            off: None
        },
        /// TensorX routes models that may reason by default, so Off sends "none"
        /// explicitly and Adaptive asks for full depth.
        TENSORX "tensorx" {
            supported: [Low, Medium, High],
            adaptive: Some(High),
            off: Some(OFF)
        },
        /// xAI Grok 4.5/4.6. Adaptive defaults to high; Off sends nothing so the
        /// model keeps its own default. `xhigh` is advertised on Grok 4.6.
        GROK "grok" {
            supported: [Low, Medium, High, XHigh],
            adaptive: Some(High),
            off: None
        },
        /// Ollama's OpenAI-compat endpoint documents low, medium and high, and
        /// rejects the rest, so anything higher snaps down. A model with its own
        /// words for it says so through `thinking_fields` instead. Leaving effort
        /// out lets a capable model start reasoning on its own, so Off has to say
        /// "none" out loud. Only use behind `Model::supports_thinking`.
        OLLAMA "ollama" {
            supported: [Low, Medium, High],
            adaptive: Some(Medium),
            off: Some(OFF)
        },
    }
}

/// The parts of a thinking level that read the model's provider registries.
pub trait ModelThinking {
    /// Thinking tokens this config asks for on `model` before any request-level
    /// trim, or `None` when the provider decides (`Off`, `Adaptive`, or an
    /// unknown output window). This is the number a caller sizing `max_tokens`
    /// has to leave room for.
    fn reserved_thinking(self, model: &Model) -> Option<u32>;

    /// What [`Self::reserved_thinking`] comes out as once the `max_tokens` on
    /// the wire has had its say.
    fn request_thinking(self, model: &Model) -> Option<u32>;

    /// The budget one request carries: the level the user picked resolved
    /// against what the model declares (`declared`), then cut to what the
    /// `max_tokens` on the wire can house.
    ///
    /// Resolve then cut, never resolve against the cut. An effort level is a
    /// percentage, so handing it a trimmed ceiling redefines what the user
    /// picked, and an explicit budget read back through [`Effort::from_budget`]
    /// comes out as a *higher* level than it went in as. Cutting afterwards
    /// lowers the thinking exactly as far as the window forces and no further.
    #[doc(hidden)]
    fn request_budget(self, model: &Model, declared: Option<u32>) -> Budgeted;

    /// The effort string to send, snapped to the dialect's supported levels
    /// here and nowhere else (never chain snaps). `None` means send nothing:
    /// `Off` without an explicit off string, or `Adaptive` on APIs with their
    /// own default behavior.
    fn effort_str(self, dialect: &EffortDialect, model: &Model) -> Option<&'static str>;

    /// Anthropic messages API body. Adaptive-thinking models get the native
    /// adaptive knob plus `output_config.effort`; legacy models get a plain
    /// token budget.
    fn apply_to_body(self, body: &mut Value, model: &Model);

    fn apply_reasoning_effort(self, body: &mut Value, dialect: &EffortDialect, model: &Model);

    /// What the model says about itself wins, and `fallback` covers the modes
    /// it left unsaid.
    fn apply_thinking(self, body: &mut Value, model: &Model, fallback: ThinkingFallback);

    /// Merges the fragment `fields` spell for this mode, and says whether they
    /// spell it at all.
    #[doc(hidden)]
    fn apply_fields(
        self,
        body: &mut Value,
        model: &Model,
        fields: &ThinkingFields,
        fallback: ThinkingFallback,
    ) -> bool;

    /// `max` is Google's own documented ceiling on thinking, which is a
    /// capability and so part of resolving the level, not a trim.
    fn apply_google_thinking(self, body: &mut Value, model: &Model, max: u32);

    /// Enforces thinking support without rewriting enabled intent. Effort
    /// stays a delegation ceiling even when the wire snaps it to a native
    /// level or sends a toggle. [`Self::shown`] resolves the displayed choice.
    fn clamped(self, model: &Model) -> Self;

    /// The picker and badge follow the wire while stored intent keeps its
    /// delegation ceiling. Declared fields keep `Adaptive` separate because
    /// their level fragments can differ from the provider fallback.
    fn shown(self, model: &Model) -> Self;
}

impl ModelThinking for ThinkingConfig {
    fn reserved_thinking(self, model: &Model) -> Option<u32> {
        match self.budget(model.max_thinking_budget()) {
            Budgeted::Tokens(n) => Some(n),
            Budgeted::Off | Budgeted::Adaptive => None,
        }
    }

    fn request_thinking(self, model: &Model) -> Option<u32> {
        let asked = self.reserved_thinking(model)?;
        Some(model.thinking_ceiling().map_or(asked, |c| asked.min(c)))
    }

    fn request_budget(self, model: &Model, declared: Option<u32>) -> Budgeted {
        match (self.budget(declared), model.thinking_ceiling()) {
            (Budgeted::Tokens(asked), Some(ceiling)) => Budgeted::Tokens(asked.min(ceiling)),
            (budgeted, _) => budgeted,
        }
    }

    fn effort_str(self, dialect: &EffortDialect, model: &Model) -> Option<&'static str> {
        let level = match self {
            Self::Off => return dialect.off,
            Self::Adaptive => dialect.adaptive?,
            Self::Effort(e) => e,
            Self::Budget(n) => Effort::from_budget(
                n,
                model
                    .max_thinking_budget()
                    .unwrap_or(FALLBACK_MAX_THINKING_BUDGET),
            ),
        };
        Some(level.snap(dialect.supported).as_str())
    }

    fn apply_to_body(self, body: &mut Value, model: &Model) {
        if requires_adaptive(&model.id) {
            if matches!(self, Self::Off) {
                return;
            }
            // These models default `display` to "omitted", so thinking arrives
            // empty and tool calls pop up out of nowhere in the UI. Asking for
            // the summary back costs nothing: thinking tokens bill the same.
            body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
            if let Some(effort) = self.effort_str(&dialect::ANTHROPIC_ADAPTIVE, model) {
                body["output_config"]["effort"] = json!(effort);
            }
            return;
        }
        match self.request_budget(model, model.max_thinking_budget()) {
            Budgeted::Off => {}
            Budgeted::Adaptive => body["thinking"] = json!({"type": "adaptive"}),
            Budgeted::Tokens(n) => {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": n});
            }
        }
    }

    fn apply_reasoning_effort(self, body: &mut Value, dialect: &EffortDialect, model: &Model) {
        if let Some(effort) = self.effort_str(dialect, model) {
            body["reasoning_effort"] = json!(effort);
        }
    }

    fn apply_thinking(self, body: &mut Value, model: &Model, fallback: ThinkingFallback) {
        if let Some(fields) = &model.thinking_fields
            && self.apply_fields(body, model, fields, fallback)
        {
            return;
        }
        match fallback {
            ThinkingFallback::None => {}
            ThinkingFallback::Dialect(dialect) => {
                if model.supports_thinking() {
                    dialect.as_discovered(model, |dialect| {
                        self.apply_reasoning_effort(body, dialect, model);
                    });
                }
            }
            // The model has no way to spell this mode, so the budget field
            // takes over: a request must never end up saying nothing.
            ThinkingFallback::BudgetField => {
                let budget = match self.request_budget(model, model.max_thinking_budget()) {
                    Budgeted::Off => 0,
                    Budgeted::Adaptive => -1,
                    Budgeted::Tokens(n) => i64::from(n),
                };
                body[LOCAL_BUDGET_FIELD] = json!(budget);
            }
        }
    }

    fn apply_fields(
        self,
        body: &mut Value,
        model: &Model,
        fields: &ThinkingFields,
        fallback: ThinkingFallback,
    ) -> bool {
        let max = model.max_thinking_budget();
        let Some((fragment, keep_budget)) = declared_fragment(fields, self, model, fallback) else {
            return false;
        };
        let Some(object) = body.as_object_mut() else {
            return false;
        };
        merge_body(object, fragment);
        if keep_budget
            && matches!(fallback, ThinkingFallback::BudgetField)
            && let Budgeted::Tokens(budget) = self.request_budget(model, max)
        {
            body[LOCAL_BUDGET_FIELD] = json!(budget);
        }
        true
    }

    fn apply_google_thinking(self, body: &mut Value, model: &Model, max: u32) {
        if uses_google_thinking_level(&model.id) {
            if matches!(self, Self::Off) {
                return;
            }
            let mut config = json!({"includeThoughts": true});
            if let Some(level) = self.effort_str(&dialect::GEMINI, model) {
                config["thinkingLevel"] = json!(level);
            }
            body["generationConfig"]["thinkingConfig"] = config;
            return;
        }
        match self.request_budget(model, Some(max)) {
            Budgeted::Off => {}
            Budgeted::Adaptive => {
                body["generationConfig"]["thinkingConfig"] = json!({"includeThoughts": true});
            }
            Budgeted::Tokens(n) => {
                body["generationConfig"]["thinkingConfig"] =
                    json!({"includeThoughts": true, "thinkingBudget": n});
            }
        }
    }

    fn clamped(self, model: &Model) -> Self {
        if !model.supports_thinking() {
            return Self::Off;
        }
        if model.requires_thinking() && !self.is_enabled() {
            return Self::Effort(Effort::Minimal);
        }
        self
    }

    fn shown(self, model: &Model) -> Self {
        let fields = model.effective_thinking_fields();
        if let Some((fields, fallback)) = fields
            && let Some(shown) = declared_thinking(fields, self, model, fallback)
        {
            return shown;
        }
        if self == Self::Adaptive && fields.is_some_and(|(fields, _)| !fields.levels.is_empty()) {
            return self;
        }
        model.thinking_sent(self)
    }
}

/// The parts of request options that read the model's provider registries.
pub trait ModelRequestOptions {
    /// Reconciles options with the model's capabilities. Called once before
    /// every request so UI state, restored sessions, and subagent flags all go
    /// through the same gate.
    fn clamped(self, model: &Model) -> Self;
}

impl ModelRequestOptions for RequestOptions {
    fn clamped(self, model: &Model) -> Self {
        Self {
            thinking: self.thinking.clamped(model),
            fast: self.fast && model.supports_fast(),
        }
    }
}

/// A change the API made to the request before the model read it, such as a
/// thinking block dropped because the prefix it was bound to changed. Kinds
/// and reasons stay strings, because the API keeps adding new ones.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InputTransformation {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct StreamResponse {
    pub message: Message,
    pub usage: TokenUsage,
    pub stop_reason: Option<StopReason>,
    pub input_transformations: Vec<InputTransformation>,
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use maki_storage::sessions::TitleSource;

    use super::*;
    use crate::model::ThinkingSupport as Support;
    use test_case::test_case;

    /// Ollama is the one path that pairs a dialect with per-model fields.
    const DIALECT: ThinkingFallback = ThinkingFallback::Dialect(&dialect::OLLAMA);

    const UNKNOWN_DIALECT: &str = "name in NAMES must resolve";

    /// Valid ASCII, but no image ever started with these bytes.
    const UNREADABLE_PAYLOAD: &str = "abc123";
    /// Below `Minimal` against [`FALLBACK_MAX_THINKING_BUDGET`].
    const SMALL_BUDGET: u32 = 2048;
    /// Between `Medium` and `High` against [`FALLBACK_MAX_THINKING_BUDGET`].
    const LARGE_BUDGET: u32 = 16_384;

    fn png_block(edge: usize) -> ContentBlock {
        let data = crate::image::png_base64(edge as u32, edge as u32);
        ContentBlock::Image {
            source: ImageSource::new(ImageMediaType::Png, Arc::from(data)),
        }
    }

    fn unreadable_block() -> ContentBlock {
        ContentBlock::Image {
            source: ImageSource::new(ImageMediaType::Png, Arc::from(UNREADABLE_PAYLOAD)),
        }
    }

    async fn adapt(model: &Model, content: Vec<ContentBlock>) -> Vec<ContentBlock> {
        let messages = vec![Message {
            role: Role::User,
            content,
            ..Default::default()
        }];
        adapt_images_for_model(model, &messages).await[0]
            .content
            .clone()
    }

    fn image_count(blocks: &[ContentBlock]) -> usize {
        blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .count()
    }

    #[tokio::test]
    async fn adapt_images_borrows_when_nothing_has_to_change() {
        let model = clamp_test_model(anthropic_spec());
        let with_image = vec![Message {
            role: Role::User,
            content: vec![png_block(32)],
            ..Default::default()
        }];
        assert!(matches!(
            adapt_images_for_model(&model, &with_image).await,
            Cow::Borrowed(_)
        ));

        let mut text_only_model = model;
        text_only_model.supports_vision_override = Some(false);
        let no_images = vec![Message::user("hi".into())];
        assert!(matches!(
            adapt_images_for_model(&text_only_model, &no_images).await,
            Cow::Borrowed(_)
        ));
    }

    /// A repaired image is repaired for the rest of the session, so this path
    /// is walked on every later turn: the messages the repair does not reach
    /// must come out of it untouched, and history must keep its own pixels.
    #[tokio::test]
    async fn adapt_images_rebuilds_only_the_messages_that_change() {
        const CAPTION: &str = "look";
        const OVERSIZED: u32 = 2600;
        let model = clamp_test_model(anthropic_spec());
        let fine = ImageSource::new(
            ImageMediaType::Png,
            Arc::from(crate::image::png_base64(32, 32)),
        );
        let carries = |content: Vec<ContentBlock>| Message {
            role: Role::User,
            content,
            ..Default::default()
        };
        let history = vec![
            Message::user(CAPTION.into()),
            carries(vec![ContentBlock::Image {
                source: fine.clone(),
            }]),
            carries(vec![
                ContentBlock::Text {
                    text: CAPTION.into(),
                },
                ContentBlock::Image {
                    source: ImageSource::new(
                        ImageMediaType::Png,
                        Arc::from(crate::image::png_base64(OVERSIZED, 30)),
                    ),
                },
            ]),
        ];

        let adapted = adapt_images_for_model(&model, &history).await;
        assert!(matches!(adapted, Cow::Owned(_)), "the repair needs a copy");
        assert_eq!(adapted[0].first_user_text(), Some(CAPTION));
        let ContentBlock::Image { source } = &adapted[1].content[0] else {
            panic!("an image nothing is wrong with must stay an image");
        };
        assert!(
            Arc::ptr_eq(&source.data, &fine.data),
            "an untouched image must not be re-encoded"
        );
        assert!(
            matches!(&adapted[2].content[0], ContentBlock::Text { text } if text == CAPTION),
            "the blocks beside a repaired one must survive"
        );
        let ContentBlock::Image { source } = &history[2].content[1] else {
            panic!("history must keep its image block");
        };
        assert_eq!(
            crate::image::dimensions(source),
            (OVERSIZED, 30),
            "only the copy going out is rewritten"
        );
    }

    #[tokio::test]
    async fn adapt_images_shrinks_what_a_provider_would_refuse() {
        let model = clamp_test_model(anthropic_spec());
        let oversized = ContentBlock::Image {
            source: ImageSource::new(
                ImageMediaType::Png,
                Arc::from(crate::image::png_base64(2600, 30)),
            ),
        };
        let text = ContentBlock::Text {
            text: "look".into(),
        };
        let blocks = adapt(&model, vec![text, oversized]).await;
        assert!(matches!(&blocks[0], ContentBlock::Text { .. }));
        let ContentBlock::Image { source } = &blocks[1] else {
            panic!("an image block must stay an image block");
        };
        let (width, height) = crate::image::dimensions(source);
        assert!(
            width.max(height) <= crate::image::MAX_EDGE,
            "{width}x{height}"
        );
    }

    /// Evicting edits early messages, which voids the cache and the thinking
    /// bound to them. So each new image must leave the request an append of
    /// the one before, except at most once per step when the oldest images
    /// make way, and the API cap is never crossed.
    #[tokio::test]
    async fn adapt_images_moves_the_prefix_once_per_step() {
        const LAST_TOTAL: usize = MAX_IMAGES + 2 * IMAGE_EVICTION_STEP + 1;
        let model = clamp_test_model(anthropic_spec());
        let images: Vec<ContentBlock> = (1..=LAST_TOTAL).map(png_block).collect();
        let wire = |blocks: &[ContentBlock]| serde_json::to_value(blocks).unwrap();
        let mut before = adapt(&model, images[..MAX_IMAGES].to_vec()).await;
        let mut last_move: Option<usize> = None;
        for total in MAX_IMAGES + 1..=LAST_TOTAL {
            let after = adapt(&model, images[..total].to_vec()).await;
            let kept = image_count(&after);
            assert!(kept <= MAX_IMAGES, "{kept} images sent");
            assert!(
                kept > MAX_IMAGES - IMAGE_EVICTION_STEP,
                "{kept} of {total} images kept"
            );
            assert!(
                after[..total - kept].iter().all(
                    |b| matches!(b, ContentBlock::Text { text } if text == IMAGE_EVICTED_NOTE)
                ),
                "the oldest images are the ones that make way"
            );
            if wire(&after[..total - 1]) != wire(&before) {
                assert!(
                    last_move.is_none_or(|at| total - at >= IMAGE_EVICTION_STEP),
                    "prefix moved at image {total}, last moved at {last_move:?}"
                );
                last_move = Some(total);
            }
            before = after;
        }
    }

    /// Readability must not decide what is evicted: an unreadable image that
    /// freed its slot would pull an evicted one back in, editing an early
    /// message outside the step schedule.
    #[tokio::test]
    async fn adapt_images_evicts_the_same_images_whatever_decodes() {
        let model = clamp_test_model(anthropic_spec());
        let readable: Vec<ContentBlock> = (1..=MAX_IMAGES + 1).map(png_block).collect();
        let mut content = readable.clone();
        content[MAX_IMAGES] = unreadable_block();
        let evicted = |blocks: &[ContentBlock]| -> Vec<usize> {
            blocks
                .iter()
                .enumerate()
                .filter(
                    |(_, b)| matches!(b, ContentBlock::Text { text } if text == IMAGE_EVICTED_NOTE),
                )
                .map(|(i, _)| i)
                .collect()
        };
        let blocks = adapt(&model, content).await;
        assert_eq!(evicted(&blocks), evicted(&adapt(&model, readable).await));
        assert!(
            matches!(&blocks[MAX_IMAGES], ContentBlock::Text { text } if text == IMAGE_UNUSABLE_NOTE)
        );
    }

    #[tokio::test]
    async fn adapt_images_replaces_blocks_for_text_only_model() {
        let mut model = clamp_test_model(anthropic_spec());
        model.supports_vision_override = Some(false);
        let tool_result = ContentBlock::tool_result("t1", "[image: pic.png 1KB]", false);
        let blocks = adapt(&model, vec![tool_result, unreadable_block()]).await;
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], ContentBlock::ToolResult { .. }));
        assert!(matches!(&blocks[1], ContentBlock::Text { text } if text == IMAGE_OMITTED_NOTE));
    }

    use Effort::{High, Low, Max, Medium, Minimal, XHigh};

    /// `max_output_tokens: 8192`, so `max_thinking_budget()` is 4096.
    fn thinking_model(id: &str) -> crate::model::Model {
        crate::model::Model {
            id: id.into(),
            ..clamp_test_model(anthropic_spec())
        }
    }

    fn native_thinking_model(id: &str, fields: Value) -> crate::model::Model {
        let mut model = thinking_model(id);
        model.provider = Arc::from("llama-cpp");
        model.thinking_fields = Some(Box::new(serde_json::from_value(fields).unwrap()));
        model
    }

    fn native_effort_model() -> crate::model::Model {
        native_thinking_model(
            "local-model",
            json!({
                "off": {"reasoning_effort": "none"},
                "adaptive": {"reasoning_effort": "medium"},
                "low": {"reasoning_effort": "low"},
                "medium": {"reasoning_effort": "medium"},
                "xhigh": {"reasoning_effort": "xhigh"}
            }),
        )
    }

    /// `Effort::snap` walks `supported` expecting it sorted.
    #[test]
    fn every_dialect_is_well_formed_and_named() {
        for name in dialect::NAMES {
            let d = dialect::by_name(name).expect(UNKNOWN_DIALECT);
            assert!(!d.supported.is_empty(), "{name} supports nothing");
            for pair in d.supported.windows(2) {
                assert!(pair[0] < pair[1], "{name} is not strictly ascending");
            }
            if let Some(adaptive) = d.adaptive {
                let supported = d.supported.contains(&adaptive);
                assert!(supported, "{name} adaptive is not a supported level");
            }
            assert_eq!(dialect::name_of(d), Some(*name));
        }
    }

    #[test_case(ThinkingConfig::Off, "claude-opus-4-5", json!({}) ; "off")]
    #[test_case(ThinkingConfig::Adaptive, "claude-opus-4-5", json!({"thinking": {"type": "adaptive"}}) ; "adaptive")]
    #[test_case(ThinkingConfig::Budget(2048), "claude-opus-4-5", json!({"thinking": {"type": "enabled", "budget_tokens": 2048}}) ; "budget_legacy_in_range")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-4-5", json!({"thinking": {"type": "enabled", "budget_tokens": 4096}}) ; "budget_legacy_clamped_to_max")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-sonnet-4-6", json!({"thinking": {"type": "enabled", "budget_tokens": 4096}}) ; "budget_legacy_sonnet")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-4-6", json!({"thinking": {"type": "enabled", "budget_tokens": 4096}}) ; "budget_legacy_opus_4_6")]
    #[test_case(ThinkingConfig::Off, "claude-opus-4-7", json!({}) ; "off_adaptive_model")]
    #[test_case(ThinkingConfig::Adaptive, "claude-opus-4-7", json!({"thinking": {"type": "adaptive", "display": "summarized"}}) ; "adaptive_adaptive_model")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-opus-4-7", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "max"}}) ; "budget_adaptive_opus_4_7")]
    #[test_case(ThinkingConfig::Effort(Low), "claude-opus-4-7", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "low"}}) ; "effort_low_passthrough")]
    #[test_case(ThinkingConfig::Budget(10000), "claude-sonnet-5", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "max"}}) ; "budget_adaptive_sonnet_5")]
    #[test_case(ThinkingConfig::Effort(Max), "claude-opus-5.5", json!({"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "max"}}) ; "effort_max_passthrough")]
    fn thinking_apply_to_body(config: ThinkingConfig, model_id: &str, expected: Value) {
        let mut body = json!({});
        config.apply_to_body(&mut body, &thinking_model(model_id));
        assert_eq!(body, expected);
    }

    #[test_case("claude-opus-4-7", Some(("opus", (4, 7))) ; "anthropic")]
    #[test_case("claude-opus-4.7", Some(("opus", (4, 7))) ; "copilot_dotted")]
    #[test_case("claude-opus-5-1m", Some(("opus", (5, 0))) ; "long_context_suffix")]
    #[test_case("anthropic/claude-opus-4-7", Some(("opus", (4, 7))) ; "gateway_prefix")]
    #[test_case("anthropic/claude-opus-5:thinking", Some(("opus", (5, 0))) ; "openrouter_variant")]
    #[test_case("anthropic.claude-opus-5-5", Some(("opus", (5, 5))) ; "bedrock")]
    #[test_case("us.anthropic.claude-sonnet-5-v1:0", Some(("sonnet", (5, 0))) ; "bedrock_region")]
    #[test_case("claude-opus-4-8@20260101", Some(("opus", (4, 8))) ; "vertex_snapshot")]
    #[test_case("claude-opus-4-20250514", Some(("opus", (4, 0))) ; "date_is_not_a_minor")]
    #[test_case("claude-3-5-sonnet-20241022", None ; "legacy_version_first")]
    #[test_case("gpt-5", None ; "not_claude")]
    fn claude_version_parses(model_id: &str, expected: Option<(&str, (u32, u32))>) {
        assert_eq!(claude_version(model_id), expected);
    }

    const TRIMMED_TURN: u32 = 8_192;
    const REWRITTEN_CAP: u32 = 131_072;

    /// Providers that resolve limits per request rewrite `max_output_tokens`
    /// after the agent sized the turn (catalog, opencode). Cutting the budget
    /// to what the `max_tokens` on the wire can house means no rewrite leaves a
    /// request asking to think for longer than it may answer, which Anthropic
    /// refuses outright and every other dialect answers with nothing but
    /// thinking.
    #[test_case(ThinkingConfig::Effort(Max) ; "the_top_effort_level")]
    #[test_case(ThinkingConfig::Effort(Minimal) ; "the_lowest")]
    #[test_case(ThinkingConfig::Budget(LARGE_BUDGET) ; "an_explicit_budget")]
    fn a_provider_that_rewrites_the_output_cap_cannot_unbound_the_thinking(config: ThinkingConfig) {
        let model = crate::model::Model {
            turn_output_tokens: Some(TRIMMED_TURN),
            max_output_tokens: Some(REWRITTEN_CAP),
            ..thinking_model("claude-sonnet-4-20250514")
        };
        let budget = config.request_thinking(&model).expect("a budget is sent");

        assert!(
            budget * 2 <= TRIMMED_TURN,
            "{budget} thinking tokens under a {TRIMMED_TURN} token cap"
        );
    }

    #[test_case(&dialect::STANDARD, ThinkingConfig::Off,             None            ; "standard_off_noop")]
    #[test_case(&dialect::STANDARD, ThinkingConfig::Adaptive,        Some("medium")  ; "standard_adaptive")]
    #[test_case(&dialect::STANDARD, ThinkingConfig::Effort(Minimal), Some("minimal") ; "standard_minimal_passthrough")]
    #[test_case(&dialect::STANDARD, ThinkingConfig::Effort(Max),     Some("high")    ; "standard_max_snaps_down")]
    #[test_case(&dialect::STANDARD, ThinkingConfig::Budget(1024),    Some("medium")  ; "standard_quarter_budget")]
    #[test_case(&dialect::CODEX, ThinkingConfig::Adaptive,        Some("medium") ; "codex_adaptive")]
    #[test_case(&dialect::CODEX, ThinkingConfig::Effort(Minimal), Some("low")    ; "codex_minimal_snaps_up")]
    #[test_case(&dialect::CODEX, ThinkingConfig::Effort(Max),     Some("xhigh")  ; "codex_max_snaps_down")]
    #[test_case(&dialect::CODING_PLAN, ThinkingConfig::Effort(Minimal), Some("minimal") ; "coding_plan_minimal_passthrough")]
    #[test_case(&dialect::CODING_PLAN, ThinkingConfig::Effort(Max),     Some("xhigh")   ; "coding_plan_max_snaps_down")]
    #[test_case(&dialect::CODING_PLAN, ThinkingConfig::Off,             Some("none")    ; "coding_plan_off")]
    #[test_case(&dialect::GPT_5_6, ThinkingConfig::Off,                 Some("none")    ; "gpt_5_6_off")]
    #[test_case(&dialect::PREFER_HIGH, ThinkingConfig::Adaptive,        Some("high") ; "prefer_high_adaptive")]
    #[test_case(&dialect::HIGH_ONLY, ThinkingConfig::Adaptive,        Some("high") ; "high_only_adaptive")]
    #[test_case(&dialect::HIGH_ONLY, ThinkingConfig::Effort(Minimal), Some("high") ; "high_only_minimal")]
    #[test_case(&dialect::GLM, ThinkingConfig::Off,          Some("none")  ; "glm_off_explicit_none")]
    #[test_case(&dialect::GLM, ThinkingConfig::Adaptive,     Some("high")  ; "glm_adaptive")]
    #[test_case(&dialect::GLM, ThinkingConfig::Effort(Max),  Some("xhigh") ; "glm_max_snaps_to_xhigh")]
    #[test_case(&dialect::DEEPSEEK, ThinkingConfig::Adaptive,        None        ; "deepseek_adaptive_uses_api_default")]
    #[test_case(&dialect::DEEPSEEK, ThinkingConfig::Effort(Minimal), Some("max") ; "deepseek_minimal")]
    #[test_case(&dialect::ANTHROPIC_ADAPTIVE, ThinkingConfig::Adaptive,      None         ; "anthropic_adaptive_is_native")]
    #[test_case(&dialect::ANTHROPIC_ADAPTIVE, ThinkingConfig::Effort(XHigh), Some("xhigh") ; "anthropic_xhigh_passthrough")]
    #[test_case(&dialect::ANTHROPIC_ADAPTIVE, ThinkingConfig::Effort(Minimal), Some("low") ; "anthropic_minimal_snaps_up")]
    #[test_case(&dialect::TENSORX, ThinkingConfig::Off,             Some("none") ; "tensorx_off_explicit_none")]
    #[test_case(&dialect::OLLAMA, ThinkingConfig::Off,             Some("none")   ; "ollama_off_explicit_none")]
    #[test_case(&dialect::OLLAMA, ThinkingConfig::Adaptive,        Some("medium") ; "ollama_adaptive")]
    #[test_case(&dialect::OLLAMA, ThinkingConfig::Effort(Minimal), Some("low")    ; "ollama_minimal_snaps_up")]
    #[test_case(&dialect::OLLAMA, ThinkingConfig::Effort(Max),     Some("high")   ; "ollama_max_snaps_down")]
    fn thinking_apply_reasoning_effort(
        dialect: &EffortDialect,
        config: ThinkingConfig,
        expected: Option<&str>,
    ) {
        let mut body = json!({"model": "test"});
        config.apply_reasoning_effort(&mut body, dialect, &thinking_model("test-model"));
        match expected {
            Some(e) => assert_eq!(body["reasoning_effort"], e),
            None => assert!(body.get("reasoning_effort").is_none()),
        }
    }

    /// The model spells out `high` and nothing else. That one wins, and every
    /// other mode has to come from the fallback, or ollama keeps reasoning
    /// after the user turned thinking off. A generic gateway has no dialect to
    /// guess at, so there it stays quiet.
    #[test_case(ThinkingConfig::Effort(High), DIALECT, json!({"reasoning_effort": "xhigh"}) ; "declared_mode_uses_its_fragment")]
    #[test_case(ThinkingConfig::Off, DIALECT, json!({"reasoning_effort": "none"}) ; "unspelled_mode_falls_back_to_the_dialect")]
    #[test_case(ThinkingConfig::Off, ThinkingFallback::None, json!({}) ; "unspelled_mode_stays_quiet_without_one")]
    fn thinking_fields_come_first(
        thinking: ThinkingConfig,
        fallback: ThinkingFallback,
        expected: Value,
    ) {
        let model =
            native_thinking_model("partial", json!({"high": {"reasoning_effort": "xhigh"}}));
        let mut body = json!({});
        thinking.apply_thinking(&mut body, &model, fallback);
        assert_eq!(body, expected);
    }

    /// Ollama turns thinking on by itself, so "off" has to be said out loud,
    /// but only to models that can think: the rest reject the field.
    #[test_case(Support::Yes, json!({"reasoning_effort": "none"}) ; "capable_model_is_told_to_stop")]
    #[test_case(Support::No, json!({}) ; "model_that_cannot_think_is_left_alone")]
    fn dialect_fallback_follows_thinking_support(support: Support, expected: Value) {
        let mut model = thinking_model("ollama-model");
        model.thinking_override = Some(support);
        let mut body = json!({});
        ThinkingConfig::Off.apply_thinking(&mut body, &model, DIALECT);
        assert_eq!(body, expected);
    }

    /// Google's own documented ceiling on thinking, which is a capability and
    /// so part of resolving the level.
    const GOOGLE_CAP: u32 = 8192;
    /// Roomy enough that the request ceiling is not what binds.
    const ROOMY_OUTPUT: Option<u32> = Some(65_536);

    #[test_case(ThinkingConfig::Off, "gemini-2.5-pro", ROOMY_OUTPUT, json!({})                                                                  ; "off")]
    #[test_case(ThinkingConfig::Adaptive, "gemini-2.5-pro", ROOMY_OUTPUT, json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true}}}) ; "adaptive")]
    #[test_case(ThinkingConfig::Budget(4096), "gemini-2.5-pro", ROOMY_OUTPUT, json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 4096}}}) ; "budget")]
    #[test_case(ThinkingConfig::Budget(10000), "gemini-2.5-pro", ROOMY_OUTPUT, json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 8192}}}) ; "budget_clamped_to_googles_cap")]
    // Half the `maxOutputTokens` the same request carries, or the answer has
    // nowhere to land.
    #[test_case(ThinkingConfig::Budget(10000), "gemini-2.5-pro", Some(GOOGLE_CAP), json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 4096}}}) ; "budget_cut_to_what_the_request_can_house")]
    #[test_case(ThinkingConfig::Off, "gemini-4-argon", ROOMY_OUTPUT, json!({}) ; "off_level_model")]
    #[test_case(ThinkingConfig::Adaptive, "gemini-4-argon", ROOMY_OUTPUT, json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true}}}) ; "adaptive_level_model")]
    #[test_case(ThinkingConfig::Effort(Minimal), "gemini-4-argon", ROOMY_OUTPUT, json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "low"}}}) ; "minimal_snaps_up_to_low_on_level_model")]
    #[test_case(ThinkingConfig::Effort(High), "gemini-4-argon", ROOMY_OUTPUT, json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}}}) ; "high_level_model")]
    #[test_case(ThinkingConfig::Effort(XHigh), "gemini-3.6-flash", ROOMY_OUTPUT, json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}}}) ; "xhigh_snaps_down_to_high_on_gemini_3")]
    #[test_case(ThinkingConfig::Budget(32768), "gemini-pro-latest", ROOMY_OUTPUT, json!({"generationConfig": {"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}}}) ; "budget_maps_to_level_on_latest_alias")]
    fn thinking_apply_google_thinking(
        config: ThinkingConfig,
        model_id: &str,
        max_output_tokens: Option<u32>,
        expected: Value,
    ) {
        let mut body = json!({});
        let model = crate::model::Model {
            max_output_tokens,
            ..thinking_model(model_id)
        };
        config.apply_google_thinking(&mut body, &model, GOOGLE_CAP);
        assert_eq!(body, expected);
    }

    #[test_case(ThinkingConfig::Off,            0    ; "off")]
    #[test_case(ThinkingConfig::Adaptive,       -1   ; "adaptive")]
    #[test_case(ThinkingConfig::Budget(4096),   4096 ; "budget")]
    #[test_case(ThinkingConfig::Budget(10000),  4096 ; "budget_clamped")]
    fn thinking_apply_budget_field(config: ThinkingConfig, expected: i64) {
        let mut body = json!({});
        config.apply_thinking(
            &mut body,
            &thinking_model("local-model"),
            ThinkingFallback::BudgetField,
        );
        assert_eq!(body["thinking_budget_tokens"], expected);
    }

    #[test_case(ThinkingConfig::Off,           json!({"reasoning_effort": "none"})   ; "off")]
    #[test_case(ThinkingConfig::Adaptive,      json!({"reasoning_effort": "medium"}) ; "adaptive")]
    #[test_case(ThinkingConfig::Effort(Low),   json!({"reasoning_effort": "low"})    ; "low")]
    #[test_case(ThinkingConfig::Effort(High),  json!({"reasoning_effort": "medium"}) ; "undeclared_high_snaps_down")]
    #[test_case(ThinkingConfig::Effort(XHigh), json!({"reasoning_effort": "xhigh"})  ; "xhigh")]
    #[test_case(ThinkingConfig::Budget(4096),  json!({"reasoning_effort": "xhigh"})  ; "numeric_budget_maps_to_declared_level")]
    fn local_native_effort_uses_declared_levels(config: ThinkingConfig, expected: Value) {
        let mut body = json!({});
        config.apply_thinking(
            &mut body,
            &native_effort_model(),
            ThinkingFallback::BudgetField,
        );
        assert_eq!(body, expected);
    }

    #[test_case(ThinkingConfig::Effort(Minimal), ThinkingConfig::Effort(Low) ; "below_lowest")]
    #[test_case(ThinkingConfig::Effort(Low), ThinkingConfig::Effort(Low) ; "declared_level")]
    #[test_case(ThinkingConfig::Effort(High), ThinkingConfig::Effort(Medium) ; "gap_snaps_down")]
    #[test_case(ThinkingConfig::Effort(Max), ThinkingConfig::Effort(XHigh) ; "above_highest")]
    #[test_case(ThinkingConfig::Adaptive, ThinkingConfig::Adaptive ; "adaptive_stays")]
    #[test_case(ThinkingConfig::Off, ThinkingConfig::Off ; "off_stays")]
    #[test_case(ThinkingConfig::Budget(SMALL_BUDGET), ThinkingConfig::Effort(Medium) ; "budget_shows_its_native_level")]
    #[test_case(ThinkingConfig::Budget(LARGE_BUDGET), ThinkingConfig::Effort(XHigh) ; "large_budget_shows_highest_native_level")]
    fn shown_native_thinking_matches_the_encoded_level_without_rewriting_intent(
        requested: ThinkingConfig,
        expected: ThinkingConfig,
    ) {
        let model = native_effort_model();
        assert_eq!(requested.clamped(&model), requested);
        assert_eq!(requested.shown(&model), expected);
        let mut original = json!({});
        let mut normalized = json!({});
        requested.apply_thinking(&mut original, &model, ThinkingFallback::BudgetField);
        expected.apply_thinking(&mut normalized, &model, ThinkingFallback::BudgetField);
        assert_eq!(original, normalized);
    }

    #[test_case(Some(8192), SMALL_BUDGET, Medium ; "declared_output_cap")]
    #[test_case(None, SMALL_BUDGET, Low ; "unknown_cap_uses_encoder_fallback")]
    #[test_case(Some(8192), LARGE_BUDGET, XHigh ; "budget_above_cap")]
    fn typed_native_budget_keeps_its_ceiling_but_shows_the_encoded_level(
        max_output_tokens: Option<u32>,
        budget: u32,
        expected: Effort,
    ) {
        let mut model = native_effort_model();
        model.max_output_tokens = max_output_tokens;
        let thinking = ThinkingConfig::Budget(budget);
        assert_eq!(thinking.clamped(&model), thinking);
        assert_eq!(thinking.shown(&model), ThinkingConfig::Effort(expected));
        let mut body = json!({});
        thinking.apply_thinking(&mut body, &model, ThinkingFallback::BudgetField);
        assert_eq!(body, json!({"reasoning_effort": expected.as_str()}));
    }

    #[test]
    fn local_required_thinking_maps_off_to_lowest_native_effort() {
        let mut model = native_effort_model();
        model.thinking_override = Some(Support::Required);
        let thinking = RequestOptions {
            thinking: ThinkingConfig::Off,
            fast: false,
        }
        .clamped(&model)
        .thinking;
        assert_eq!(thinking, ThinkingConfig::Effort(Minimal));
        assert_eq!(thinking.shown(&model), ThinkingConfig::Effort(Low));
        let mut body = json!({});
        thinking.apply_thinking(&mut body, &model, ThinkingFallback::BudgetField);
        assert_eq!(body, json!({"reasoning_effort": "low"}));
    }

    #[test_case(ThinkingConfig::Off,          json!({"chat_template_kwargs": {"enable_thinking": false, "keep": 1}}) ; "off")]
    #[test_case(ThinkingConfig::Adaptive,     json!({"chat_template_kwargs": {"enable_thinking": true, "keep": 1}})  ; "adaptive")]
    #[test_case(ThinkingConfig::Effort(High), json!({"chat_template_kwargs": {"enable_thinking": true, "keep": 1}})  ; "effort_without_levels_uses_adaptive")]
    #[test_case(ThinkingConfig::Budget(2048), json!({"chat_template_kwargs": {"enable_thinking": true, "keep": 1}, "thinking_budget_tokens": 2048}) ; "numeric_budget")]
    fn local_native_toggle_merges_into_nested_object(config: ThinkingConfig, expected: Value) {
        let model = native_thinking_model(
            "local-toggle-model",
            json!({
                "off": {"chat_template_kwargs": {"enable_thinking": false}},
                "adaptive": {"chat_template_kwargs": {"enable_thinking": true}}
            }),
        );
        let mut body = json!({"chat_template_kwargs": {"keep": 1}});
        config.apply_thinking(&mut body, &model, ThinkingFallback::BudgetField);
        assert_eq!(body, expected);
    }

    #[test_case("llama-cpp", None, ThinkingConfig::Budget(LARGE_BUDGET), Some(4096) ; "llama_cpp_sends_a_clamped_count")]
    #[test_case("llama-cpp", Some(SMALL_BUDGET), ThinkingConfig::Budget(LARGE_BUDGET), Some(1024) ; "llama_cpp_respects_the_turn_cap")]
    #[test_case("ollama", None, ThinkingConfig::Adaptive, None ; "ollama_sends_only_the_toggle")]
    #[test_case("openai", None, ThinkingConfig::Adaptive, None ; "openai_chat_sends_only_the_toggle")]
    fn toggle_budget_is_shown_only_when_the_adapter_forwards_it(
        provider: &str,
        turn_output_tokens: Option<u32>,
        expected: ThinkingConfig,
        sent_budget: Option<u32>,
    ) {
        let mut model = native_thinking_model(
            "local-toggle-model",
            json!({"adaptive": {"enable_thinking": true}}),
        );
        model.provider = Arc::from(provider);
        model.thinking_override = Some(Support::Yes);
        model.turn_output_tokens = turn_output_tokens;
        let thinking = ThinkingConfig::Budget(LARGE_BUDGET);
        assert_eq!(thinking.clamped(&model), thinking);
        assert_eq!(thinking.shown(&model), expected);
        let (_, fallback) = model.effective_thinking_fields().unwrap();
        let mut body = json!({});
        thinking.apply_thinking(&mut body, &model, fallback);
        let mut expected_body = json!({"enable_thinking": true});
        if let Some(budget) = sent_budget {
            expected_body[LOCAL_BUDGET_FIELD] = json!(budget);
        }
        assert_eq!(body, expected_body);
    }

    #[test_case(Support::Yes, ThinkingConfig::Off, ThinkingConfig::Off ; "optional_off")]
    #[test_case(Support::Required, ThinkingConfig::Off, ThinkingConfig::Adaptive ; "required_off_enables_toggle")]
    #[test_case(Support::Yes, ThinkingConfig::Effort(High), ThinkingConfig::Adaptive ; "effort_uses_toggle")]
    #[test_case(Support::Yes, ThinkingConfig::Adaptive, ThinkingConfig::Adaptive ; "adaptive_stays")]
    #[test_case(Support::Yes, ThinkingConfig::Budget(SMALL_BUDGET), ThinkingConfig::Budget(SMALL_BUDGET) ; "explicit_budget_stays")]
    #[test_case(Support::No, ThinkingConfig::Adaptive, ThinkingConfig::Off ; "unsupported_stays_off")]
    fn shown_toggle_thinking_uses_only_the_declared_modes(
        support: Support,
        requested: ThinkingConfig,
        expected: ThinkingConfig,
    ) {
        let mut model = native_thinking_model(
            "local-toggle-model",
            json!({
                "off": {"enable_thinking": false},
                "adaptive": {"enable_thinking": true}
            }),
        );
        model.thinking_override = Some(support);
        let clamped = requested.clamped(&model);
        assert_eq!(clamped.shown(&model), expected);
    }

    /// A mode the model has no fragment for must still reach the server, so
    /// the budget field takes over instead of the request saying nothing.
    #[test_case(json!({"low": {"reasoning_effort": "low"}}), ThinkingConfig::Off, 0 ; "off_without_off")]
    #[test_case(json!({"adaptive": {"enable_thinking": true}}), ThinkingConfig::Off, 0 ; "toggle_without_off")]
    #[test_case(json!({"off": {"reasoning_effort": "none"}}), ThinkingConfig::Adaptive, -1 ; "adaptive_without_adaptive")]
    #[test_case(json!({"off": {"reasoning_effort": "none"}}), ThinkingConfig::Budget(4096), 4096 ; "budget_without_levels")]
    fn local_native_missing_fragment_falls_back_to_budget(
        fields: Value,
        config: ThinkingConfig,
        expected: i64,
    ) {
        let model = native_thinking_model("local-partial", fields);
        let mut body = json!({});
        config.apply_thinking(&mut body, &model, ThinkingFallback::BudgetField);
        assert_eq!(body, json!({ "thinking_budget_tokens": expected }));
    }

    /// llama.cpp models have no known output window; the budget the user
    /// asked for must reach the server untouched.
    #[test]
    fn local_thinking_unknown_window_passes_budget_through() {
        let mut model = thinking_model("llama-cpp-model");
        model.max_output_tokens = None;
        let mut body = json!({});
        ThinkingConfig::Budget(16_384).apply_thinking(
            &mut body,
            &model,
            ThinkingFallback::BudgetField,
        );
        assert_eq!(body["thinking_budget_tokens"], 16_384);
    }

    fn discovered_ollama(model_id: &str, supported: &[Effort], send_off: bool) -> Model {
        use crate::model::{ModelEffort, ModelInfo};

        crate::model_registry::set_known_models(
            "ollama",
            vec![ModelInfo {
                supports_thinking: Some(true),
                effort: Some(ModelEffort {
                    supported: supported.to_vec(),
                    send_off: Some(send_off),
                }),
                ..ModelInfo::id_only(model_id.into())
            }],
        );
        Model::from_spec(&format!("ollama/{model_id}")).unwrap()
    }

    /// The badge has to name what goes out, or a level the model does not
    /// take would show as itself while another one is sent.
    #[test_case(&[Low, Medium, High, XHigh] ; "named_levels")]
    #[test_case(&[Medium] ; "toggle")]
    fn ollama_shows_the_level_on_the_wire(supported: &[Effort]) {
        let model = discovered_ollama("badge", supported, true);

        for choice in Effort::ALL
            .map(ThinkingConfig::Effort)
            .into_iter()
            .chain([ThinkingConfig::Adaptive])
        {
            let mut body = json!({});
            choice.apply_thinking(&mut body, &model, DIALECT);
            let shown = choice.shown(&model).level().map(Effort::as_str);
            assert_eq!(body["reasoning_effort"].as_str(), shown, "{choice:?}");
        }
    }

    /// A model that cannot stop thinking gets nothing for off, rather than a
    /// "none" it would ignore.
    #[test_case(true, json!({"reasoning_effort": "none"}) ; "can_turn_off")]
    #[test_case(false, json!({}) ; "cannot_turn_off")]
    fn ollama_off_follows_discovery(send_off: bool, expected: Value) {
        let model = discovered_ollama("off", &[Low, Medium, High], send_off);
        let mut body = json!({});
        ThinkingConfig::Off.apply_thinking(&mut body, &model, DIALECT);
        assert_eq!(body, expected);
    }

    fn anthropic_spec() -> &'static crate::spec::ProviderSpec {
        crate::spec::ProviderRegistry::get("anthropic").unwrap()
    }

    fn google_spec() -> &'static crate::spec::ProviderSpec {
        crate::spec::ProviderRegistry::get("google").unwrap()
    }

    fn clamp_test_model(spec: &'static crate::spec::ProviderSpec) -> crate::model::Model {
        crate::model::Model {
            id: "test-model".into(),
            provider: std::sync::Arc::<str>::from(spec.slug),
            tier: crate::model::ModelTier::Medium,
            family: spec.family,
            supports_tool_examples_override: None,
            thinking_override: None,
            supports_vision_override: Some(spec.family.supports_vision()),
            supports_fast_override: None,
            pricing: crate::model::ModelPricing::default(),
            subsidised_by: None,
            discovered_free: false,
            max_output_tokens: Some(8192),
            turn_output_tokens: None,
            context_window: 200_000,
            thinking_fields: None,
        }
    }

    #[test_case(None,                    ThinkingConfig::Adaptive, ThinkingConfig::Adaptive        ; "provider_default_keeps")]
    #[test_case(Some(Support::No),       ThinkingConfig::Adaptive, ThinkingConfig::Off             ; "unsupported_clamps_off")]
    #[test_case(Some(Support::Yes),      ThinkingConfig::Off,      ThinkingConfig::Off             ; "supported_keeps_off")]
    #[test_case(Some(Support::Required), ThinkingConfig::Off,      ThinkingConfig::Effort(Minimal) ; "required_raises_off_to_minimal")]
    #[test_case(Some(Support::Required), ThinkingConfig::Adaptive, ThinkingConfig::Adaptive        ; "required_keeps_enabled")]
    fn request_options_clamped_thinking(
        thinking_override: Option<Support>,
        thinking: ThinkingConfig,
        expected: ThinkingConfig,
    ) {
        let mut model = clamp_test_model(anthropic_spec());
        model.thinking_override = thinking_override;
        let opts = RequestOptions {
            thinking,
            fast: false,
        };
        assert_eq!(opts.clamped(&model).thinking, expected);
    }

    /// The picker and the status bar read the level, but the session keeps
    /// `Adaptive`: a stored level would cap subagents where `Adaptive` does not.
    #[test_case(ThinkingConfig::Adaptive,    ThinkingConfig::Effort(High) ; "adaptive_reads_as_the_fixed_level")]
    #[test_case(ThinkingConfig::Effort(Low), ThinkingConfig::Effort(Low)  ; "a_named_level_stays")]
    #[test_case(ThinkingConfig::Off,         ThinkingConfig::Off          ; "off_stays")]
    fn shown_resolves_adaptive_without_changing_what_is_stored(
        thinking: ThinkingConfig,
        expected: ThinkingConfig,
    ) {
        let mut model = clamp_test_model(crate::spec::ProviderRegistry::get("xai").unwrap());
        model.thinking_override = Some(Support::Yes);

        assert_eq!(thinking.shown(&model), expected);
        assert_eq!(thinking.clamped(&model), thinking);
    }

    /// A model that spells `adaptive` itself keeps the mode, whatever the
    /// provider's dialect would have sent.
    #[test_case("llama-cpp", json!({"high": {"reasoning_effort": "xhigh"}}), Some(Effort::Medium) ; "unspelled_takes_the_dialect_level")]
    #[test_case("llama-cpp", json!({"adaptive": {"reasoning_effort": "auto"}}), None ; "spelled_stays_adaptive")]
    #[test_case("zai", json!({"adaptive": {"reasoning_effort": "auto"}}), Some(Effort::Medium) ; "inactive_fields_do_not_hide_the_fixed_mode")]
    fn fallback_adaptive_effort_defers_to_the_models_own_fields(
        provider: &str,
        fields: Value,
        expected: Option<Effort>,
    ) {
        let mut model = native_thinking_model("partial", fields);
        model.provider = Arc::from(provider);
        let adaptive =
            |fallback: ThinkingFallback| fallback.sends(&model, ThinkingConfig::Adaptive).level();
        assert_eq!(adaptive(DIALECT), expected);
        assert_eq!(adaptive(ThinkingFallback::None), None);
    }

    #[test]
    fn request_options_clamped_fast_requires_model_support() {
        let model = clamp_test_model(google_spec());
        let opts = RequestOptions {
            thinking: ThinkingConfig::Off,
            fast: true,
        };
        assert!(!opts.clamped(&model).fast);
    }

    #[test]
    fn thinking_serde_no_signature_omits_field() {
        let block = ContentBlock::Thinking {
            thinking: "x".into(),
            signature: None,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert!(json.get("signature").is_none());
    }
}
