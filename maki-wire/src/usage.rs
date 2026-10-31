//! The usage extension: a turn's usage and the session's list-price total,
//! which the standard `UsageUpdate` has no field for. It travels in
//! `_meta.maki.usage` on that update, so a client without it still reads
//! the standard gauge and cost.
//!
//! The usage ledger extension rides on it: the session's token total and
//! per-model rows in the same metadata, and `_maki/usage/quota` for the
//! agent's provider quota.

use std::collections::HashMap;

use agent_client_protocol_schema::v1::{Meta, SessionId, SessionUpdate, UsageUpdate};
use maki_storage::sessions::{StoredTokenUsage, add_cost};
use maki_types::event::TurnCompleteEvent;
use maki_types::usage::{ProviderUsage, TokenUsage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extensions::META_KEY;

pub const USAGE_KEY: &str = "usage";
pub const USAGE_VERSION: u64 = 2;
pub const LEDGER_KEY: &str = "usage_ledger";
pub const LEDGER_VERSION: u64 = 2;
pub const QUOTA_METHOD: &str = "_maki/usage/quota";
/// The only currency the local display draws.
const USD: &str = "USD";

/// What a client draws of a session's spending, from one `UsageUpdate`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionUsage {
    pub background_tasks: Option<usize>,
    pub used: u32,
    pub window: u32,
    /// The standard total. Another currency leaves it unset.
    pub cost: Option<f64>,
    pub list_cost: Option<f64>,
    pub subsidised_by: Option<String>,
    /// The session's token total and rows, under the ledger extension.
    pub ledger: Option<SessionLedger>,
}

impl SessionUsage {
    /// `extension` is the update's usage extension, when it was agreed and
    /// could be read.
    pub fn from_update(update: &UsageUpdate, extension: Option<&WireUsage>) -> Self {
        Self {
            background_tasks: extension.and_then(|usage| usage.background_tasks),
            used: saturate(update.used),
            window: saturate(update.size),
            cost: update
                .cost
                .as_ref()
                .filter(|cost| cost.currency == USD)
                .map(|cost| cost.amount),
            list_cost: extension.and_then(|usage| usage.list_cost),
            subsidised_by: extension.and_then(|usage| usage.subsidised_by.clone()),
            ledger: extension.and_then(|usage| usage.ledger.clone()),
        }
    }
}

/// What a session has spent, built from each `TurnCompleteEvent` by the
/// rule `App::bill` follows, so a remote `/usage` reads what a local one
/// would.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLedger {
    pub total: TokenUsage,
    pub cost: Option<f64>,
    pub list_cost: Option<f64>,
    pub by_model: HashMap<String, StoredTokenUsage>,
}

impl SessionLedger {
    pub fn bill(&mut self, turn: &TurnCompleteEvent) {
        self.total += turn.usage;
        add_cost(&mut self.cost, turn.cost);
        add_cost(&mut self.list_cost, turn.subsidised_list_cost);
        *self.by_model.entry(turn.model.clone()).or_default() += turn
            .usage
            .billed_with_subsidised_list_cost(turn.cost, turn.subsidised_list_cost);
    }
}

fn saturate(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// The usage extension shares the token and billing records a session stores.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_tasks: Option<usize>,
    /// The session's total at list price, like the standard `cost`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_cost: Option<f64>,
    /// The display name of the subscription that pays for the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subsidised_by: Option<String>,
    /// A main-agent turn only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<WireTurn>,
    /// The main agent sent the last turn's tool results back, which is when
    /// a local run puts the turn's usage line on its last tool row.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub results_submitted: bool,
    /// Under the ledger extension: the whole ledger at this update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger: Option<SessionLedger>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaRequest {
    pub session_id: SessionId,
}

