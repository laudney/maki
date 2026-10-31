//! Token counts, the rates that price them, and the quotas a provider
//! reports, as a session stores them and a display path draws them.

use std::ops::AddAssign;

use jiff::Timestamp;
use maki_storage::sessions::StoredTokenUsage;
use serde::{Deserialize, Deserializer, Serialize};

const PER_MILLION: f64 = 1_000_000.0;

/// Also the shape of a rate in `models/<slug>.toml`. None of the four rates may
/// ever gain a `#[serde(default)]`: a curated row that forgets one has to fail
/// loudly instead of quietly billing the user zero.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    /// Anthropic fast mode charges a premium that differs per model. `None`
    /// means the model has no fast tier, so asking for fast mode quietly falls
    /// back to standard rates instead of overcharging.
    #[serde(default)]
    pub fast: Option<FastPricing>,
}

/// Cache rates are missing on purpose: Anthropic derives them from `input` with
/// the same multipliers it uses for standard pricing, so storing them would just
/// invite the two copies to drift apart.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct FastPricing {
    pub input: f64,
    pub output: f64,
}

impl ModelPricing {
    /// Rates in USD per 1M tokens, with no fast tier and no subsidy -- the shape of
    /// every static catalog entry. `const` so provider catalogs can call it;
    /// it also spares each entry from spelling out fields it never sets.
    pub const fn per_million(input: f64, output: f64, cache_write: f64, cache_read: f64) -> Self {
        Self {
            input,
            output,
            cache_write,
            cache_read,
            fast: None,
        }
    }

    pub const ZERO: Self = Self {
        input: 0.0,
        output: 0.0,
        cache_write: 0.0,
        cache_read: 0.0,
        fast: None,
    };

    pub fn is_zero(&self) -> bool {
        self.input == 0.0 && self.output == 0.0 && self.cache_write == 0.0 && self.cache_read == 0.0
    }

    /// Cache multipliers Anthropic applies on top of the base input rate.
    const CACHE_WRITE_MULTIPLIER: f64 = 1.25;
    const CACHE_READ_MULTIPLIER: f64 = 0.10;

    /// Fast mode only ever quotes two rates, so its cache rates come off its own
    /// input rate rather than the standard one they no longer relate to.
    fn rates(&self, fast: bool) -> (f64, f64, f64, f64) {
        match &self.fast {
            Some(f) if fast => (
                f.input,
                f.output,
                f.input * Self::CACHE_WRITE_MULTIPLIER,
                f.input * Self::CACHE_READ_MULTIPLIER,
            ),
            _ => (self.input, self.output, self.cache_write, self.cache_read),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Non-cached input tokens. Total input = `input + cache_read + cache_creation`.
    #[serde(rename = "input_tokens")]
    pub input: u32,
    #[serde(rename = "output_tokens")]
    pub output: u32,
    #[serde(rename = "cache_creation_input_tokens")]
    pub cache_creation: u32,
    #[serde(rename = "cache_read_input_tokens")]
    pub cache_read: u32,
    /// What this one response cost, straight from the provider, when it bothers
    /// to say (OpenRouter's `usage.cost`). Worth preferring on a router, where
    /// our table prices the model we asked for and the router bills for
    /// whichever upstream it picked.
    ///
    /// One response only, so it is neither summed nor stored. Sessions add up
    /// their bill a turn at a time in [`StoredTokenUsage::cost`].
    #[serde(skip)]
    pub cost: Option<f64>,
}

impl From<StoredTokenUsage> for TokenUsage {
    fn from(s: StoredTokenUsage) -> Self {
        Self {
            input: s.input,
            output: s.output,
            cache_creation: s.cache_creation,
            cache_read: s.cache_read,
            // A stored cost belongs to a whole session and this field to one
            // response, so there is nothing honest to carry across.
            cost: None,
        }
    }
}

impl TokenUsage {
    /// Ready to store, with what the turn was billed. No `From<TokenUsage>` on
    /// purpose: a caller that forgets the cost quietly loses money from the
    /// session total, so saying it out loud is mandatory.
    pub fn billed(&self, cost: Option<f64>) -> StoredTokenUsage {
        self.billed_with_subsidised_list_cost(cost, None)
    }

    /// Like [`billed`](Self::billed), but also records what a subsidised
    /// turn would have cost at the provider's list rates. `subsidised_list_cost`
    /// is `None` for every non-subsidised turn.
    pub fn billed_with_subsidised_list_cost(
        &self,
        cost: Option<f64>,
        subsidised_list_cost: Option<f64>,
    ) -> StoredTokenUsage {
        StoredTokenUsage {
            input: self.input,
            output: self.output,
            cache_creation: self.cache_creation,
            cache_read: self.cache_read,
            cost,
            subsidised_list_cost,
        }
    }

    pub fn total_input(&self) -> u32 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_creation)
    }

    pub fn context_tokens(&self) -> u32 {
        self.total_input().saturating_add(self.output)
    }

    pub fn format(&self, cost: Option<f64>) -> String {
        self.format_cost(cost, "")
    }

