//! Who owns a permission ask that a frontend put in front of its user, and
//! where its answer goes, so ACP and the SDK route and retire an ask by the
//! same rules.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;

use flume::Sender;
use maki_storage::id::MakiId;
use tracing::warn;

use crate::cancel::CancelMap;
use crate::{AgentEvent, CancelToken, DETACHED_RUN_ID, Envelope, SessionMailbox};

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

/// Where the answer to one permission ask goes.
pub struct PermissionRoute {
    pub owner: AskOwner,
    /// The call whose input the ask is about.
    pub input_id: String,
    /// `None` sends the answer down the main agent's own channel.
    pub answer_tx: Option<Sender<String>>,
}

impl PermissionRoute {
    /// Routes the ask {envelope} published as {request_id}. A moved call's
    /// ask answers through the call, a child's through the child. A child
    /// ask with no channel would park the child for good, so the child is
    /// cancelled and `None` comes back.
    pub fn of(
        session: MakiId,
        envelope: &Envelope,
        request_id: &str,
        cancels: &CancelMap<String>,
    ) -> Option<Self> {
        let managed = SessionMailbox::tool_permission(session, request_id);
        let child = envelope.subagent.as_ref();
        let input_id = managed
            .as_ref()
            .map(|(original, _)| original.clone())
            .or_else(|| child.and_then(|info| info.permission_tool_use_id.clone()))
            .unwrap_or_else(|| request_id.to_owned());
        let answer_tx = managed
            .map(|(_, tx)| tx)
            .or_else(|| child.and_then(|info| info.answer_tx.clone()));
        if let Some(child) = child
            && answer_tx.is_none()
        {
            warn!(
                child = %child.parent_tool_use_id,
                ask_id = request_id,
                "child permission ask has no answer channel, cancelling the child"
            );
            cancels.cancel(child.parent_tool_use_id.clone());
            return None;
        }
        Some(Self {
            owner: AskOwner::of(envelope),
            input_id,
            answer_tx,
        })
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

    /// The child outlives the run that opened it, so its asks do too.
    fn detach_child(&mut self, child: &str) {
        for (owner, _) in self.asks.values_mut() {
            if owner.child.as_deref() == Some(child) {
                owner.run_id = None;
            }
        }
    }

    pub fn end_child(&mut self, child: &str) {
        self.asks
            .retain(|_, (owner, _)| owner.child.as_deref() != Some(child) && owner.is_live());
    }

    /// Follows the child lifecycle {envelope} reports: a detached child
    /// keeps its asks past the run, and a child whose history closed takes
    /// them with it.
    pub fn observe(&mut self, envelope: &Envelope) {
        match &envelope.event {
            AgentEvent::SubagentDetached { child } => self.detach_child(&child.parent_tool_use_id),
            AgentEvent::SubagentHistory { tool_use_id, .. } => self.end_child(tool_use_id),
            _ => {}
        }
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
    use crate::{SubagentInfo, TaskKind};

    const RUN: u64 = 7;
    const OTHER_RUN: u64 = 8;
    const CHILD: &str = "child";
    const MAIN_ASK: u64 = 1;
    const CHILD_ASK: u64 = 2;
    const ASK_ID: &str = "ask";
    const CHILD_CALL: &str = "child-call";

    fn child_envelope(run_id: u64, detached: bool, generation: CancelToken) -> Envelope {
        Envelope {
            event: AgentEvent::StreamClosed,
            subagent: Some(SubagentInfo {
                kind: TaskKind::Agent,
                parent_tool_use_id: CHILD.into(),
                name: CHILD.into(),
                prompt: None,
                model: None,
                detached,
                owner_cancel: Some(generation),
                spawn_tool_use_id: None,
                spawn_parent_task_id: None,
                opts: None,
                answer_tx: None,
                permission_tool_use_id: None,
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
    fn a_detached_child_keeps_its_asks_until_it_ends() {
        let child = || child_envelope(RUN, false, CancelToken::none());
        let mut asks = asks(&child());
        let mut detached = child();
        detached.event = AgentEvent::SubagentDetached {
            child: Box::new(child().subagent.unwrap()),
        };
        asks.observe(&detached);
        asks.end_run(RUN);
        assert_eq!(asks.len(), 1);
        let mut ended = child();
        ended.event = AgentEvent::SubagentHistory {
            tool_use_id: CHILD.into(),
            messages: Vec::new(),
            status: None,
        };
        asks.observe(&ended);
        assert!(asks.is_empty());
    }

    #[test_case(false; "main_agent")]
    #[test_case(true; "child")]
    fn an_answerable_ask_routes_to_its_own_channel(child: bool) {
        let (answer_tx, answer_rx) = flume::unbounded();
        let envelope = match child {
            true => {
                let mut envelope = child_envelope(RUN, false, CancelToken::none());
                let info = envelope.subagent.as_mut().unwrap();
                info.answer_tx = Some(answer_tx);
                info.permission_tool_use_id = Some(CHILD_CALL.into());
                envelope
            }
            false => Envelope {
                event: AgentEvent::StreamClosed,
                subagent: None,
                run_id: RUN,
            },
        };
        let route =
            PermissionRoute::of(MakiId::generate(), &envelope, ASK_ID, &CancelMap::new()).unwrap();
        assert_eq!(route.input_id, if child { CHILD_CALL } else { ASK_ID });
        assert_eq!(route.answer_tx.is_some(), child);
        if let Some(tx) = route.answer_tx {
            tx.send(ASK_ID.into()).unwrap();
            assert_eq!(answer_rx.try_recv().as_deref(), Ok(ASK_ID));
        }
    }

    #[test]
    fn a_child_ask_nobody_can_answer_cancels_the_child() {
        let cancels = CancelMap::new();
        let (trigger, token) = CancelToken::new();
        cancels.insert(CHILD.to_owned(), trigger);
        let envelope = child_envelope(RUN, false, CancelToken::none());
        assert!(PermissionRoute::of(MakiId::generate(), &envelope, ASK_ID, &cancels).is_none());
        assert!(token.is_cancelled());
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