/// The quota of the provider it names, which may not be the one a later
/// model switch put behind the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaAnswer {
    pub provider: String,
    #[serde(flatten)]
    pub state: QuotaState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QuotaState {
    Ready(ProviderUsage),
    /// `{}` on the wire, so every state is an object field.
    Unsupported {},
    Error(String),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireTurn {
    pub model: String,
    #[serde(flatten)]
    pub usage: StoredTokenUsage,
}

impl From<&TurnCompleteEvent> for WireTurn {
    fn from(turn: &TurnCompleteEvent) -> Self {
        Self {
            model: turn.model.clone(),
            usage: turn
                .usage
                .billed_with_subsidised_list_cost(turn.cost, turn.subsidised_list_cost),
        }
    }
}

/// Anything but a `UsageUpdate` passes through unchanged.
pub fn attach(update: SessionUpdate, usage: &WireUsage) -> SessionUpdate {
    let SessionUpdate::UsageUpdate(update) = update else {
        return update;
    };
    let mut maki = Meta::new();
    maki.insert(
        USAGE_KEY.into(),
        serde_json::to_value(usage).unwrap_or_default(),
    );
    let meta = Meta::from_iter([(META_KEY.to_owned(), Value::Object(maki))]);
    SessionUpdate::UsageUpdate(update.meta(meta))
}

/// `None` when the update carries no extension.
pub fn read(meta: Option<&Meta>) -> Option<Result<WireUsage, String>> {
    let value = meta?.get(META_KEY)?.get(USAGE_KEY)?;
    Some(WireUsage::deserialize(value).map_err(|e| format!("unreadable usage: {e}")))
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::Cost;
    use maki_types::message::Message;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const MODEL: &str = "claude-sonnet-4";
    const COST: f64 = 0.5;
    const LIST_COST: f64 = 1.5;
    const USED: u64 = 10;
    const SIZE: u64 = 100;

    fn turn() -> TurnCompleteEvent {
        TurnCompleteEvent {
            message: Message::default(),
            usage: TokenUsage {
                input: 1,
                output: 2,
                cache_read: 3,
                cache_creation: 4,
                cost: None,
            },
            model: MODEL.into(),
            cost: Some(COST),
            subsidised_list_cost: Some(LIST_COST),
            subsidised_by: None,
            context_size: None,
            context_window: 0,
        }
    }

    #[test]
    fn billed_usage_reaches_the_display_without_changing_session_records() {
        let turn = turn();
        let mut ledger = SessionLedger::default();
        ledger.bill(&turn);
        ledger.bill(&turn);
        let wire = WireUsage {
            turn: Some(WireTurn::from(&turn)),
            ledger: Some(ledger.clone()),
            list_cost: ledger.list_cost,
            subsidised_by: Some("Max".into()),
            results_submitted: true,
            background_tasks: Some(1),
        };
        let SessionUpdate::UsageUpdate(update) = attach(
            SessionUpdate::UsageUpdate(UsageUpdate::new(USED, SIZE)),
            &wire,
        ) else {
            unreachable!()
        };
        let decoded = read(update.meta.as_ref()).unwrap().unwrap();
        assert_eq!(decoded, wire);
        let session = SessionUsage::from_update(&update, Some(&decoded));
        assert_eq!(session.ledger, Some(ledger));
        let ledger = session.ledger.unwrap();
        assert_eq!(ledger.total.input, 2);
        assert_eq!(ledger.total.output, 4);
        assert_eq!(ledger.total.cache_read, 6);
        assert_eq!(ledger.total.cache_creation, 8);
        assert_eq!(ledger.cost, Some(COST * 2.0));
        assert_eq!(ledger.list_cost, Some(LIST_COST * 2.0));
        assert_eq!(
            ledger.by_model[MODEL],
            ledger
                .total
                .billed_with_subsidised_list_cost(ledger.cost, ledger.list_cost)
        );
        let json = serde_json::to_value(&decoded).unwrap();
        assert_eq!(
            json["turn"],
            json!({
                "model": MODEL, "input": 1, "output": 2, "cache_read": 3, "cache_creation": 4,
                "cost": COST, "subsidised_list_cost": LIST_COST,
            })
        );
        assert_eq!(
            json["ledger"]["byModel"][MODEL],
            json!({
                "input": 2, "output": 4, "cache_read": 6, "cache_creation": 8,
                "cost": COST * 2.0, "subsidised_list_cost": LIST_COST * 2.0,
            })
        );
        assert_eq!(
            json["ledger"]["total"],
            json!({
                "input_tokens": 2, "output_tokens": 4, "cache_read_input_tokens": 6,
                "cache_creation_input_tokens": 8,
            })
        );
        let quota = QuotaAnswer {
            provider: MODEL.into(),
            state: QuotaState::Unsupported {},
        };
        assert_eq!(
            serde_json::to_value(quota).unwrap(),
            json!({"provider": MODEL, "unsupported": {}})
        );
    }

    #[test_case("USD", Some(COST) ; "usd")]
    #[test_case("EUR", None ; "another_currency")]
    fn standard_usage_remains_readable_without_the_extension(
        currency: &str,
        expected: Option<f64>,
    ) {
        let update = UsageUpdate::new(USED, SIZE).cost(Cost::new(COST, currency));
        assert!(read(update.meta.as_ref()).is_none());
        let session = SessionUsage::from_update(&update, None);
        assert_eq!((session.used, session.window), (USED as u32, SIZE as u32));
        assert_eq!(session.cost, expected);
        assert!(session.ledger.is_none());
    }
}
