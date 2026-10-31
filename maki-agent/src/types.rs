use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use maki_providers::{AgentError, add_cost};
use maki_storage::sessions::StoredTokenUsage;
pub use maki_types::event::{
    ALLOWED_BY, AgentEvent, CallRecord, CostTally, Envelope, EventSender, EventStreamGuard,
    PartStatus, PermissionChoice, RunTotals, SessionEndReason, SessionEvents, SteerKind,
    SubagentInfo, ToolDoneEvent, ToolPart, ToolProgressEvent, ToolStartEvent, TurnCompleteEvent,
    event_stream, tool_results,
};
pub use maki_types::snapshot::{
    BufferSnapshot, DefaultColor, InlineStyle, SharedBuf, SnapshotLine, SnapshotSpan, SpanColor,
    SpanStyle, UiWaker,
};
pub use maki_types::stop_reason::DoneReason;
pub use maki_types::tool::{
    ChildStatus, DEFAULT_DATA_VERSION, GrepFileEntry, GrepLine, GrepMatchGroup, InstructionBlock,
    NO_FILES_FOUND, TextOutput, TodoItem, TodoPriority, TodoStatus, ToolChild, ToolData, ToolInput,
    ToolOutput,
};
use maki_types::usage::TokenUsage;

pub fn error_event(error: &AgentError, totals: RunTotals) -> AgentEvent {
    AgentEvent::Error {
        message: error.user_message(),
        auth: error.is_auth_error(),
        totals,
    }
}

/// Spend accumulator for one run, chained to the run that spawned it.
///
/// A round is added where it was paid for and walks up the chain, so every
/// ledger holds its own subtree: a subagent reports its own spend, and the
/// turn that spawned it still gets billed for the whole fan-out. Several
/// subagents run at once, hence the lock.
#[derive(Debug, Default)]
pub struct RunLedger {
    spend: Mutex<Spend>,
    parent: Option<Arc<RunLedger>>,
}

/// A ledger's totals and rows, taken together.
#[derive(Debug, Clone, Default)]
pub struct Spend {
    pub totals: RunTotals,
    /// The rows a session file keeps, built as the App builds its own.
    pub by_model: HashMap<String, StoredTokenUsage>,
}

impl RunLedger {
    pub fn child(parent: &Arc<Self>) -> Arc<Self> {
        Self::carrying(parent, RunTotals::default())
    }

    pub fn carrying(parent: &Arc<Self>, carried: RunTotals) -> Arc<Self> {
        Arc::new(Self {
            spend: Mutex::new(Spend {
                totals: carried,
                ..Spend::default()
            }),
            parent: Some(Arc::clone(parent)),
        })
    }

    /// The event is the one description of a billed response, so the file
    /// built from this and the App built from the event cannot disagree.
    /// `list_cost` is banked for every model, unlike the event's.
    pub fn bill(&self, turn: &TurnCompleteEvent, list_cost: Option<f64>) {
        {
            let mut spend = self.locked();
            spend.totals.usage += turn.usage;
            add_cost(&mut spend.totals.cost, turn.cost);
            add_cost(&mut spend.totals.list_cost, list_cost);
            *spend.by_model.entry(turn.model.clone()).or_default() += turn
                .usage
                .billed_with_subsidised_list_cost(turn.cost, turn.subsidised_list_cost);
        }
        if let Some(parent) = &self.parent {
            parent.bill(turn, list_cost);
        }
    }

    pub fn add(&self, usage: TokenUsage, cost: Option<f64>, list_cost: Option<f64>) {
        self.locked().totals += RunTotals {
            usage,
            cost,
            list_cost,
        };
        if let Some(parent) = &self.parent {
            parent.add(usage, cost, list_cost);
        }
    }

    pub fn totals(&self) -> RunTotals {
        self.locked().totals
    }

    /// One snapshot, so a child billing meanwhile cannot land in the totals
    /// without its row.
    pub fn spend(&self) -> Spend {
        self.locked().clone()
    }

    /// A poisoned lock only means another run panicked mid-update. The totals
    /// are still sound, and dropping a session's accounting over it is worse.
    fn locked(&self) -> MutexGuard<'_, Spend> {
        self.spend.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_providers::Message;
    use test_case::test_case;

    const FIRST_MODEL: &str = "first-model";
    const SECOND_MODEL: &str = "second-model";
    const FIRST_COST: f64 = 0.5;
    const SECOND_COST: f64 = 0.25;
    const FIRST_INPUT: u32 = 10;
    const SECOND_INPUT: u32 = 5;
    const OWN_SUBTREE_MSG: &str = "a child ledger reports only what it spent itself";
    const ROLLUP_MSG: &str = "a child's spend has to land in the parent's totals";

    fn turn(model: &str, input: u32, cost: Option<f64>) -> TurnCompleteEvent {
        TurnCompleteEvent {
            message: Message::default(),
            usage: usage(input),
            model: model.into(),
            cost,
            subsidised_list_cost: None,
            subsidised_by: None,
            context_size: None,
            context_window: 0,
        }
    }

    fn usage(input: u32) -> TokenUsage {
        TokenUsage {
            input,
            ..Default::default()
        }
    }

    #[test_case(Some(FIRST_COST), Some(SECOND_COST), Some(FIRST_COST + SECOND_COST) ; "priced_rounds_add_up")]
    #[test_case(None, None, None ; "unpriced_rounds_stay_unpriced")]
    fn ledger_folds_usage_and_cost_across_adds(
        first: Option<f64>,
        second: Option<f64>,
        expected: Option<f64>,
    ) {
        let ledger = RunLedger::default();
        ledger.bill(&turn(FIRST_MODEL, FIRST_INPUT, first), first);
        ledger.bill(&turn(FIRST_MODEL, SECOND_INPUT, second), second);

        let totals = ledger.totals();
        assert_eq!(totals.usage.input, FIRST_INPUT + SECOND_INPUT);
        assert_eq!(totals.cost, expected);
        assert_eq!(totals.list_cost, expected);
    }

    #[test]
    fn ledger_child_spend_rolls_up_into_parent() {
        let parent = Arc::new(RunLedger::default());
        parent.bill(
            &turn(FIRST_MODEL, FIRST_INPUT, Some(FIRST_COST)),
            Some(FIRST_COST),
        );
        let child = RunLedger::child(&parent);
        child.bill(
            &turn(SECOND_MODEL, SECOND_INPUT, Some(SECOND_COST)),
            Some(SECOND_COST),
        );

        let own = child.totals();
        assert_eq!(own.usage.input, SECOND_INPUT, "{OWN_SUBTREE_MSG}");
        assert_eq!(own.cost, Some(SECOND_COST), "{OWN_SUBTREE_MSG}");

        let rolled_up = parent.totals();
        assert_eq!(
            rolled_up.usage.input,
            FIRST_INPUT + SECOND_INPUT,
            "{ROLLUP_MSG}"
        );
        assert_eq!(
            rolled_up.cost,
            Some(FIRST_COST + SECOND_COST),
            "{ROLLUP_MSG}"
        );
        let rows = parent.spend().by_model;
        assert_eq!(rows[FIRST_MODEL].input, FIRST_INPUT, "{ROLLUP_MSG}");
        assert_eq!(rows[SECOND_MODEL].cost, Some(SECOND_COST), "{ROLLUP_MSG}");
        assert!(
            !child.spend().by_model.contains_key(FIRST_MODEL),
            "{OWN_SUBTREE_MSG}"
        );
    }
}
