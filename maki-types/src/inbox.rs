//! Messages a user queues for a subagent while it runs. The host pushes,
//! the subagent's loop drains them between turns as one `<user-interrupt>`.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use event_listener::Event;

use crate::cancel::CancelToken;
use crate::input::AgentInput;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InboxSnapshot {
    pub revision: u64,
    pub texts: Vec<String>,
}

#[derive(Default)]
struct Items {
    revision: u64,
    queue: VecDeque<AgentInput>,
}

#[derive(Default)]
pub struct SubagentInbox {
    items: Mutex<Items>,
    pushed: Event,
}

impl SubagentInbox {
    pub fn push(&self, input: AgentInput) {
        let mut items = self.lock();
        items.revision += 1;
        items.queue.push_back(input);
        drop(items);
        self.pushed.notify(usize::MAX);
    }

    pub async fn wait_nonempty(&self) {
        loop {
            let listener = self.pushed.listen();
            if !self.is_empty() {
                return;
            }
            listener.await;
        }
    }

    pub fn drain(&self) -> Vec<AgentInput> {
        let mut items = self.lock();
        items.revision += u64::from(!items.queue.is_empty());
        items.queue.drain(..).collect()
    }

    pub fn remove(&self, index: usize) -> Option<AgentInput> {
        let mut items = self.lock();
        let removed = items.queue.remove(index);
        items.revision += u64::from(removed.is_some());
        removed
    }

    pub fn len(&self) -> usize {
        self.lock().queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().queue.is_empty()
    }

    pub fn texts(&self) -> Vec<String> {
        self.snapshot().texts
    }

    pub fn snapshot(&self) -> InboxSnapshot {
        let items = self.lock();
        InboxSnapshot {
            revision: items.revision,
            texts: items
                .queue
                .iter()
                .map(|input| input.message.clone())
                .collect(),
        }
    }

    /// The whole burst, oldest first, or `None` when the queue is empty or
    /// `cancel` fired. The cancel is checked under the lock, so a cancelled
    /// run leaves what came after it for the next one.
    pub fn drain_unless_cancelled(&self, cancel: &CancelToken) -> Option<Vec<AgentInput>> {
        let mut items = self.lock();
        if cancel.is_cancelled() {
            return None;
        }
        if items.queue.is_empty() {
            return None;
        }
        items.revision += 1;
        Some(items.queue.drain(..).collect())
    }

    fn lock(&self) -> MutexGuard<'_, Items> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for SubagentInbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubagentInbox")
            .field("len", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{AgentMode, InputSource};
    use crate::thinking::ThinkingConfig;

    const BURST: [&str; 2] = ["a", "b"];

    fn input(message: &str) -> AgentInput {
        AgentInput {
            message: message.into(),
            mode: AgentMode::Build,
            images: Vec::new(),
            preamble: Vec::new(),
            earlier: Vec::new(),
            thinking: ThinkingConfig::default(),
            fast: false,
            workflow: false,
            prompt: None,
            source: InputSource::Tui,
        }
    }

    fn queued_burst() -> SubagentInbox {
        let queue = SubagentInbox::default();
        for message in BURST {
            queue.push(input(message));
        }
        queue
    }

    #[test]
    fn drain_unless_cancelled_leaves_the_burst_for_a_cancelled_run() {
        let queue = queued_burst();
        let (trigger, cancel) = CancelToken::new();
        trigger.cancel();

        assert!(queue.drain_unless_cancelled(&cancel).is_none());
        assert_eq!(queue.texts(), BURST);
    }

    #[test]
    fn drain_unless_cancelled_takes_the_whole_burst_for_a_live_run() {
        let queue = queued_burst();

        let drained = queue.drain_unless_cancelled(&CancelToken::none());
        let texts: Vec<_> = drained.into_iter().flatten().map(|i| i.message).collect();
        assert_eq!(texts, BURST);
        assert!(queue.is_empty());
    }

    #[test]
    fn remove_takes_one_entry_by_position() {
        let queue = SubagentInbox::default();
        queue.push(input("a"));
        queue.push(input("b"));
        assert_eq!(queue.remove(0).map(|i| i.message).as_deref(), Some("a"));
        assert_eq!(queue.texts(), ["b"]);
        assert!(queue.remove(5).is_none());
    }
}
