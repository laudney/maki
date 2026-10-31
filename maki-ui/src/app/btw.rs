use std::fmt::Display;
use std::future::Future;

use flume::{Receiver, Sender};
use futures_lite::future;
use maki_types::btw::SideAnswer;

use crate::agent::RemoteSession;
use crate::agent::capabilities::SESSION_OPENING;
use crate::components::btw_modal::BtwEvent;

use super::App;

const BTW_BUFFER: usize = 64;

impl App {
    /// The agent answers from its transcript, on its model.
    pub(crate) fn start_remote_btw(&mut self, question: String, remote: RemoteSession) {
        // Until the loop installs a `/new`, the client still names the old
        // session, whose conversation the user has left.
        let Some(session) = remote.session() else {
            self.flash(SESSION_OPENING.into());
            return;
        };
        let (answer, answers) = flume::unbounded();
        let asked = question.clone();
        let ask = async move { remote.client.btw(session, asked, answer).await };
        self.answer_btw(&question, answers, ask);
    }

    /// Opens the modal on `question` and draws the answer `ask` streams.
    /// `ask` owns the answer's sender, so the stream ends with it.
    pub(crate) fn answer_btw<E: Display + Send + 'static>(
        &mut self,
        question: &str,
        answers: Receiver<SideAnswer>,
        ask: impl Future<Output = Result<(), E>> + Send + 'static,
    ) {
        let (btw_tx, btw_rx) = flume::bounded(BTW_BUFFER);
        self.open_prompt(|app| app.btw_modal.open(question, btw_rx));
        tokio::spawn(into_modal(btw_tx, answers, ask));
    }
}

async fn into_modal<E: Display + Send>(
    btw_tx: Sender<BtwEvent>,
    answers: Receiver<SideAnswer>,
    ask: impl Future<Output = Result<(), E>>,
) {
    // Owns the receiver, so a closed modal refuses the rest of the answer
    // instead of queueing it until `ask` ends.
    let forward_tx = btw_tx.clone();
    let forward = async move {
        while let Ok(answer) = answers.recv_async().await {
            let event = match answer {
                SideAnswer::Text(text) => BtwEvent::TextDelta(text),
                SideAnswer::Thinking(text) => BtwEvent::ThinkingDelta(text),
            };
            if forward_tx.send_async(event).await.is_err() {
                return;
            }
        }
    };
    let (result, ()) = future::zip(ask, forward).await;
    let end = match result {
        Ok(()) => BtwEvent::Done,
        Err(e) => BtwEvent::Error(e.to_string()),
    };
    let _ = btw_tx.send_async(end).await;
}
