//! A remote tab's unsent draft and queue, kept in the client's own store
//! per agent and session, as a local tab keeps them in its session meta.

use std::time::{Duration, Instant};

use agent_client_protocol_schema::v1::SessionId;
use maki_storage::remote_drafts::{DraftClaim, RemoteDraft, RemoteStore};
use tracing::warn;

use crate::agent::RemoteSession;
use crate::agent::capabilities::AgentBackend;

use super::App;

/// The record a tab writes, and what it wrote last.
pub(crate) struct RemoteDrafts {
    store: RemoteStore,
    link: RemoteSession,
    claim: Claim,
    written: RemoteDraft,
    at: Instant,
}

enum Claim {
    None,
    /// Held for as long as the tab is bound to the session.
    Held(SessionId, DraftClaim),
    /// Another client holds it: this tab keeps its input in memory only.
    Refused(SessionId),
}

impl RemoteDrafts {
    /// A `resumed` tab claims its session's record at once and starts from
    /// what it holds.
    pub(crate) fn new(store: RemoteStore, link: RemoteSession, resumed: bool) -> Self {
        let mut drafts = Self {
            store,
            link,
            claim: Claim::None,
            written: RemoteDraft::default(),
            at: Instant::now(),
        };
        if let Some(session) = resumed.then(|| drafts.link.session()).flatten()
            && drafts.claim_for(session)
            && let Claim::Held(_, claim) = &drafts.claim
        {
            drafts.written = claim.load();
        }
        drafts
    }

    pub(crate) fn native(store: RemoteStore, link: RemoteSession, fallback: RemoteDraft) -> Self {
        let mut drafts = Self::new(store, link, true);
        if let Claim::Held(_, claim) = &drafts.claim {
            drafts.written = claim.load_or(fallback);
        }
        drafts
    }

    pub(crate) fn restored(&self) -> RemoteDraft {
        self.written.clone()
    }

    /// The session to write for: the bound one once it is installed, and
    /// the old one when a replacement failed and ended the link; nothing
    /// while a `/new` is still opening, so the left session gets none of
    /// the new one's input.
    fn target(&self, backend: AgentBackend) -> Option<SessionId> {
        match backend {
            AgentBackend::Disconnected => self.link.client.bound_session().ok(),
            _ => self.link.session(),
        }
    }

    fn claimed(&self) -> Option<&SessionId> {
        match &self.claim {
            Claim::Held(session, _) | Claim::Refused(session) => Some(session),
            Claim::None => None,
        }
    }

    /// Claims `session`'s record when the tab moved to it, and tells
    /// whether the tab holds it.
    fn claim_for(&mut self, session: SessionId) -> bool {
        if self.claimed() != Some(&session) {
            self.claim = match self.store.claim(&session.to_string()) {
                Ok(Some(claim)) => Claim::Held(session, claim),
                Ok(None) => Claim::Refused(session),
                Err(error) => {
                    warn!(%error, "cannot claim the remote draft record");
                    Claim::Refused(session)
                }
            };
            self.written = RemoteDraft::default();
        }
        matches!(self.claim, Claim::Held(..))
    }

    fn write(&mut self, draft: RemoteDraft, backend: AgentBackend, soft_delay: Duration) {
        let Some(session) = self.target(backend) else {
            return;
        };
        // A session the tab never typed into gets no record, nor a lock file,
        // and the session it left is free for another client.
        if draft.is_empty() && self.claimed() != Some(&session) {
            self.claim = Claim::None;
            return;
        }
        if !self.claim_for(session) || draft == self.written {
            return;
        }
        let queue_moved = draft.queued_messages != self.written.queued_messages;
        if !queue_moved && self.at.elapsed() < soft_delay {
            return;
        }
        let Claim::Held(_, claim) = &self.claim else {
            return;
        };
        match claim.save(&draft) {
            Ok(()) => {
                self.written = draft;
                self.at = Instant::now();
            }
            Err(error) => warn!(%error, "cannot save the remote draft"),
        }
    }
}

impl App {
    /// The input half of a resume: the draft into the box, the queue back
    /// into the queue, which runs it. The replay stays as drawn.
    pub(crate) fn restore_remote_input(&mut self, draft: RemoteDraft) {
        if self.backend.is_native() && draft.input_draft.is_none() {
            self.restore_input("", None);
        }
        if let Some(text) = draft.input_draft {
            self.restore_input(&text, draft.input_draft_writers);
        }
        self.restore_queue(draft.queued_messages);
    }