    /// Like [`format`](Self::format), but marks the cost as a running total.
    pub fn format_sum_cost(&self, cost: Option<f64>) -> String {
        self.format_cost(cost, "Σ")
    }

    fn format_cost(&self, cost: Option<f64>, prefix: &str) -> String {
        let tokens = format!(
            "{}↑ {}↓",
            format_tokens(self.total_input()),
            format_tokens(self.output)
        );
        match cost {
            Some(cost) => format!("{tokens} {prefix}${cost:.3}"),
            None => tokens,
        }
    }

    /// Public only for `Model` pricing in `maki-providers`, and hidden for
    /// the same reason it is not meant for anything else: pricing outside
    /// `Model` skips the provider's schedule, and the bill it may have sent
    /// us.
    #[doc(hidden)]
    pub fn estimate(&self, pricing: &ModelPricing, fast: bool) -> f64 {
        let (input, output, cache_write, cache_read) = pricing.rates(fast);
        self.input as f64 * input / PER_MILLION
            + self.output as f64 * output / PER_MILLION
            + self.cache_creation as f64 * cache_write / PER_MILLION
            + self.cache_read as f64 * cache_read / PER_MILLION
    }
}

pub fn format_tokens(tokens: impl Into<u64>) -> String {
    let tokens = tokens.into();
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => format!("{:.1}k", tokens as f64 / 1_000.0),
        _ => format!("{:.1}m", tokens as f64 / 1_000_000.0),
    }
}

impl AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input = self.input.saturating_add(rhs.input);
        self.output = self.output.saturating_add(rhs.output);
        self.cache_creation = self.cache_creation.saturating_add(rhs.cache_creation);
        self.cache_read = self.cache_read.saturating_add(rhs.cache_read);
        // Only some responses arrive with a bill, so a running total of them is
        // part paid and part missing while looking like the lot.
        self.cost = None;
    }
}

/// Provider-reported usage quota, independent of local token accounting. Not every
/// provider exposes a programmatic quota endpoint; check `Provider::fetch_usage`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUsage {
    /// Subscription/plan level when the provider reports one (e.g. "lite").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub limits: Vec<UsageLimit>,
    /// Per-model breakdown of the current UTC day, when the provider exposes
    /// one. The window is part of the contract: the modal labels these rows as
    /// today's, so a provider reporting a month or a lifetime needs its own
    /// field rather than this one. Sorted by the provider (typically spend
    /// desc).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_model_today: Vec<ModelUsageRow>,
}

/// One row of a provider-reported per-model usage table. Money is kept as
/// integer micro-dollars to keep `ProviderUsage` in `Eq` territory for tests;
/// callers format at the UI layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsageRow {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// Spend in micro-dollars (1_000_000 = $1.00).
    pub spend_microdollars: u64,
}

/// A single quota window (e.g. a 5-hour or weekly token quota).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLimit {
    /// Human-readable label for the window, provided by the provider.
    pub label: String,
    /// Usage percentage within the window, 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percentage: Option<u32>,
    /// When the window resets, as epoch milliseconds. Also read from an RFC
    /// 3339 timestamp, see `deserialize_reset_at`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_reset_at"
    )]
    pub reset_at: Option<u64>,
    /// Extra provider-supplied context, e.g. "$2.33 spent" for usage credits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Epoch milliseconds of an RFC 3339 timestamp, clamped at the epoch. `None`
/// for a string that is no timestamp.
pub(crate) fn rfc3339_millis(at: &str) -> Option<u64> {
    at.parse::<Timestamp>()
        .ok()
        .map(|at| at.as_millisecond().max(0) as u64)
}

