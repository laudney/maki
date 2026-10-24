//! Who owns a permission ask that a frontend put in front of its user, so
//! ACP and the SDK retire an ask by the same rules.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;

use crate::{CancelToken, DETACHED_RUN_ID, Envelope};

/// A main-agent ask ends with its run. A child's ask ends with the child, and
/// with its run while the child is still attached to it. Every ask ends with
/// the session generation that owns it.
#[derive(Clone, Debug)]
pub struct AskOwner {
    run_id: Option<u64>,
    child: Option<String>,
    generation: Option<CancelToken>,
}

impl AskOwner {
    pub fn run(run_id: u64) -> Self {
        Self {
            run_id: Some(run_id),
            child: None,
            generation: None,
        }
    }

    pub fn of(envelope: &Envelope) -> Self {
        let Some(info) = &envelope.subagent else {
            return Self::run(envelope.run_id);
        };
        Self {
            run_id: (!info.detached && envelope.run_id != DETACHED_RUN_ID)
                .then_some(envelope.run_id),
            child: Some(info.parent_tool_use_id.clone()),
            generation: info.owner_cancel.clone(),
        }
    }

    fn is_live(&self) -> bool {
        self.generation
            .as_ref()
            .is_none_or(|generation| !generation.is_cancelled())
    }
}

/// Outstanding asks keyed by the frontend's own request id.
pub struct OwnedAsks<K, A> {
    asks: HashMap<K, (AskOwner, A)>,
}

impl<K, A> Default for OwnedAsks<K, A> {
    fn default() -> Self {
        Self {
            asks: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash, A> OwnedAsks<K, A> {
    pub fn insert(&mut self, key: K, owner: AskOwner, ask: A) {
        self.asks.insert(key, (owner, ask));
    }

    /// The ask an answer is for, unless the generation that owns it ended.
    pub fn take<Q: Eq + Hash + ?Sized>(&mut self, key: &Q) -> Option<A>
    where
        K: Borrow<Q>,
    {
        self.asks
            .remove(key)
            .filter(|(owner, _)| owner.is_live())
            .map(|(_, ask)| ask)
    }

    pub fn end_run(&mut self, run_id: u64) {
        self.asks
            .retain(|_, (owner, _)| owner.run_id != Some(run_id) && owner.is_live());
    }

    pub fn end_child(&mut self, child: &str) {
        self.asks
            .retain(|_, (owner, _)| owner.child.as_deref() != Some(child) && owner.is_live());
    }

    pub fn end_cancelled(&mut self) {
        self.asks.retain(|_, (owner, _)| owner.is_live());
    }

    pub fn clear(&mut self) {
        self.asks.clear();
    }

    pub fn drain(&mut self) -> impl Iterator<Item = (K, A)> + '_ {
        self.asks.drain().map(|(key, (_, ask))| (key, ask))
    }

    pub fn len(&self) -> usize {
        self.asks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.asks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::{AgentEvent, SubagentInfo};

    const RUN: u64 = 7;
    const OTHER_RUN: u64 = 8;
    const CHILD: &str = "child";
    const MAIN_ASK: u64 = 1;
    const CHILD_ASK: u64 = 2;

    fn child_envelope(run_id: u64, detached: bool, generation: CancelToken) -> Envelope {
        Envelope {
            event: AgentEvent::StreamClosed,
            subagent: Some(SubagentInfo {
                parent_tool_use_id: CHILD.into(),
                name: CHILD.into(),
                prompt: None,
                model: None,
                detached,
                owner_cancel: Some(generation),
                spawn_tool_use_id: None,
                opts: None,
                answer_tx: None,
                inbox: None,
            }),
            run_id,
        }
    }

    fn asks(child: &Envelope) -> OwnedAsks<u64, ()> {
        let mut asks = OwnedAsks::default();
        asks.insert(MAIN_ASK, AskOwner::run(RUN), ());
        asks.insert(CHILD_ASK, AskOwner::of(child), ());
        asks
    }

    #[test_case(RUN, false, false; "attached_child_ends_with_its_run")]
    #[test_case(RUN, true, true; "detached_child_outlives_the_run")]
    #[test_case(DETACHED_RUN_ID, false, true; "generation_routed_child_outlives_the_run")]
    fn ending_a_run_keeps_only_asks_that_outlive_it(run_id: u64, detached: bool, kept: bool) {
        let mut asks = asks(&child_envelope(run_id, detached, CancelToken::none()));
        asks.end_run(OTHER_RUN);
        assert_eq!(asks.len(), 2);
        asks.end_run(RUN);
        assert!(asks.take(&MAIN_ASK).is_none());
        assert_eq!(asks.take(&CHILD_ASK).is_some(), kept);
    }

    #[test]
    fn a_child_keeps_its_asks_until_it_ends() {
        let mut asks = asks(&child_envelope(RUN, true, CancelToken::none()));
        asks.end_run(RUN);
        assert_eq!(asks.len(), 1);
        asks.end_child(CHILD);
        assert!(asks.is_empty());
    }

    #[test_case(false; "on_answer")]
    #[test_case(true; "on_sweep")]
    fn an_ended_generation_retires_its_asks(sweep: bool) {
        let (owner, generation) = CancelToken::new();
        let mut asks = asks(&child_envelope(DETACHED_RUN_ID, true, generation));
        owner.cancel();
        if sweep {
            asks.end_cancelled();
            assert_eq!(asks.len(), 1);
        }
        assert!(asks.take(&CHILD_ASK).is_none());
        assert!(asks.take(&MAIN_ASK).is_some());
    }
}
