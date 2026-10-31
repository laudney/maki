//! What a model is and can do, as far as a display path needs to know.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelFamily {
    Claude,
    Generic,
    Gemini,
    Glm,
    Gpt,
    Synthetic,
}

impl ModelFamily {
    pub fn supports_tool_examples(self) -> bool {
        match self {
            ModelFamily::Claude | ModelFamily::Gpt | ModelFamily::Synthetic => true,
            ModelFamily::Generic | ModelFamily::Gemini | ModelFamily::Glm => false,
        }
    }

    /// Fallback for models missing from the static tables; per-model truth
    /// lives in `ModelEntry::supports_vision`.
    pub fn supports_vision(self) -> bool {
        matches!(self, Self::Claude | Self::Gpt | Self::Gemini)
    }
}

/// `Required` marks APIs that reject requests with thinking disabled;
/// `RequestOptions::clamped` raises `Off` to minimal effort for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingSupport {
    No,
    Yes,
    Required,
}

impl ThinkingSupport {
    /// `requires` wins: an API that rejects thinking-off requests
    /// necessarily supports thinking.
    pub fn from_flags(supports: Option<bool>, requires: bool) -> Option<Self> {
        match (requires, supports) {
            (true, _) => Some(Self::Required),
            (false, Some(true)) => Some(Self::Yes),
            (false, Some(false)) => Some(Self::No),
            (false, None) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastSupport {
    Pending,
    Supported,
    Unsupported,
}