/// A usage hook may hand back the timestamp it read off the wire as is, so a
/// plugin needs no date parser that could drift from this one. A string that
/// is no timestamp reads as no reset, the way maki's own providers read one,
/// rather than failing the whole report over one field.
fn deserialize_reset_at<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ResetAt {
        Millis(u64),
        Timestamp(String),
    }
    Ok(match Option::<ResetAt>::deserialize(deserializer)? {
        Some(ResetAt::Millis(millis)) => Some(millis),
        Some(ResetAt::Timestamp(at)) => rfc3339_millis(&at),
        None => None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;

    /// Four counters that cannot be confused with each other.
    const COUNTERS: TokenUsage = TokenUsage {
        input: 11,
        output: 22,
        cache_creation: 33,
        cache_read: 44,
        cost: None,
    };
    const RECORDED_COST: f64 = 0.25;
    const USAGE_LABEL: &str = "Spend";
    const RESET_MILLIS: u64 = 1_790_812_800_250;

    #[test_case(999, "999"         ; "under_thousand")]
    #[test_case(1_000, "1.0k"      ; "thousand")]
    #[test_case(999_999, "1000.0k" ; "just_under_million")]
    #[test_case(1_000_000, "1.0m"  ; "million")]
    fn format_tokens_display(tokens: u32, expected: &str) {
        assert_eq!(format_tokens(tokens), expected);
    }

    #[test_case(TokenUsage { input: 12_000, output: 456, cache_creation: 200, cache_read: 100, cost: None }, None, "12.3k↑ 456↓" ; "without_cost")]
    #[test_case(TokenUsage { input: 1_000_000, output: 100_000, cache_creation: 200_000, cache_read: 500_000, cost: None }, Some(5.4), "1.7m↑ 100.0k↓ $5.400" ; "with_cost")]
    #[test_case(TokenUsage { input: u32::MAX, output: 1, cache_creation: 1, cache_read: 1, cost: None }, None, "4295.0m↑ 1↓" ; "input_saturates")]
    fn usage_formatting(usage: TokenUsage, cost: Option<f64>, expected: &str) {
        assert_eq!(usage.format(cost), expected);
    }

    #[test]
    fn sum_marker_applies_only_to_the_cost() {
        let usage = TokenUsage {
            input: 12_000,
            output: 456,
            cache_creation: 200,
            cache_read: 100,
            ..Default::default()
        };
        assert_eq!(usage.format_sum_cost(Some(1.5)), "12.3k↑ 456↓ Σ$1.500");
        assert_eq!(usage.format_sum_cost(None), usage.format(None));
    }

    #[test]
    fn total_input_includes_cached_tokens() {
        let usage = TokenUsage {
            input: 5_000,
            output: 1_000,
            cache_creation: 10_000,
            cache_read: 150_000,
            ..Default::default()
        };
        assert_eq!(usage.total_input(), 165_000);
    }

    #[test]
    fn estimate_computes_all_token_types() {
        let pricing = ModelPricing::per_million(3.00, 15.00, 3.75, 0.30);
        let usage = TokenUsage {
            input: 1_000_000,
            output: 100_000,
            cache_creation: 200_000,
            cache_read: 500_000,
            ..Default::default()
        };
        let cost = usage.estimate(&pricing, false);
        let expected = 3.0 + 1.5 + 0.75 + 0.15;
        assert!((cost - expected).abs() < 1e-10);
    }

    /// Only some responses arrive with a bill, so a total of them would be part
    /// paid and part missing while looking like the whole session.
    #[test]
    fn adding_usage_sums_counters_and_drops_the_bill() {
        let mut total = TokenUsage {
            cost: Some(RECORDED_COST),
            ..COUNTERS
        };
        total += TokenUsage {
            cost: Some(RECORDED_COST),
            ..COUNTERS
        };
        assert_eq!(total.input, COUNTERS.input * 2);
        assert_eq!(total.cost, None);
    }

    #[test]
    fn fast_mode_applies_premium_rates() {
        let pricing = ModelPricing {
            fast: Some(FastPricing {
                input: 30.00,
                output: 150.00,
            }),
            ..ModelPricing::per_million(5.00, 25.00, 6.25, 0.50)
        };
        let usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_creation: 1_000_000,
            cache_read: 1_000_000,
            ..Default::default()
        };
        let fast = usage.estimate(&pricing, true);
        let expected = 30.0 + 150.0 + 37.5 + 3.0;
        assert!((fast - expected).abs() < 1e-10);
        assert!(fast > usage.estimate(&pricing, false));
    }

    #[test]
    fn fast_flag_ignored_without_fast_tier() {
        let pricing = ModelPricing::per_million(3.00, 15.00, 3.75, 0.30);
        let usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_creation: 0,
            cache_read: 0,
            ..Default::default()
        };
        assert_eq!(
            usage.estimate(&pricing, true),
            usage.estimate(&pricing, false)
        );
    }

    /// Every later total is rebuilt from what was stored, so storing a turn must
    /// not shuffle the counters, invent one, or drop the cost.
    #[test]
    fn billed_stores_every_counter_and_the_cost() {
        assert_eq!(
            COUNTERS.billed(Some(RECORDED_COST)),
            StoredTokenUsage {
                input: COUNTERS.input,
                output: COUNTERS.output,
                cache_creation: COUNTERS.cache_creation,
                cache_read: COUNTERS.cache_read,
                cost: Some(RECORDED_COST),
                subsidised_list_cost: None,
            }
        );
        assert_eq!(COUNTERS.billed(None).cost, None);
    }

    #[test_case(json!(RESET_MILLIS), Some(RESET_MILLIS) ; "epoch_millis")]
    #[test_case(json!("2026-10-01T02:00:00.25+02:00"), Some(RESET_MILLIS) ; "rfc3339_with_offset_and_fraction")]
    #[test_case(json!("1969-12-31T23:59:59Z"), Some(0) ; "before_the_epoch_clamps")]
    #[test_case(json!("next month"), None ; "not_a_timestamp")]
    #[test_case(Value::Null, None ; "null")]
    fn usage_limit_reset_at_reads_millis_or_rfc3339(reset_at: Value, expected: Option<u64>) {
        let limit: UsageLimit =
            serde_json::from_value(json!({ "label": USAGE_LABEL, "reset_at": reset_at })).unwrap();
        assert_eq!(limit.reset_at, expected);
    }

    #[test]
    fn usage_limit_without_reset_at_has_none() {
        let limit: UsageLimit = serde_json::from_value(json!({ "label": USAGE_LABEL })).unwrap();
        assert_eq!(limit.reset_at, None);
    }
}
