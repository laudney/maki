//! The thinking level a session asks for, and the options one request
//! carries, as a session stores them and a display path draws them.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use maki_storage::sessions::{Effort, MIN_THINKING_BUDGET, StoredThinking, THINKING_ADAPTIVE};

pub const THINKING_USAGE: &str =
    "Usage: /thinking [off|adaptive|minimal|low|medium|high|xhigh|max|<budget>]";

/// Effort levels are percentages, so they need a ceiling even when the model
/// never told us its output window. 32k matches common frontier thinking
/// caps. Explicit user budgets never go through this.
#[doc(hidden)]
pub const FALLBACK_MAX_THINKING_BUDGET: u32 = 32_768;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StoredThinking", into = "StoredThinking")]
pub enum ThinkingConfig {
    #[default]
    Off,
    Adaptive,
    Effort(Effort),
    Budget(u32),
}

/// Resolved thinking value for token-budget APIs.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budgeted {
    Off,
    Adaptive,
    Tokens(u32),
}

impl ThinkingConfig {
    pub fn level(self) -> Option<Effort> {
        match self {
            Self::Effort(level) => Some(level),
            Self::Off | Self::Adaptive | Self::Budget(_) => None,
        }
    }

    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// The token budget to send, clamped to `[MIN_THINKING_BUDGET, max]` here
    /// and nowhere else. An unknown `max` never caps: the user's number goes
    /// through as asked, and effort levels scale the fallback ceiling.
    #[doc(hidden)]
    pub fn budget(self, max: Option<u32>) -> Budgeted {
        match self {
            Self::Off => Budgeted::Off,
            Self::Adaptive => Budgeted::Adaptive,
            Self::Effort(e) => {
                Budgeted::Tokens(e.budget(max.unwrap_or(FALLBACK_MAX_THINKING_BUDGET)))
            }
            Self::Budget(n) => Budgeted::Tokens(match max {
                Some(max) => n.clamp(MIN_THINKING_BUDGET, max.max(MIN_THINKING_BUDGET)),
                None => n.max(MIN_THINKING_BUDGET),
            }),
        }
    }

    pub fn parse(input: &str, current: Self) -> Result<Self, &'static str> {
        if input.is_empty() {
            return Ok(if current.is_enabled() {
                Self::Off
            } else {
                Self::Adaptive
            });
        }
        StoredThinking::parse_setting(input)
            .map(Into::into)
            .map_err(|_| THINKING_USAGE)
    }

    /// Caps this config at `parent`. A subagent's thinking request is written
    /// by the model, not the user, so it may go down but never above what the
    /// parent session runs with. A parent of `Adaptive` set no ceiling at all,
    /// so the child gets whatever it asked for; but against a parent that did
    /// name a level or a count, a child's `Adaptive` is "let the model decide"
    /// with no upper bound, which sits above any concrete ceiling, so the
    /// parent's config wins. Concrete sides compare as token budgets, the only
    /// unit an effort level and an explicit count share; the winner keeps its
    /// original form either way.
    pub fn clamp_to(self, parent: Self) -> Self {
        match (parent.budget(None), self.budget(None)) {
            (Budgeted::Off, _) | (_, Budgeted::Off) => Self::Off,
            (Budgeted::Adaptive, _) => self,
            (_, Budgeted::Adaptive) => parent,
            (Budgeted::Tokens(ceiling), Budgeted::Tokens(asked)) => {
                if asked <= ceiling {
                    self
                } else {
                    parent
                }
            }
        }
    }

    pub fn status_label(self) -> Option<Cow<'static, str>> {
        match self {
            Self::Off => None,
            Self::Adaptive => Some(Cow::Borrowed(THINKING_ADAPTIVE)),
            Self::Effort(e) => Some(Cow::Borrowed(e.as_str())),
            Self::Budget(n) => Some(Cow::Owned(format!("{n} tokens"))),
        }
    }
}

impl std::fmt::Display for ThinkingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", StoredThinking::from(*self))
    }
}

impl From<StoredThinking> for ThinkingConfig {
    fn from(s: StoredThinking) -> Self {
        match s {
            StoredThinking::Off => Self::Off,
            StoredThinking::Adaptive => Self::Adaptive,
            StoredThinking::Effort { level } => Self::Effort(level),
            StoredThinking::Budget { tokens } => Self::Budget(tokens),
        }
    }
}

/// One place decides what silence means, so a session that never set a level, a
/// config without `always_thinking` and a frontend with no toggle all read it
/// the same way: off.
impl From<Option<StoredThinking>> for ThinkingConfig {
    fn from(s: Option<StoredThinking>) -> Self {
        s.map_or(Self::Off, Self::from)
    }
}

