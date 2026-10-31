use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;

use maki_storage::sessions::{SessionClaim, StoredMode};
use maki_types::thinking::ThinkingConfig;
use maki_types::usage::TokenUsage;

use crate::{AppSession, OpenSession};

use super::mode::{Mode, PlanState};

pub(crate) struct Yolo {
    seed: bool,
    stored: Cell<Option<bool>>,
}

impl Yolo {
    pub(crate) fn new(seed: bool) -> Self {
        Self {
            seed,
            stored: Cell::new(None),
        }
    }

    pub(crate) fn is_yolo(&self) -> bool {
        self.stored.get().unwrap_or(self.seed)
    }

    pub(crate) fn persisted_yolo(&self) -> Option<bool> {
        self.stored.get()
    }

    pub(crate) fn set_session_yolo(&self, value: Option<bool>) {
        self.stored.set(value);
    }

    pub(crate) fn toggle_yolo(&self) -> bool {
        let on = !self.is_yolo();
        self.stored.set(Some(on));
        on
    }
}

pub(crate) struct SessionState {
    pub session: Arc<AppSession>,
    /// Kept with a native tab across reload, so another process cannot claim it.
    pub claim: SessionClaim,
    pub context_window: u32,
    pub subsidised_by: Option<Arc<str>>,
    pub token_usage: TokenUsage,
    /// What the session has billed so far: the restored total plus every turn
    /// since. Kept running, because re-deriving it from the counters would
    /// re-price history at today's rates. `None` while nothing was priced.
    pub cost: Option<f64>,
    /// Sum of what subsidised turns in this session would have cost at the
    /// provider's published list price: the total restored from the session
    /// file plus every subsidised turn since. `None` until a subsidised turn
    /// lands; unaffected by ordinary, per-token-billed turns.
    pub subsidised_list_cost: Option<f64>,
    pub context_size: u32,
    pub mode: Mode,
    pub plan: PlanState,
    pub thinking: ThinkingConfig,
    pub fast: bool,
    pub workflow: bool,
}

impl SessionState {
    pub fn from_session(open: OpenSession) -> Self {
        let OpenSession { session, claim } = open;

        let mode = match session.meta.mode {
            Some(StoredMode::Plan) => Mode::Plan,
            _ => Mode::Build,
        };

        let plan = match &session.meta.plan_path {
            Some(p) if session.meta.plan_written => PlanState::Ready(PathBuf::from(p)),
            Some(p) => PlanState::Drafting(PathBuf::from(p)),
            None => PlanState::None,
        };

        let thinking = session.meta.thinking.into();
        let fast = session.meta.fast;
        let token_usage = session.token_usage;
        let cost = session
            .usage_by_model()
            .values()
            .filter_map(|row| row.cost)
            .reduce(|a, b| a + b);
        // Unlike `cost` there is nothing to settle: the list price was
        // recorded per turn and never moves, so resuming just adds the rows
        // back up. Without it a resumed subsidised session reads `$0.000`
        // with no reference figure until the next turn lands.
        let subsidised_list_cost = session
            .usage_by_model()
            .values()
            .filter_map(|usage| usage.subsidised_list_cost)
            .reduce(|total, cost| total + cost);
        let context_size = session.meta.context_size;

        Self {
            thinking,
            fast,
            workflow: session.meta.workflow,
            session: Arc::new(session),
            claim,
            context_window: 0,
            subsidised_by: None,
            token_usage,
            cost,
            subsidised_list_cost,
            context_size,
            mode,
            plan,
        }
    }

    pub fn session_mut(&mut self) -> &mut AppSession {
        Arc::make_mut(&mut self.session)
    }
}