    /// A queue change is written at once, so a dispatched message leaves the
    /// record before a crash could replay it; a draft change alone waits for
    /// `soft_delay`, as a local session's meta does.
    pub(super) fn checkpoint_remote(&mut self, soft_delay: Duration) {
        let draft = self.unsent();
        let backend = self.backend;
        if let Some(drafts) = &mut self.remote_drafts {
            drafts.write(draft, backend, soft_delay);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures_lite::future;
    use maki_storage::StateDir;
    use maki_wire::client::{AcpClient, Link};
    use maki_wire::methods;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::super::QueuedMessage;
    use super::super::tests::test_app;
    use super::*;

    const AGENT: &str = "maki acp in /work";
    const SESSION: &str = "sess-1";
    const OTHER_SESSION: &str = "sess-2";
    const CWD: &str = "/agent/workspace";
    const DRAFT: &str = "half a thought";
    const QUEUED: &str = "and run the tests";
    const NEXT: &str = "then the docs";
    const NEVER: Duration = Duration::from_secs(3600);

    /// A remote tab bound to `SESSION`, and the peer that keeps its link up.
    struct Tab {
        app: App,
        store: RemoteStore,
        link: RemoteSession,
        _dir: TempDir,
        _peer: (flume::Receiver<String>, flume::Sender<String>),
    }

    impl Tab {
        fn new(resumed: bool) -> Self {
            let dir = TempDir::new().unwrap();
            let store = RemoteStore::open(&StateDir::from_path(dir.path().into()), AGENT);
            Self::with_store(store, dir, resumed)
        }

        fn with_store(store: RemoteStore, dir: TempDir, resumed: bool) -> Self {
            let (client_tx, from_client) = flume::unbounded::<String>();
            let (to_client, client_rx) = flume::unbounded::<String>();
            let (client, _events) = AcpClient::new(Link::new(client_tx, client_rx));
            let peer = (from_client, to_client);
            open_session(&client, &peer, SESSION);
            let link = RemoteSession {
                rewind_tx: None,
                client: Arc::new(client),
                binding: Arc::default(),
            };
            let mut app = test_app();
            app.backend = AgentBackend::Remote {
                native: false,
                host: false,
                images: false,
                plan: true,
                settings: true,
                btw: true,
                usage_ledger: true,
                login: true,
                plan_file: false,
                reload: false,
            };
            app.remote_drafts = Some(RemoteDrafts::new(store.clone(), link.clone(), resumed));
            Self {
                app,
                store,
                link,
                _dir: dir,
                _peer: peer,
            }
        }

        /// A `/new` the loop installed.
        fn switch_to(&self, session: &str) {
            open_session(&self.link.client, &self._peer, session);
        }

        fn type_draft(&mut self, text: &str) {
            self.app.input_box.set_input(text);
        }

        fn queue(&mut self, text: &str) {
            self.app.queue_and_notify(QueuedMessage {
                text: text.into(),
                images: Vec::new(),
            });
        }

        /// The next run of `maki connect`, once this one let the record go.
        fn reopen(self, resumed: bool) -> Self {
            let Self {
                app, store, _dir, ..
            } = self;
            drop(app);
            Self::with_store(store, _dir, resumed)
        }

        /// What a later run finds, once this tab let the record go.
        fn saved(mut self) -> RemoteDraft {
            self.app.remote_drafts = None;
            self.store.claim(SESSION).unwrap().unwrap().load()
        }
    }

    fn open_session(
        client: &AcpClient,
        (from_client, to_client): &(flume::Receiver<String>, flume::Sender<String>),
        session: &str,
    ) {
        let (opened, ()) =
            crate::test_block_on(future::zip(client.new_session(CWD.into()), async {
                let request: Value = serde_json::from_str(&from_client.recv().unwrap()).unwrap();
                let result = methods::new_session_response(session);
                let answer = json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
                to_client.send(answer.to_string()).unwrap();
            }));
        opened.unwrap();
    }

    fn draft(input: Option<&str>, queued: &[&str]) -> RemoteDraft {
        RemoteDraft {
            input_draft: input.map(str::to_owned),
            input_draft_writers: None,
            queued_messages: queued.iter().map(|text| text.to_string()).collect(),
        }
    }

    #[test]
    fn a_checkpoint_now_keeps_the_draft_and_queue_of_the_bound_session() {
        let _runtime = crate::test_runtime().enter();
        let mut tab = Tab::new(false);
        tab.type_draft(DRAFT);
        tab.queue(QUEUED);
        tab.app.checkpoint_now();
        assert_eq!(tab.saved(), draft(Some(DRAFT), &[QUEUED]));
    }

    #[test]
    fn a_draft_alone_waits_for_the_soft_delay() {
        let _runtime = crate::test_runtime().enter();
        let mut tab = Tab::new(false);
        tab.type_draft(DRAFT);
        tab.app.checkpoint_with(NEVER);
        assert_eq!(tab.saved(), RemoteDraft::default());
    }

    /// A dispatch and a message queued in the same frame keep the queue's
    /// length, and the dispatched one must still leave the record.
    #[test_case(&[NEXT] ; "grows")]
    #[test_case(&[] ; "empties")]
    #[test_case(&[NEXT, QUEUED] ; "same_length_after_a_dispatch")]
    fn a_queue_change_is_written_at_once(after: &[&str]) {
        let _runtime = crate::test_runtime().enter();
        let mut tab = Tab::new(false);
        tab.queue(QUEUED);
        tab.app.checkpoint_now();
        tab.app.queue.clear();
        for text in after {
            tab.queue(text);
        }
        tab.app.checkpoint_with(NEVER);
        assert_eq!(tab.saved(), draft(None, after));
    }

    #[test]
    fn an_emptied_draft_leaves_nothing_to_restore() {
        let _runtime = crate::test_runtime().enter();
        let mut tab = Tab::new(false);
        tab.type_draft(DRAFT);
        tab.app.checkpoint_now();
        tab.type_draft("");
        tab.app.checkpoint_now();
        assert_eq!(tab.saved(), RemoteDraft::default());
    }

    /// While a `/new` opens nothing lands in the session the user left;
    /// once it failed and ended the link, the old session keeps the input.
    #[test]
    fn a_new_session_that_fails_writes_the_old_one() {
        let _runtime = crate::test_runtime().enter();
        let mut tab = Tab::new(false);
        tab.link.binding.request();
        tab.type_draft(DRAFT);
        tab.app.checkpoint_now();
        let refused = tab.store.claim(SESSION).unwrap();
        assert!(refused.is_some_and(|claim| claim.load().is_empty()));

        tab.app.connection_lost();
        tab.app.checkpoint_now();
        assert_eq!(tab.saved(), draft(Some(DRAFT), &[]));
    }

    /// The left session keeps what was typed in it, and its record is free
    /// for the next client once the tab moved on.
    #[test]
    fn a_new_session_frees_the_left_one() {
        let _runtime = crate::test_runtime().enter();
        let mut tab = Tab::new(false);
        tab.type_draft(DRAFT);
        tab.app.checkpoint_now();
        tab.switch_to(OTHER_SESSION);
        tab.type_draft("");
        tab.app.checkpoint_now();
        let left = tab.store.claim(SESSION).unwrap().unwrap();
        assert_eq!(left.load(), draft(Some(DRAFT), &[]));
    }

    #[test]
    fn a_tab_without_the_lock_writes_nothing() {
        let _runtime = crate::test_runtime().enter();
        let dir = TempDir::new().unwrap();
        let store = RemoteStore::open(&StateDir::from_path(dir.path().into()), AGENT);
        let other = store.claim(SESSION).unwrap().unwrap();
        let mut tab = Tab::with_store(store, dir, true);
        tab.type_draft(DRAFT);
        tab.app.checkpoint_now();
        assert!(other.load().is_empty());
    }

    /// The replay is drawn first; the restore only fills the input box and
    /// the queue.
    #[test]
    fn a_resumed_tab_restores_its_draft_and_queue() {
        let _runtime = crate::test_runtime().enter();
        let mut first = Tab::new(false);
        first.type_draft(DRAFT);
        first.queue(QUEUED);
        first.app.checkpoint_now();

        let mut tab = first.reopen(true);
        let restored = tab.app.remote_drafts.as_ref().unwrap().restored();
        tab.app.restore_remote_input(restored);
        assert_eq!(tab.app.input_box.draft_text(), DRAFT);
        assert_eq!(tab.app.queue.text_messages(), [QUEUED]);
    }

    #[test]
    fn a_new_tab_restores_nothing() {
        let _runtime = crate::test_runtime().enter();
        let mut first = Tab::new(false);
        first.type_draft(DRAFT);
        first.app.checkpoint_now();

        let tab = first.reopen(false);
        assert!(
            tab.app
                .remote_drafts
                .as_ref()
                .unwrap()
                .restored()
                .is_empty()
        );
    }
}
