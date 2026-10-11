use std::sync::Arc;

use flume::Sender;
use futures_lite::future;
use maki_agent::prompt::PromptId;
use maki_agent::template;
use maki_providers::provider::Provider;
use maki_providers::{Message, Model, ProviderEvent, RequestOptions};
use maki_storage::id::SessionRef;
use serde_json::Value;

use crate::components::btw_modal::BtwEvent;

use super::App;

const BTW_REMINDER: &str = "<system-reminder>\nThis is a side question. Answer it directly in a \
single response.\n- You have NO tools: you cannot read files, run commands, or take any action.\n\
- One-off response: there are no follow-up turns.\n- Answer ONLY from the existing conversation \
context.\n- Never say \"Let me...\", \"I'll now...\", or promise any action.\n- If you don't know, \
say so; do not offer to look it up.\n</system-reminder>";

/// The reminder leads so the model treats the question as a quick aside, not a task to act on.
pub(crate) fn btw_question(question: &str) -> Message {
    Message::user(format!("{BTW_REMINDER}\n\n{question}"))
}

impl App {
    pub(crate) fn start_btw(
        &mut self,
        question: String,
        provider: Arc<dyn Provider>,
        model: Model,
    ) {
        // The mirror is verbatim, so mid-turn it can end on an open tool call.
        // Providers reject that, so close them off on our own copy.
        let snapshot = self.shared_history.as_ref().map(|h| h.load_full());
        let mut messages = snapshot
            .as_ref()
            .map(|s| Vec::clone(&s.messages))
            .unwrap_or_default();
        maki_agent::close_dangling_tool_calls(&mut messages, maki_agent::UNAVAILABLE_RESULT);
        // The session's own prompt, so the side question reads the transcript
        // under the instructions it was written under.
        let system = snapshot
            .as_ref()
            .and_then(|s| s.frame.as_ref())
            .map(|f| f.system.clone());
        messages.push(btw_question(&question));

        let (tx, rx) = flume::bounded(64);
        self.btw_modal.open(&question, rx);

        let session_id = SessionRef::from(self.state.session.id);
        let lua_handle = self.lua_event_handle.clone();
        smol::spawn(async move {
            let system = match system {
                Some(system) => system,
                None => lua_handle.collect_prompt_slots_async().await.render(
                    PromptId::BtwFallback,
                    &template::env_vars(),
                    "",
                ),
            };
            run_btw(provider, model, system, messages, tx, Some(session_id)).await;
        })
        .detach();
    }
}

async fn run_btw(
    provider: Arc<dyn Provider>,
    model: Model,
    system: String,
    messages: Vec<Message>,
    btw_tx: Sender<BtwEvent>,
    session_id: Option<SessionRef>,
) {
    let (event_tx, event_rx) = flume::unbounded();
    let tools = Value::Array(vec![]);
    let messages = maki_providers::adapt_images_for_model(&model, &messages).await;

    let stream_fut = provider.stream_message(
        &model,
        &messages,
        &system,
        &tools,
        &event_tx,
        RequestOptions::default(),
        session_id.as_ref(),
    );

    let forward_fut = async {
        while let Ok(event) = event_rx.recv_async().await {
            let event = match event {
                ProviderEvent::TextDelta { text } => BtwEvent::TextDelta(text),
                ProviderEvent::ThinkingDelta { text } => BtwEvent::ThinkingDelta(text),
                _ => continue,
            };
            if btw_tx.send(event).is_err() {
                return;
            }
        }
    };

    let (result, _) = future::zip(stream_fut, forward_fut).await;

    match result {
        Ok(_) => {
            let _ = btw_tx.send(BtwEvent::Done);
        }
        Err(e) => {
            let _ = btw_tx.send(BtwEvent::Error(e.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: &str = "why sqlite?";

    fn user_text(msg: &Message) -> String {
        msg.content
            .iter()
            .filter_map(|b| match b {
                maki_providers::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn injects_reminder_before_question() {
        let text = user_text(&btw_question(Q));
        assert!(text.starts_with(BTW_REMINDER), "reminder leads the message");
        assert!(text.ends_with(Q), "question trails the message");
    }
}