impl From<Mode> for StoredMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Build => StoredMode::Build,
            Mode::Plan => StoredMode::Plan,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_storage::StateDir;

    const RECORDED_COST: f64 = 0.42;
    /// What a subsidised turn would have billed; the turn itself billed `$0`.
    const RECORDED_LIST_COST: f64 = 1.75;
    /// A round million, so a per-million rate reads straight off the bill.
    const MILLION_INPUT: TokenUsage = TokenUsage {
        input: 1_000_000,
        output: 0,
        cache_creation: 0,
        cache_read: 0,
        cost: None,
    };
    const UNRESOLVABLE_MODEL: &str = "a-model-no-table-has-ever-heard-of";

    fn resumed(session: AppSession) -> SessionState {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        SessionState::from_session(OpenSession::claimed(session, &storage))
    }

    /// An old session: counters, no per-model breakdown.
    fn session_with_counters() -> AppSession {
        let mut session = AppSession::new("test-model", "/tmp");
        session.token_usage = MILLION_INPUT;
        session
    }

    fn make_plan_session(mode: Option<StoredMode>, plan_path: Option<String>) -> AppSession {
        let mut session = AppSession::new("test-model", "/tmp");
        session.meta.mode = mode;
        session.meta.plan_path = plan_path;
        session
    }

    /// A resumed session opens on the bill it ran up, not on its counters
    /// re-priced with whatever model is selected now. The model that recorded
    /// this one prices to nothing, so only the recorded cost can answer.
    #[test]
    fn resumed_session_opens_on_the_cost_its_turns_recorded() {
        let mut session = session_with_counters();
        session.add_model_usage(
            UNRESOLVABLE_MODEL,
            session.token_usage.billed(Some(RECORDED_COST)),
        );
        let state = resumed(session);
        assert_eq!(state.cost, Some(RECORDED_COST));
    }

    /// The list price is written per model and never re-derived, so a resumed
    /// subsidised session has to add the stored rows back up. Dropping it left
    /// the status bar on a bare `$0.000` until the next turn landed.
    #[test]
    fn resumed_session_restores_the_recorded_list_price() {
        let mut session = session_with_counters();
        session.add_model_usage(
            UNRESOLVABLE_MODEL,
            session
                .token_usage
                .billed_with_subsidised_list_cost(Some(0.0), Some(RECORDED_LIST_COST)),
        );
        let state = resumed(session);
        assert_eq!(state.subsidised_list_cost, Some(RECORDED_LIST_COST));
    }

    /// Nothing subsidised ever ran, so there is no reference figure to show
    /// and the status bar must not invent one.
    #[test]
    fn resumed_metered_session_has_no_list_price() {
        let mut session = session_with_counters();
        session.add_model_usage(
            UNRESOLVABLE_MODEL,
            session.token_usage.billed(Some(RECORDED_COST)),
        );
        assert_eq!(resumed(session).subsidised_list_cost, None);
    }

    #[test]
    fn plan_mode_waits_for_the_host_to_assign_its_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session = make_plan_session(Some(StoredMode::Plan), None);
        let state = SessionState::from_session(OpenSession::claimed(session, &storage));
        assert_eq!(state.mode, Mode::Plan);
        assert!(state.plan.path().is_none());
    }

    #[test]
    fn plan_mode_with_existing_file_preserves_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let plan_file = tmp.path().join("existing-plan.md");
        std::fs::write(&plan_file, "# Plan").unwrap();
        let session = make_plan_session(
            Some(StoredMode::Plan),
            Some(plan_file.to_string_lossy().into_owned()),
        );
        let state = SessionState::from_session(OpenSession::claimed(session, &storage));
        assert_eq!(state.mode, Mode::Plan);
        assert_eq!(state.plan.path(), Some(plan_file.as_path()));
    }

    #[test]
    fn build_mode_does_not_allocate_path() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let session = make_plan_session(Some(StoredMode::Build), None);
        let state = SessionState::from_session(OpenSession::claimed(session, &storage));
        assert_eq!(state.mode, Mode::Build);
        assert!(state.plan.path().is_none());
    }
}
