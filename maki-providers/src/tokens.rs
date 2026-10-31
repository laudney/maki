//! Prompt size accounting.
//!
//! An estimate maki computes from message bytes, and the count the provider
//! returns with a response. [`ContextGauge`] carries one session's running
//! total, so a measurement is never re-derived from an estimate.

pub use maki_types::tokens::{estimate_message_tokens, estimate_prompt_tokens};
use serde_json::Value;

use crate::types::Message;

/// The running prompt size of one session: the provider's own count for the
/// last prompt it was sent, plus a byte estimate of everything appended since.
///
/// The two are held apart because only one of them can be wrong, and every
/// response shrinks the guessed part back to nothing.
///
/// Lives as long as the transcript, which is longer than one agent run. A gauge
/// rebuilt per user turn throws away every measurement the session ever made
/// and is back to guessing.
#[derive(Debug, Clone, Default)]
pub struct ContextGauge {
    measured: u32,
    /// Byte estimate of the messages appended since that count.
    appended: u32,
}

impl ContextGauge {
    /// A session read back from disk was measured before it was stored, so it
    /// starts from that count rather than from an estimate of its transcript.
    pub fn restored(measured: u32) -> Self {
        Self {
            measured,
            appended: 0,
        }
    }

    pub fn size(&self) -> u32 {
        self.measured.saturating_add(self.appended)
    }

    /// Ground truth for a prompt that was just sent, from a response or from a
    /// rejection that quoted its own count. It covers every message that
    /// request carried, so it replaces the total instead of adding to it.
    ///
    /// A zero measurement means the provider reported no usage (Z.AI streams
    /// without it), not an empty prompt, so it overwrites nothing.
    pub fn record(&mut self, measured: u32) {
        if measured == 0 {
            return;
        }
        self.measured = measured;
        self.appended = 0;
    }

    /// Messages appended since the last measurement, estimated until the next
    /// response measures them along with the rest.
    pub fn append(&mut self, messages: &[Message]) {
        self.appended = self
            .appended
            .saturating_add(estimate_message_tokens(messages));
    }

    /// Starts over from a transcript nobody has sent yet, as compaction leaves
    /// behind. Whatever was measured describes messages that no longer exist.
    ///
    /// Takes the whole prompt and not just the transcript, because the system
    /// prompt and the tool schemas stay in every request compaction did not
    /// touch. Sized from the messages alone, a freshly compacted session reads
    /// as having a five figure baseline of room it does not have, and spends it
    /// on a provider overflow instead of the next scheduled compaction.
    pub fn reset(&mut self, messages: &[Message], system: &str, tools: &Value) {
        *self = Self {
            measured: 0,
            appended: estimate_prompt_tokens(messages, system, tools),
        };
    }

    /// A resumed session can already fill the window, and the gauge only learns
    /// the real size from a response, so without this its first request goes
    /// out unguarded.
    pub fn seed_if_empty(&mut self, messages: &[Message], system: &str, tools: &Value) {
        if self.size() == 0 {
            self.reset(messages, system, tools);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use maki_types::tokens::CHARS_PER_TOKEN;

    use super::*;
    use crate::types::{ContentBlock, Role};

    const MEASURED: u32 = 10_000;
    const TEXT_BYTES: usize = 4_000;
    const TEXT_TOKENS: u32 = (TEXT_BYTES / CHARS_PER_TOKEN) as u32;

    fn text_message(len: usize) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "x".repeat(len),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_measurement_replaces_the_estimate_and_later_appends_stack_on_it() {
        let mut gauge = ContextGauge::default();
        gauge.append(&[text_message(TEXT_BYTES)]);
        assert_eq!(gauge.size(), TEXT_TOKENS);

        gauge.record(MEASURED);
        assert_eq!(
            gauge.size(),
            MEASURED,
            "the estimate the measurement covers is not counted twice"
        );

        gauge.record(0);
        assert_eq!(
            gauge.size(),
            MEASURED,
            "a provider that reports no usage overwrites nothing"
        );

        gauge.append(&[text_message(TEXT_BYTES)]);
        assert_eq!(gauge.size(), MEASURED + TEXT_TOKENS);
    }

    /// The transcript compaction summarized away is gone, so the measurement
    /// describing it cannot keep sizing the session.
    #[test]
    fn a_reset_drops_the_measurement_with_the_transcript() {
        let mut gauge = ContextGauge::restored(MEASURED);
        gauge.reset(&[text_message(TEXT_BYTES)], "", &json!([]));
        assert_eq!(gauge.size(), TEXT_TOKENS);
    }

    /// Compaction shrinks the transcript and nothing else, so a gauge that
    /// forgets the baseline here under-reports by the whole tool catalog and
    /// lets the session sail past the threshold that would have compacted it.
    #[test]
    fn a_reset_keeps_the_system_prompt_and_tool_schemas_in_the_size() {
        let messages = [text_message(TEXT_BYTES)];
        let tools = json!([{"name": "read", "input_schema": {"path": "string"}}]);

        let mut bare = ContextGauge::default();
        bare.reset(&messages, "", &json!([]));

        let mut with_baseline = ContextGauge::default();
        with_baseline.reset(&messages, &"s".repeat(TEXT_BYTES), &tools);

        assert_eq!(bare.size(), TEXT_TOKENS);
        assert_eq!(
            with_baseline.size(),
            estimate_prompt_tokens(&messages, &"s".repeat(TEXT_BYTES), &tools)
        );
        assert!(
            with_baseline.size() > bare.size() + TEXT_TOKENS,
            "the reset size is blind to the system prompt and the tool schemas"
        );
    }

    /// Two entry points onto one size: if they ever disagree, whichever one a
    /// call site happens to use decides when the session overflows.
    #[test]
    fn reset_and_seeding_size_the_same_prompt_alike() {
        let messages = [text_message(TEXT_BYTES)];
        let system = "s".repeat(TEXT_BYTES);
        let tools = json!([{"name": "read"}]);

        let mut reset = ContextGauge::restored(MEASURED);
        reset.reset(&messages, &system, &tools);
        let mut seeded = ContextGauge::default();
        seeded.seed_if_empty(&messages, &system, &tools);

        assert_eq!(reset.size(), seeded.size());
    }

    #[test]
    fn seeding_only_fills_an_empty_gauge() {
        let messages = [text_message(TEXT_BYTES)];
        let mut gauge = ContextGauge::default();
        gauge.seed_if_empty(&messages, "", &json!([]));
        assert_eq!(gauge.size(), TEXT_TOKENS);

        gauge.seed_if_empty(&[text_message(TEXT_BYTES * 10)], "", &json!([]));
        assert_eq!(gauge.size(), TEXT_TOKENS, "a seeded gauge is not reseeded");
    }
}
