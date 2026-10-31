//! `/btw`: a side question answered from the session's transcript in one
//! request with no tools. The answer stays out of the session.

use futures_lite::future;
use maki_providers::provider::Provider;
use maki_providers::{AgentError, Message, Model, ProviderEvent, RequestOptions};
use maki_storage::id::SessionRef;
pub use maki_types::btw::SideAnswer;
use serde_json::Value;

use crate::agent::{HistorySnapshot, UNAVAILABLE_RESULT, close_dangling_tool_calls};

const BTW_REMINDER: &str = "<system-reminder>\nThis is a side question. Answer it directly in a \
single response.\n- You have NO tools: you cannot read files, run commands, or take any action.\n\
- One-off response: there are no follow-up turns.\n- Answer ONLY from the existing conversation \
context.\n- Never say \"Let me...\", \"I'll now...\", or promise any action.\n- If you don't know, \
say so; do not offer to look it up.\n</system-reminder>";

pub const BTW_FALLBACK_SYSTEM: &str = "You are a helpful coding assistant. Answer concisely \
from the conversation context.";

/// The reminder leads so the model treats the question as a quick aside, not a task to act on.
fn btw_question(question: &str) -> Message {
    Message::user(format!("{BTW_REMINDER}\n\n{question}"))
}

/// Streams the answer into `answer` and returns once the stream ends.
/// Forwarding stops when `answer` is closed; the request still runs out.
pub async fn side_question(
    provider: &dyn Provider,
    model: &Model,
    snapshot: Option<&HistorySnapshot>,
    question: &str,
    session_id: Option<&SessionRef>,
    answer: &flume::Sender<SideAnswer>,
) -> Result<(), AgentError> {
    // The mirror is verbatim, so mid-turn it can end on an open tool call.
    // Providers reject that, so close them off on our own copy.
    let mut messages = snapshot
        .map(|snapshot| Vec::clone(&snapshot.messages))
        .unwrap_or_default();
    close_dangling_tool_calls(&mut messages, UNAVAILABLE_RESULT);
    // The session's own prompt, so the side question reads the transcript
    // under the instructions it was written under.
    let system = snapshot
        .and_then(|snapshot| snapshot.frame.as_ref())
        .and_then(|frame| frame.prefix())
        .unwrap_or(BTW_FALLBACK_SYSTEM);
    messages.push(btw_question(question));
    let messages = maki_providers::adapt_images_for_model(model, &messages).await;
    let tools = Value::Array(Vec::new());

    let (event_tx, event_rx) = flume::unbounded();
    // Owns the sender, so the forwarder sees the channel close the moment
    // the stream ends and the two finish together.
    let stream = async move {
        provider
            .stream_message(
                model,
                &messages,
                system,
                &tools,
                &event_tx,
                RequestOptions::default(),
                session_id,
            )
            .await
    };
    // Owns the receiver too, so once `answer` closes the provider's later
    // deltas are refused instead of queued until the stream ends.
    let forward = async move {
        while let Ok(event) = event_rx.recv_async().await {
            let delta = match event {
                ProviderEvent::TextDelta { text } => SideAnswer::Text(text),
                ProviderEvent::ThinkingDelta { text } => SideAnswer::Thinking(text),
                _ => continue,
            };
            if answer.send(delta).is_err() {
                return;
            }
        }
    };
    let (result, ()) = future::zip(stream, forward).await;
    result.map(drop)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use maki_providers::provider::BoxFuture;
    use maki_providers::{ContentBlock, ModelInfo, StreamResponse};
    use maki_storage::frame::{PromptFacts, StoredFrame};
    use test_case::test_case;

    use super::*;

    const QUESTION: &str = "why sqlite?";
    const FRAME_POLICY: &str = "Use the active conversation policy.";
    const ANSWER: &str = "it is embedded";
    const FAILURE: &str = "provider is down";
    /// Far more polls than the forwarder needs to see a closed answer.
    const RELEASE_POLLS: usize = 100;

    /// What the provider was sent: the system, the messages and the tools.
    type Sent = (String, Vec<Message>, Value);

    /// Answers with one delta, then succeeds or fails, and records what it
    /// was sent.
    struct OneShot {
        fails: bool,
        sent: flume::Sender<Sent>,
    }

    impl Provider for OneShot {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            system: &'a str,
            tools: &'a Value,
            event_tx: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.sent
                    .send((system.to_owned(), messages.to_vec(), tools.clone()))
                    .unwrap();
                event_tx
                    .send(ProviderEvent::TextDelta {
                        text: ANSWER.into(),
                    })
                    .unwrap();
                match self.fails {
                    true => Err(AgentError::Config {
                        message: FAILURE.into(),
                    }),
                    false => Ok(StreamResponse::default()),
                }
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    /// Sends one delta, then waits for the forwarder to let the channel go.
    struct Lingering {
        released: Arc<AtomicBool>,
    }

    impl Provider for Lingering {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            event_tx: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                let _ = event_tx.send(ProviderEvent::TextDelta {
                    text: ANSWER.into(),
                });
                for _ in 0..RELEASE_POLLS {
                    if event_tx.is_disconnected() {
                        self.released.store(true, Ordering::SeqCst);
                        break;
                    }
                    future::yield_now().await;
                }
                Ok(StreamResponse::default())
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    /// The modal closed: the rest of the stream must not pile up unread.
    #[test]
    fn a_closed_answer_lets_the_provider_channel_go() {
        let released = Arc::new(AtomicBool::new(false));
        let (answer, answers) = flume::unbounded();
        drop(answers);
        let provider = Lingering {
            released: Arc::clone(&released),
        };
        crate::test_block_on(side_question(
            &provider,
            &model(),
            None,
            QUESTION,
            None,
            &answer,
        ))
        .unwrap();
        assert!(released.load(Ordering::SeqCst));
    }

    fn model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
    }

    fn ask(
        fails: bool,
        snapshot: Option<&HistorySnapshot>,
    ) -> (Result<(), AgentError>, Sent, Vec<SideAnswer>) {
        let (sent, requests) = flume::unbounded();
        let (answer, answers) = flume::unbounded();
        let result = crate::test_block_on(side_question(
            &OneShot { fails, sent },
            &model(),
            snapshot,
            QUESTION,
            None,
            &answer,
        ));
        let request = requests.try_recv().unwrap();
        (result, request, answers.try_iter().collect())
    }

    fn with_frame(frame: StoredFrame) -> HistorySnapshot {
        let mut snapshot = HistorySnapshot::new(Vec::new());
        snapshot.frame = Some(Arc::new(frame));
        snapshot
    }

    /// The answer receiver stays open, as the modal's does, and the call
    /// still ends with the stream.
    #[test_case(false ; "success")]
    #[test_case(true ; "failure")]
    fn a_finite_answer_ends_with_its_result(fails: bool) {
        let (result, _, answers) = ask(fails, None);
        assert_eq!(result.is_err(), fails);
        assert!(matches!(&answers[..], [SideAnswer::Text(text)] if text == ANSWER));
    }

    /// An imported session holds a frame with facts and no prompt until its
    /// first run, and an empty system is a 400 on Anthropic.
    #[test_case(None, BTW_FALLBACK_SYSTEM ; "native_fallback")]
    #[test_case(Some(StoredFrame::new(FRAME_POLICY.into(), String::new(), None, Vec::new())), FRAME_POLICY ; "stored_frame_precedence")]
    #[test_case(Some(StoredFrame::facts_only(PromptFacts::default())), BTW_FALLBACK_SYSTEM ; "imported_before_first_run")]
    fn a_side_question_uses_the_sessions_policy(frame: Option<StoredFrame>, expected: &str) {
        let snapshot = frame.map(with_frame);
        let (_, (system, _, tools), _) = ask(false, snapshot.as_ref());
        assert_eq!(system, expected);
        assert!(tools.as_array().unwrap().is_empty());
    }

    /// The mirror is verbatim mid-turn, and providers reject an open call.
    #[test]
    fn a_dangling_tool_call_is_closed_and_the_reminder_leads_the_question() {
        let call = Message {
            role: maki_providers::Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "bash".into(),
                input: Value::Null,
                thought_signature: None,
            }],
            ..Default::default()
        };
        let snapshot = HistorySnapshot::new(vec![call]);
        let (_, (_, messages, _), _) = ask(false, Some(&snapshot));
        let [_, closed, question] = &messages[..] else {
            panic!("expected the call, its closing result and the question");
        };
        assert!(
            closed
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        );
        let text = question.user_text().unwrap();
        assert!(text.starts_with(BTW_REMINDER), "reminder leads the message");
        assert!(text.ends_with(QUESTION), "question trails the message");
    }
}
