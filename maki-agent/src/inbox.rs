//! The subagent's loop drains its inbox between turns as one
//! `<user-interrupt>`.

use std::future;

use maki_providers::provider::BoxFuture;
pub use maki_types::inbox::SubagentInbox;

use crate::{AgentMode, CancelToken, ExtractedCommand, InterruptSource};

impl InterruptSource for SubagentInbox {
    /// The cancel is checked under the lock, so a cancelled run leaves what
    /// came after it for the next one.
    fn poll<'a>(
        &'a self,
        cancel: &'a CancelToken,
        _: &'a AgentMode,
    ) -> BoxFuture<'a, Option<ExtractedCommand>> {
        let drained = self
            .drain_unless_cancelled(cancel)
            .map(ExtractedCommand::Interrupt);
        Box::pin(future::ready(drained))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentInput, AgentMode, InputSource};
    use maki_providers::ThinkingConfig;

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

    fn poll_now(queue: &SubagentInbox) -> Option<ExtractedCommand> {
        crate::test_block_on(queue.poll(&CancelToken::none(), &AgentMode::Build))
    }

    #[test]
    fn a_cancelled_poll_leaves_the_burst() {
        let queue = SubagentInbox::default();
        queue.push(input("a"));
        let (trigger, cancel) = CancelToken::new();
        trigger.cancel();

        assert!(crate::test_block_on(queue.poll(&cancel, &AgentMode::Build)).is_none());
        assert_eq!(queue.texts(), ["a"]);
    }

    #[test]
    fn poll_drains_the_burst_in_order_and_empties_the_queue() {
        let queue = SubagentInbox::default();
        assert!(poll_now(&queue).is_none());
        queue.push(input("a"));
        queue.push(input("b"));
        assert_eq!(queue.texts(), ["a", "b"]);

        let Some(ExtractedCommand::Interrupt(inputs)) = poll_now(&queue) else {
            panic!("a non-empty queue polls as one interrupt");
        };
        let texts: Vec<_> = inputs.into_iter().map(|i| i.message).collect();
        assert_eq!(texts, ["a", "b"]);
        assert!(queue.is_empty());
        assert!(poll_now(&queue).is_none());
    }
}
