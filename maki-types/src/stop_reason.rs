//! Why a model stopped writing, as a provider reports it, and why a whole
//! run ended.

use serde::{Deserialize, Serialize};
use strum::{Display, IntoStaticStr};
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

impl StopReason {
    pub fn from_anthropic(s: &str) -> Self {
        match s {
            "end_turn" => Self::EndTurn,
            "tool_use" => Self::ToolUse,
            "max_tokens" => Self::MaxTokens,
            _ => Self::EndTurn,
        }
    }

    pub fn from_openai(s: &str) -> Self {
        match s {
            "stop" => Self::EndTurn,
            "tool_calls" => Self::ToolUse,
            "length" => Self::MaxTokens,
            _ => Self::EndTurn,
        }
    }

    pub fn from_google(s: &str) -> Self {
        match s {
            "STOP" => Self::EndTurn,
            "MAX_TOKENS" => Self::MaxTokens,
            "SAFETY" | "RECITATION" => {
                warn!("Gemini stop reason: {s}, treating as end_turn");
                Self::EndTurn
            }
            _ => Self::EndTurn,
        }
    }
}

/// Why a run ended. The provider's `StopReason` describes one turn, this
/// describes the whole run, including the endings only the agent knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum DoneReason {
    EndTurn,
    MaxTokens,
    MaxTurns,
    Cancelled,
    /// A manual `/compact` ended the run, but no user turn ended with it, so
    /// a goal loop should not treat this as a turn boundary.
    Compact,
    /// An `agent.user_message` layer dropped the message before the model saw
    /// it.
    Dropped,
}

impl From<Option<StopReason>> for DoneReason {
    /// We only ask at the end of a turn that called no tool, so a tool-use stop
    /// and a provider that says nothing both mean the same thing: it is over.
    fn from(reason: Option<StopReason>) -> Self {
        match reason {
            Some(StopReason::MaxTokens) => Self::MaxTokens,
            Some(StopReason::EndTurn | StopReason::ToolUse) | None => Self::EndTurn,
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("end_turn", StopReason::EndTurn   ; "end_turn")]
    #[test_case("tool_use", StopReason::ToolUse   ; "tool_use")]
    #[test_case("max_tokens", StopReason::MaxTokens ; "max_tokens")]
    #[test_case("unknown", StopReason::EndTurn    ; "unknown_defaults_to_end_turn")]
    fn stop_reason_from_anthropic(input: &str, expected: StopReason) {
        assert_eq!(StopReason::from_anthropic(input), expected);
    }

    #[test_case("stop", StopReason::EndTurn       ; "stop_maps_to_end_turn")]
    #[test_case("tool_calls", StopReason::ToolUse ; "tool_calls_maps_to_tool_use")]
    #[test_case("length", StopReason::MaxTokens   ; "length_maps_to_max_tokens")]
    #[test_case("unknown", StopReason::EndTurn    ; "unknown_defaults_to_end_turn")]
    fn stop_reason_from_openai(input: &str, expected: StopReason) {
        assert_eq!(StopReason::from_openai(input), expected);
    }

    #[test_case(None ; "no_stop_reason")]
    #[test_case(Some(StopReason::ToolUse) ; "tool_use")]
    fn stop_reason_without_its_own_ending_becomes_end_turn(stop: Option<StopReason>) {
        assert_eq!(DoneReason::from(stop), DoneReason::EndTurn);
    }
}