impl From<ThinkingConfig> for StoredThinking {
    fn from(c: ThinkingConfig) -> Self {
        match c {
            ThinkingConfig::Off => Self::Off,
            ThinkingConfig::Adaptive => Self::Adaptive,
            ThinkingConfig::Effort(e) => Self::Effort { level: e },
            ThinkingConfig::Budget(n) => Self::Budget { tokens: n },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestOptions {
    pub thinking: ThinkingConfig,
    /// Raw user preference, reconciled by `RequestOptions::clamped` before use.
    pub fast: bool,
}

#[cfg(test)]
mod tests {
    use maki_storage::sessions::Effort::{High, Low, Max, Minimal};
    use test_case::test_case;

    use super::*;

    /// Below `Minimal` against [`FALLBACK_MAX_THINKING_BUDGET`].
    const SMALL_BUDGET: u32 = 2048;
    /// Between `Medium` and `High` against [`FALLBACK_MAX_THINKING_BUDGET`].
    const LARGE_BUDGET: u32 = 16_384;

    /// The badge reads as whatever the session is set to, and stays quiet when
    /// thinking is off.
    #[test_case(ThinkingConfig::Off, None ; "off_shows_no_badge")]
    #[test_case(ThinkingConfig::Adaptive, Some("adaptive") ; "adaptive")]
    #[test_case(ThinkingConfig::Effort(High), Some("high") ; "effort_names_the_level")]
    #[test_case(ThinkingConfig::Budget(8192), Some("8192 tokens") ; "budget_keeps_its_unit")]
    fn thinking_status_label(config: ThinkingConfig, expected: Option<&str>) {
        assert_eq!(config.status_label().as_deref(), expected);
    }

    #[test_case(ThinkingConfig::Off,             Some(4096), Budgeted::Off            ; "off")]
    #[test_case(ThinkingConfig::Adaptive,        Some(4096), Budgeted::Adaptive       ; "adaptive")]
    #[test_case(ThinkingConfig::Effort(Max),     Some(4096), Budgeted::Tokens(4096)   ; "effort_delegates_to_level_budget")]
    #[test_case(ThinkingConfig::Budget(2048),    Some(4096), Budgeted::Tokens(2048)   ; "budget_in_range")]
    #[test_case(ThinkingConfig::Budget(512),     Some(4096), Budgeted::Tokens(1024)   ; "budget_floored")]
    #[test_case(ThinkingConfig::Budget(10000),   Some(4096), Budgeted::Tokens(4096)   ; "budget_clamped_to_max")]
    #[test_case(ThinkingConfig::Budget(2048),    Some(512),  Budgeted::Tokens(1024)   ; "tiny_max_raised_to_floor")]
    #[test_case(ThinkingConfig::Budget(16384),   None,       Budgeted::Tokens(16384)  ; "unknown_max_passes_budget_through")]
    #[test_case(ThinkingConfig::Budget(512),     None,       Budgeted::Tokens(1024)   ; "unknown_max_still_floors")]
    #[test_case(ThinkingConfig::Effort(Max),     None,       Budgeted::Tokens(32_768) ; "unknown_max_effort_scales_fallback")]
    #[test_case(ThinkingConfig::Effort(Minimal), None,       Budgeted::Tokens(3_276)  ; "unknown_max_minimal_effort")]
    fn thinking_budget_resolver(config: ThinkingConfig, max: Option<u32>, expected: Budgeted) {
        assert_eq!(config.budget(max), expected);
    }

    #[test_case(ThinkingConfig::Off, ThinkingConfig::Effort(Max), ThinkingConfig::Off ; "parent_off_wins_over_any_request")]
    #[test_case(ThinkingConfig::Effort(Max), ThinkingConfig::Off, ThinkingConfig::Off ; "child_may_always_turn_it_off")]
    #[test_case(ThinkingConfig::Adaptive, ThinkingConfig::Off, ThinkingConfig::Off ; "child_off_beats_an_unbounded_parent")]
    #[test_case(ThinkingConfig::Adaptive, ThinkingConfig::Effort(Max), ThinkingConfig::Effort(Max) ; "parent_adaptive_is_not_a_ceiling")]
    #[test_case(ThinkingConfig::Adaptive, ThinkingConfig::Adaptive, ThinkingConfig::Adaptive ; "parent_adaptive_lets_the_child_stay_adaptive")]
    #[test_case(ThinkingConfig::Effort(Minimal), ThinkingConfig::Adaptive, ThinkingConfig::Effort(Minimal) ; "child_adaptive_cannot_escape_a_parent_effort")]
    #[test_case(ThinkingConfig::Effort(Max), ThinkingConfig::Adaptive, ThinkingConfig::Effort(Max) ; "child_adaptive_yields_even_to_the_top_effort")]
    #[test_case(ThinkingConfig::Budget(SMALL_BUDGET), ThinkingConfig::Adaptive, ThinkingConfig::Budget(SMALL_BUDGET) ; "child_adaptive_cannot_escape_a_parent_budget")]
    #[test_case(ThinkingConfig::Effort(Low), ThinkingConfig::Effort(Max), ThinkingConfig::Effort(Low) ; "effort_capped_at_parent")]
    #[test_case(ThinkingConfig::Effort(Max), ThinkingConfig::Effort(Low), ThinkingConfig::Effort(Low) ; "effort_lower_child_kept")]
    #[test_case(ThinkingConfig::Budget(SMALL_BUDGET), ThinkingConfig::Budget(LARGE_BUDGET), ThinkingConfig::Budget(SMALL_BUDGET) ; "budget_capped_at_parent")]
    #[test_case(ThinkingConfig::Budget(LARGE_BUDGET), ThinkingConfig::Budget(SMALL_BUDGET), ThinkingConfig::Budget(SMALL_BUDGET) ; "budget_lower_child_kept")]
    #[test_case(ThinkingConfig::Effort(Minimal), ThinkingConfig::Budget(LARGE_BUDGET), ThinkingConfig::Effort(Minimal) ; "mixed_parent_effort_caps_child_budget")]
    #[test_case(ThinkingConfig::Effort(Max), ThinkingConfig::Budget(SMALL_BUDGET), ThinkingConfig::Budget(SMALL_BUDGET) ; "mixed_lower_child_budget_keeps_its_tokens")]
    #[test_case(ThinkingConfig::Budget(SMALL_BUDGET), ThinkingConfig::Effort(High), ThinkingConfig::Budget(SMALL_BUDGET) ; "mixed_parent_budget_caps_child_effort")]
    #[test_case(ThinkingConfig::Budget(LARGE_BUDGET), ThinkingConfig::Effort(Minimal), ThinkingConfig::Effort(Minimal) ; "mixed_lower_child_effort_keeps_its_level")]
    fn thinking_clamp_to_parent(
        parent: ThinkingConfig,
        child: ThinkingConfig,
        expected: ThinkingConfig,
    ) {
        assert_eq!(child.clamp_to(parent), expected);
    }

    #[test_case(None,                           ThinkingConfig::Off      ; "absent_means_off")]
    #[test_case(Some(StoredThinking::Adaptive), ThinkingConfig::Adaptive ; "a_stored_level_carries_over")]
    fn optional_stored_thinking_into_config(
        stored: Option<StoredThinking>,
        expected: ThinkingConfig,
    ) {
        assert_eq!(ThinkingConfig::from(stored), expected);
    }

    #[test_case("",         ThinkingConfig::Off,      Ok(ThinkingConfig::Adaptive)  ; "toggle_on")]
    #[test_case("",         ThinkingConfig::Adaptive, Ok(ThinkingConfig::Off)       ; "toggle_off")]
    #[test_case("off",      ThinkingConfig::Adaptive, Ok(ThinkingConfig::Off)       ; "explicit_off")]
    #[test_case("adaptive", ThinkingConfig::Off,      Ok(ThinkingConfig::Adaptive)  ; "explicit_adaptive")]
    #[test_case("high",     ThinkingConfig::Off,      Ok(ThinkingConfig::Effort(High)) ; "explicit_effort")]
    #[test_case("8192",     ThinkingConfig::Off,      Ok(ThinkingConfig::Budget(8192)) ; "explicit_budget")]
    #[test_case("512",      ThinkingConfig::Off,      Ok(ThinkingConfig::Budget(512)) ; "small_budget")]
    #[test_case("0",        ThinkingConfig::Off,      Err(())                       ; "budget_zero")]
    #[test_case("garbage",  ThinkingConfig::Off,      Err(())                       ; "invalid_input")]
    fn thinking_parse(input: &str, current: ThinkingConfig, expected: Result<ThinkingConfig, ()>) {
        let result = ThinkingConfig::parse(input, current).map_err(|_| ());
        assert_eq!(result, expected);
    }

    #[test_case(ThinkingConfig::Off      ; "off")]
    #[test_case(ThinkingConfig::Adaptive ; "adaptive")]
    #[test_case(ThinkingConfig::Effort(Max) ; "effort")]
    #[test_case(ThinkingConfig::Budget(8192) ; "budget")]
    fn thinking_display_round_trip(config: ThinkingConfig) {
        let s = config.to_string();
        let parsed = ThinkingConfig::parse(&s, ThinkingConfig::Off).unwrap();
        assert_eq!(parsed, config);
    }
}
