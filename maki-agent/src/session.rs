//! The one vocabulary every driver uses to talk about a persisted session.

mod queue;

pub use queue::QueueError;

use crate::permissions::{PermissionManager, stored_to_rules};
use crate::{AgentEvent, Envelope};
use maki_config::Effect;
use maki_types::inbox::SubagentInbox;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use arc_swap::ArcSwap;
use maki_providers::{ContextGauge, Message, Model, TokenUsage, settle_session};
use maki_storage::frame::StoredFrame;
use maki_storage::id::{MakiId, SessionRef};
use maki_storage::sessions::{
    QueueReceipt, QueueStatus, SAVE_FAILED, SessionClaim, SessionError, StoredEffect, StoredMode,
    StoredRule, StoredThinking, StoredTokenUsage,
};
use maki_storage::{StateDir, StorageError};
pub use maki_types::session::{
    SessionHeader, StoredSession, record_task_event, rewind_turns, truncate_to_turn,
};
use thiserror::Error;
use tracing::warn;

use crate::agent::CallCapture;
use crate::agent::{
    CapturedCall, History, HistorySnapshot, RunContextBuilder, SharedHeader, SharedMessages,
    publish_live_session,
};
use crate::types::EventSender;
use crate::types::Spend;
use crate::{AgentInput, AgentMode, AgentRunParams, RunLedger, RunTotals, SessionMailbox};

pub const FORK_TITLE_PREFIX: &str = "fork of ";
pub const NOTHING_TO_STORE: &str = "session has no messages, nothing to store";
/// Settling a loaded file's rows prices them at standard rates, as a load
/// does: the turns that wrote them were priced when they ran.
const RESTORED_FAST: bool = false;

/// A session's settings as the user last chose them, which a host can keep
/// newer than any run's [`AgentInput`]: a toggle can come while a run is out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSettings {
    pub thinking: Option<StoredThinking>,
    pub fast: bool,
    pub workflow: bool,
    /// `None` when the session never chose, so `--yolo` stays one-shot.
    pub yolo: Option<bool>,
}

impl SessionSettings {
    pub fn of(session: &StoredSession) -> Self {
        Self {
            thinking: session.meta.thinking,
            fast: session.meta.fast,
            workflow: session.meta.workflow,
            yolo: session.meta.yolo,
        }
    }

    pub fn shared(self) -> SharedSettings {
        Arc::new(ArcSwap::from_pointee(self))
    }

    fn write(&self, session: &mut StoredSession) {
        session.meta.thinking = self.thinking;
        session.meta.fast = self.fast;
        session.meta.workflow = self.workflow;
        session.meta.yolo = self.yolo;
    }
}

/// Shared between a host and its driver: the host writes it in the order it
/// takes changes, and every save of the driver reads the latest.
pub type SharedSettings = Arc<ArcSwap<SessionSettings>>;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("{NOTHING_TO_STORE}")]
    Empty,
    #[error(transparent)]
    Session(#[from] SessionError),
}

/// Writes a session nobody holds yet, under its own claim, which drops once
/// it is on disk.
pub fn store_new(session: StoredSession, dir: &StateDir) -> Result<MakiId, StoreError> {
    let claim = SessionClaim::acquire(session.id, dir)?;
    save_new(session, &claim, dir)
}

/// Every new session reaches the disk through here. One with no messages is
/// refused, since the picker and `--continue` would offer it as a dead entry.
/// The check runs on what is about to be written, so a history that repaired
/// down to nothing is caught too.
pub fn save_new(
    mut session: StoredSession,
    claim: &SessionClaim,
    dir: &StateDir,
) -> Result<MakiId, StoreError> {
    if session.messages().is_empty() {
        return Err(StoreError::Empty);
    }
    session.save(claim, dir)?;
    Ok(session.id)
}

/// A transcript a driver starts from. The id comes from the caller, because
/// every entry point resolves one and reports it before the run starts, and a
/// second answer minted here would not be the one the user was told.
pub struct Resumed {
    pub id: SessionRef,
    /// The transcript to run on. It is moved out of [`Self::session`] when
    /// there is one, so the two never hold two copies of it.
    pub history: Vec<Message>,
    /// The provider's last prompt count for `history`, so a resumed run budgets
    /// from a measurement instead of an estimate.
    pub context_size: u32,
    /// The prefix `history` was sent under, so the next request can extend it
    /// instead of starting the cache and the model's reasoning over.
    pub frame: Option<Arc<StoredFrame>>,
    /// The stored session `history` came out of, if the driver read one. The
    /// run writes back into it, so the title, spending and meta survive without
    /// a second parse of the file. `None` when nothing is stored under the id
    /// yet, or when the driver owns the session and persists it itself.
    pub session: Option<StoredSession>,
}

impl Resumed {
    /// A session nothing has been stored for yet. Not [`Default`], because
    /// minting an id is a decision, and reaching for it by accident writes a
    /// transcript under an id nobody reported.
    pub fn fresh() -> Self {
        Self::empty(SessionRef::generate())
    }

    /// A run under `id` with nothing read from disk for it.
    pub fn empty(id: SessionRef) -> Self {
        Self {
            id,
            history: Vec::new(),
            context_size: 0,
            frame: None,
            session: None,
        }
    }

    /// A run continuing a session the driver already loaded.
    pub fn stored(id: SessionRef, mut session: StoredSession) -> Self {
        Self {
            id,
            history: session.drain_messages(),
            context_size: session.meta.context_size,
            frame: session.frame().cloned(),
            session: Some(session),
        }
    }
}

/// Owns the claimed session, its transcript and its context gauge. Turns save
/// on drop; an explicit rewind commits a cut before changing the live history.
pub struct SessionTrack {
    history: History,
    gauge: ContextGauge,
    store: SessionControl,
    spend: Arc<RunLedger>,
    opened_usage: TokenUsage,
    reported: RunTotals,
}

impl SessionTrack {
    /// Publishes the claimed session without writing a file. The first turn
    /// with messages saves it, so a failed connection leaves no empty entry.
    pub fn open(resumed: Resumed, claim: SessionClaim, storage: StateDir, cwd: &str) -> Self {
        let mailbox = SessionMailbox::register(resumed.id.id());
        Self::with_background(resumed, claim, storage, mailbox, Arc::default(), cwd)
    }

    pub fn with_background(
        resumed: Resumed,
        claim: SessionClaim,
        storage: StateDir,
        mailbox: SessionMailbox,
        spend: Arc<RunLedger>,
        cwd: &str,
    ) -> Self {
        let mirror: SharedMessages = Arc::new(ArcSwap::from_pointee(HistorySnapshot::default()));
        let mut store = SessionStore::open(storage, claim, mailbox, resumed.session, cwd);
        store.restored = store.persisted.as_ref().map(|_| resumed.history.clone());
        publish_live_session(resumed.id.id(), &mirror, &store.header);
        store.history = Arc::clone(&mirror);
        let history = History::restored(resumed.history)
            .with_frame(resumed.frame)
            .capturing_calls()
            .with_mirror(mirror);
        store.calls = history.call_capture();
        Self {
            opened_usage: store.session.token_usage,
            spend,
            reported: RunTotals::default(),
            store: SessionControl(Arc::new(Mutex::new(store))),
            history,
            gauge: ContextGauge::restored(resumed.context_size),
        }
    }

    /// Every save then writes the host's latest settings, so no order of a
    /// prompt and a toggle can store an older value.
    pub fn with_settings(self, settings: SharedSettings) -> Self {
        self.store.lock().settings = Some(settings);
        self
    }

    pub fn with_permissions(self, permissions: Arc<PermissionManager>) -> Self {
        let mut store = self.store.lock();
        permissions.load_session_rules(stored_to_rules(&store.session.meta.session_rules));
        store.permissions = Some(permissions);
        drop(store);
        self
    }

    pub fn control(&self) -> SessionControl {
        self.store.clone()
    }

    pub fn save_settings(&mut self) {
        let mut store = self.store.lock();
        if let Some(settings) = store.settings.clone() {
            settings.load().write(&mut store.session);
        }
        store.publish_header(false);
        store.save_metadata();
    }

    /// One turn against this transcript. The returned guard is the only way to
    /// reach [`AgentRunParams`] and it persists on drop. An
    /// [`Agent`](crate::Agent) built from it borrows the guard, so borrowck
    /// puts the write after the run without the driver arranging it.
    ///
    /// The spec lands here rather than at the end because a driver picks its
    /// model before it builds the agent. Since this is the only path to a
    /// write, no stored session can carry a spec no turn ran on.
    ///
    /// The settings come from `input` for the same reason: they are what the
    /// turn really runs under, so readers and the saved file never report a
    /// mode the driver switched away from.
    pub fn turn(&mut self, model_spec: String, input: &AgentInput) -> SessionTurn<'_> {
        {
            let mut store = self.store.lock();
            store.session.meta.mode = Some(match input.mode {
                AgentMode::Build => StoredMode::Build,
                AgentMode::Plan(_) => StoredMode::Plan,
            });
            if let AgentMode::Plan(path) = &input.mode {
                store.session.meta.plan_path = Some(path.to_string_lossy().into_owned());
            }
            if store.settings.is_none() {
                store.session.meta.thinking = Some(input.thinking.into());
                store.session.meta.fast = input.fast;
                store.session.meta.workflow = input.workflow;
            }
        }
        self.begin(model_spec)
    }

    /// A turn no prompt started, such as a `/compact`: the session keeps the
    /// mode and the settings it had.
    pub fn compaction(&mut self, model_spec: String) -> SessionTurn<'_> {
        self.begin(model_spec)
    }

    pub fn rewind(
        &mut self,
        index: usize,
        prompt: &str,
        next_run: u64,
    ) -> Result<Vec<QueueReceipt>, SessionError> {
        let mut store = self.store.lock();
        let mut snapshot = store.snapshot();
        if !rewind_turns(snapshot.messages()).contains(&(index, prompt)) {
            return Err(StorageError::NotFound("rewind turn changed".into()).into());
        }
        snapshot.meta.context_size = truncate_to_turn(&mut snapshot, index, self.gauge.size());
        let surviving: HashSet<_> = snapshot
            .messages()
            .iter()
            .flat_map(Message::tool_uses)
            .map(|(id, _, _)| id.to_owned())
            .collect();
        let forgotten: Vec<_> = store
            .mailbox
            .unsettled()
            .into_iter()
            .filter(|id| !surviving.contains(id))
            .collect();
        snapshot
            .meta
            .pending_background
            .retain(|id| surviving.contains(id));
        let mut cancelled = Vec::new();
        for work in &mut snapshot.meta.accepted_work {
            if matches!(work.status, QueueStatus::Queued | QueueStatus::Running) {
                work.status = QueueStatus::Cancelled;
                work.input = None;
                cancelled.push(work.clone());
            }
        }
        if snapshot.messages().is_empty() {
            snapshot.meta.accepted_work.clear();
            if let Err(error) = StoredSession::delete(&store.claim, &store.dir)
                && !error.is_not_found()
            {
                return Err(error);
            }
        } else {
            snapshot.save(&store.claim, &store.dir)?;
        }
        for id in forgotten {
            store.mailbox.forget(&id);
        }
        self.history = History::restored(snapshot.messages().to_vec())
            .with_frame(snapshot.frame().cloned())
            .capturing_calls()
            .with_mirror(Arc::clone(&store.history));
        self.history.mark_saved();
        self.gauge = ContextGauge::restored(snapshot.meta.context_size);
        store.task_from_run = next_run;
        store.deleted_tasks.retain(|id| surviving.contains(id));
        store.task_inboxes.retain(|id, _| {
            snapshot
                .subagents()
                .iter()
                .any(|task| task.tool_use_id == *id)
        });
        store.calls = self.history.call_capture();
        store.persisted = Some(SessionSettings::of(&snapshot));
        store.session = snapshot;
        store.restored = None;
        store.publish_header(false);
        Ok(cancelled)
    }

    pub(crate) fn finish_queue(
        &mut self,
        status: QueueStatus,
        error: Option<String>,
    ) -> Result<Vec<QueueReceipt>, QueueError> {
        if self.history.has_unsaved()
            && self
                .store
                .lock()
                .session
                .meta
                .accepted_work
                .iter()
                .any(|work| work.status == QueueStatus::Running)
        {
            return Err(QueueError::Invalid("turn transcript could not be saved"));
        }
        self.store.finish_queue(status, error)
    }

    fn begin(&mut self, model_spec: String) -> SessionTurn<'_> {
        let ledger = RunLedger::carrying(&self.spend, self.spend.totals().since(&self.reported));
        {
            let mut store = self.store.lock();
            store.session.set_model(model_spec);
            if let Some(settings) = store.settings.clone() {
                settings.load().write(&mut store.session);
            }
            store.publish_header(true);
        }
        SessionTurn {
            track: self,
            ledger,
        }
    }

    fn settle(&mut self) {
        let mut store = self.store.lock();
        if let Some(settings) = store.settings.clone() {
            settings.load().write(&mut store.session);
        }
        let Spend { totals, by_model } = self.spend.spend();
        if !by_model.is_empty()
            && let Some(recorded) = store.unsettled.take()
        {
            settle_baseline(&mut store.session, &recorded);
            store.opened_rows = store.session.usage_by_model().clone();
        }
        let mut usage = self.opened_usage;
        usage += totals.usage;
        let billed = usage != store.session.token_usage;
        store.session.token_usage = usage;
        *store.session.usage_by_model_mut() = store.opened_rows.clone();
        for (model, row) in by_model {
            store.session.add_model_usage(&model, row);
        }
        let calls = self.history.take_calls();
        if self.history.has_unsaved() || !calls.is_empty() {
            match store.record_turn(
                calls,
                self.history.as_slice(),
                self.history.frame(),
                self.gauge.size(),
            ) {
                Ok(()) => {
                    self.history.mark_saved();
                    store.persisted = Some(SessionSettings::of(&store.session));
                    store.restored = None;
                }
                Err(error) => {
                    self.history.mark_unsaved();
                    warn!(%error, session_id = %store.session.id, "failed to persist session");
                    eprintln!("{SAVE_FAILED}{}: {error}", store.session.id);
                }
            }
        } else if billed {
            store.save_metadata();
        }
    }
}

/// A host can accept a change and close the session before the driver saw
/// it idle; cancelling the driver drops the track, which saves it then.
impl Drop for SessionTrack {
    fn drop(&mut self) {
        self.settle();
        let unsaved = self.store.lock().settings_unsaved();
        if unsaved {
            self.save_settings();
        }
    }
}

/// A turn in progress. See [`SessionTrack::turn`].
pub struct SessionTurn<'a> {
    track: &'a mut SessionTrack,
    ledger: Arc<RunLedger>,
}

impl SessionTurn<'_> {
    /// What the turn's agent bills to. The turn adds it to the session's
    /// usage when it ends, so the agent must be built with this one.
    pub fn ledger(&self) -> Arc<RunLedger> {
        Arc::clone(&self.ledger)
    }

    pub fn run_params(
        &mut self,
        event_tx: EventSender,
        context: RunContextBuilder,
    ) -> AgentRunParams<'_> {
        AgentRunParams {
            history: &mut self.track.history,
            gauge: &mut self.track.gauge,
            event_tx,
            context,
        }
    }
}

impl Drop for SessionTurn<'_> {
    /// A turn that changed nothing writes nothing. A rewrite would move
    /// `updated_at`, which reorders `--continue`, stamp a model no turn ran
    /// on, and commit the restore-time repair over the only copy of the
    /// transcript. A turn that only spent, such as a paid summary that came
    /// back empty, saves the bill under the transcript as it was.
    fn drop(&mut self) {
        self.track.reported += self.ledger.totals();
        self.track.settle();
        self.track.store.lock().publish_header(false);
    }
}

/// Prices what a loaded file held before exact rows join it, against the
/// model it recorded, or the one the turn ran on when that no longer
/// resolves, as an ACP load falls back. With neither, the counts stay as an
/// unpriced row, which a later load can still price.
fn settle_baseline(session: &mut StoredSession, recorded: &str) {
    let before = session.token_usage;
    match Model::from_spec(recorded).or_else(|_| Model::from_spec(&session.model)) {
        Ok(model) => {
            settle_session(&before, session.usage_by_model_mut(), &model, RESTORED_FAST);
        }
        Err(_) if session.usage_by_model().is_empty() && before != TokenUsage::default() => {
            session.add_model_usage(recorded, before.billed(None));
        }
        Err(_) => {}
    }
}

/// The claimed session store shared by its runner and host operations.
#[derive(Clone)]
pub struct SessionControl(Arc<Mutex<SessionStore>>);

impl SessionControl {
    fn lock(&self) -> MutexGuard<'_, SessionStore> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn background_tasks(&self) -> usize {
        self.lock().mailbox.unsettled().len()
    }

    pub fn snapshot(&self) -> StoredSession {
        self.lock().snapshot()
    }

    /// Holds off queue starts and enqueues while `edit` changes idle state.
    /// The callback must not call back into this session control.
    pub fn while_idle<T>(&self, edit: impl FnOnce() -> T) -> Option<T> {
        let store = self.lock();
        if store.header.load().working
            || store
                .session
                .meta
                .accepted_work
                .iter()
                .any(|work| matches!(work.status, QueueStatus::Queued | QueueStatus::Running))
        {
            return None;
        }
        Some(edit())
    }

    pub fn task_inbox(&self, id: &str) -> Result<Arc<SubagentInbox>, SessionError> {
        self.lock()
            .task_inboxes
            .get(id)
            .cloned()
            .ok_or_else(|| StorageError::NotFound(format!("task inbox: {id}")).into())
    }

    pub fn remove_task(&self, id: &str) -> Result<(), SessionError> {
        let mut store = self.lock();
        if !store
            .session
            .subagents()
            .iter()
            .any(|task| task.tool_use_id == id)
        {
            return Err(StorageError::NotFound(format!("unknown task: {id}")).into());
        }
        if store.task_inboxes.contains_key(id) {
            return Err(StorageError::NotFound(format!("task is still running: {id}")).into());
        }
        let before = store.session.clone();
        store.session.remove_subagent(id, |message| {
            message
                .tool_uses()
                .map(|(id, _, _)| id.to_owned())
                .collect()
        });
        store.commit(before)?;
        store.deleted_tasks.insert(id.to_owned());
        Ok(())
    }

    pub fn record_task(&self, envelope: &Envelope, plugin: Option<Arc<str>>) -> bool {
        let mut store = self.lock();
        let id = match &envelope.event {
            AgentEvent::SubagentHistory { tool_use_id, .. } => Some(tool_use_id.as_str()),
            AgentEvent::TaskEnded { id, .. } => Some(id.as_str()),
            AgentEvent::ToolParts { id, .. } | AgentEvent::ToolMovable { id, .. }
                if envelope.subagent.is_none() =>
            {
                Some(id.as_str())
            }
            AgentEvent::ToolDone(done) if envelope.subagent.is_none() => Some(done.id.as_str()),
            AgentEvent::ToolProgress(progress) if envelope.subagent.is_none() => {
                Some(progress.id.as_str())
            }
            _ => envelope
                .subagent
                .as_ref()
                .map(|info| info.parent_tool_use_id.as_str()),
        };
        let parent_call = envelope
            .subagent
            .as_ref()
            .and_then(|info| info.parent_call.as_deref())
            .or(id);
        let receipt_bound = plugin.is_none()
            || envelope
                .subagent
                .as_ref()
                .is_some_and(|info| info.parent_call.is_some())
            || matches!(
                envelope.event,
                AgentEvent::ToolDone(_)
                    | AgentEvent::ToolProgress(_)
                    | AgentEvent::ToolParts { .. }
                    | AgentEvent::ToolMovable { .. }
            ) && envelope.subagent.is_none()
            || id.is_some_and(|id| {
                store
                    .session
                    .subagents()
                    .iter()
                    .any(|task| task.tool_use_id == id && task.parent_call.is_some())
            });
        if receipt_bound
            && envelope.run_id < store.task_from_run
            && !parent_call.is_some_and(|id| {
                store
                    .history
                    .load()
                    .messages
                    .iter()
                    .flat_map(Message::tool_uses)
                    .any(|(known, _, _)| known == id)
            })
        {
            return false;
        }
        if id.is_some_and(|id| store.deleted_tasks.contains(id)) {
            return false;
        }
        if let Some(info) = &envelope.subagent
            && let Some(inbox) = &info.inbox
        {
            store
                .task_inboxes
                .insert(info.parent_tool_use_id.clone(), Arc::clone(inbox));
        }
        if plugin.is_none()
            && let AgentEvent::SubagentHistory { tool_use_id, .. } = &envelope.event
        {
            store.task_inboxes.remove(tool_use_id);
        }
        if let AgentEvent::StreamClosed = &envelope.event
            && let Some(id) = id
        {
            store.task_inboxes.remove(id);
        }
        if record_task_event(&mut store.session, envelope, plugin) {
            store.save_metadata();
        }
        true
    }

    pub fn set_cwd(&self, cwd: String) -> Result<(), SessionError> {
        let mut store = self.lock();
        let before = store.session.clone();
        store.session.set_cwd(cwd);
        store.commit(before)
    }

    pub fn set_title(&self, title: String) -> Result<(), SessionError> {
        let mut store = self.lock();
        let before = store.session.clone();
        store.session.set_title(title);
        store.commit(before)
    }
}

struct SessionStore {
    dir: StateDir,
    claim: SessionClaim,
    mailbox: SessionMailbox,
    opened_rows: HashMap<String, StoredTokenUsage>,
    session: StoredSession,
    /// `session`'s header as readers see it, republished whenever it moves.
    header: SharedHeader,
    history: SharedMessages,
    calls: Option<CallCapture>,
    queue_ready: (flume::Sender<()>, flume::Receiver<()>),
    settings: Option<SharedSettings>,
    permissions: Option<Arc<PermissionManager>>,
    persisted: Option<SessionSettings>,
    restored: Option<Vec<Message>>,
    unsettled: Option<String>,
    task_from_run: u64,
    task_inboxes: HashMap<String, Arc<SubagentInbox>>,
    deleted_tasks: HashSet<String>,
}

impl SessionStore {
    fn snapshot(&self) -> StoredSession {
        let mut session = self.session.clone();
        let history = self.history.load_full();
        session.replace_messages(history.messages.as_ref().clone());
        session.set_frame(history.frame.clone());
        if let Some(settings) = &self.settings {
            settings.load().write(&mut session);
        }
        if let Some(rules) = self.permission_rules() {
            session.meta.session_rules = rules;
        }
        if let Some(calls) = &self.calls {
            for (id, output, data) in calls.snapshot() {
                session.insert_tool_output(id, output, data);
            }
        }
        session
    }

    /// Opening touches no file: the driver either read the session already or
    /// there is nothing under the id to read. A session nothing was written for
    /// yet is held in memory until [`Self::record_turn`] has something to
    /// store, so every file on disk has a transcript in it. The blank model
    /// spec is [`SessionTrack::turn`]'s to fill, and it runs before any write.
    fn open(
        dir: StateDir,
        claim: SessionClaim,
        mailbox: SessionMailbox,
        stored: Option<StoredSession>,
        cwd: &str,
    ) -> Self {
        let persisted = stored.as_ref().map(SessionSettings::of);
        let unsettled = stored.as_ref().map(|session| session.model.clone());
        let session = stored.unwrap_or_else(|| {
            let mut session = StoredSession::new("", cwd);
            session.id = claim.id();
            session
        });
        Self {
            dir,
            claim,
            mailbox,
            opened_rows: session.usage_by_model().clone(),
            header: Arc::new(ArcSwap::from_pointee(SessionHeader::of(&session))),
            session,
            history: Arc::default(),
            calls: None,
            queue_ready: flume::bounded(1),
            settings: None,
            permissions: None,
            persisted,
            restored: None,
            unsettled,
            task_from_run: 0,
            task_inboxes: HashMap::new(),
            deleted_tasks: HashSet::new(),
        }
    }

    fn commit(&mut self, before: StoredSession) -> Result<(), SessionError> {
        if let Some(settings) = &self.settings {
            settings.load().write(&mut self.session);
        }
        if let Err(error) = self.persist_metadata() {
            self.session = before;
            return Err(error);
        }
        self.publish_header(self.header.load().working);
        Ok(())
    }

    fn settings_unsaved(&self) -> bool {
        self.settings
            .as_ref()
            .is_some_and(|settings| Some(&**settings.load()) != self.persisted.as_ref())
            || self
                .permission_rules()
                .is_some_and(|rules| rules != self.session.meta.session_rules)
    }

    fn permission_rules(&self) -> Option<Vec<StoredRule>> {
        self.permissions.as_ref().map(|permissions| {
            permissions
                .session_rules_snapshot()
                .into_iter()
                .map(|rule| StoredRule {
                    tool: rule.tool.to_string(),
                    scope: rule.scope,
                    effect: match rule.effect {
                        Effect::Allow => StoredEffect::Allow,
                        Effect::Deny => StoredEffect::Deny,
                    },
                })
                .collect()
        })
    }

    fn sync_permissions(&mut self) {
        if let Some(rules) = self.permission_rules() {
            self.session.meta.session_rules = rules;
        }
    }

    fn persist_metadata(&mut self) -> Result<(), SessionError> {
        self.sync_permissions();
        self.session.meta.pending_background = self.mailbox.unsettled();
        let messages = self
            .restored
            .clone()
            .unwrap_or_else(|| self.history.load().messages.as_ref().clone());
        if messages.is_empty() && self.session.meta.accepted_work.is_empty() {
            return Ok(());
        }
        self.session.replace_messages(messages);
        if self.restored.is_none() {
            self.session.set_frame(self.history.load().frame.clone());
            if let Some(calls) = &self.calls {
                for (id, output, data) in calls.snapshot() {
                    self.session.insert_tool_output(id, output, data);
                }
            }
        }
        self.session.save(&self.claim, &self.dir)?;
        self.persisted = Some(SessionSettings::of(&self.session));
        Ok(())
    }

    fn save_metadata(&mut self) {
        if let Err(error) = self.persist_metadata() {
            warn!(%error, session_id = %self.session.id, "failed to save session meta");
        }
    }

    fn publish_header(&self, working: bool) {
        self.header.store(Arc::new(SessionHeader {
            working,
            ..SessionHeader::of(&self.session)
        }));
    }

    /// `context_size` travels with the messages, since a resumed session seeds
    /// its gauge from it. Stored without one, the next process is back to
    /// estimating a transcript this one had measured.
    ///
    /// The turn's calls join the session first, so a failed write keeps them
    /// for the next one.
    ///
    /// An empty transcript is not a session, the same rule [`save_new`]
    /// holds a new one to. An empty log is one the picker offers and `--continue`
    /// resolves to, so a run killed before its first turn would leave a dead
    /// entry behind for good. The same guard stops a history that sanitized
    /// down to nothing from replacing the copy it was restored from.
    fn record_turn(
        &mut self,
        calls: Vec<CapturedCall>,
        messages: &[Message],
        frame: Option<&Arc<StoredFrame>>,
        context_size: u32,
    ) -> Result<(), SessionError> {
        self.sync_permissions();
        for (id, output, data) in calls {
            self.session.insert_tool_output(id, output, data);
        }
        if messages.is_empty() {
            return Ok(());
        }
        self.session.replace_messages(messages.to_vec());
        self.session.set_frame(frame.cloned());
        self.session.meta.context_size = context_size;
        self.session.meta.pending_background = self.mailbox.unsettled();
        self.session.update_title_if_default();
        self.session.save(&self.claim, &self.dir)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use maki_providers::{ContentBlock, Role};
    use maki_storage::frame::PromptFacts;
    use maki_storage::id::MakiId;
    use maki_storage::sessions::{SESSIONS_DIR, generate_title};
    use maki_types::event::{SubagentInfo, ToolDoneEvent};
    use maki_types::input::{Compaction, SessionInput};
    use maki_types::tool::{DEFAULT_DATA_VERSION, ToolData, ToolOutput};
    use tempfile::TempDir;
    use test_case::test_case;

    use crate::tools::RequestTools;
    use crate::{InputSource, RunContext, SessionDefaults, TurnCompleteEvent};

    use super::*;
    use crate::agent::{live_header, live_history};

    const SESSION_ID: &str = "01965087-4c71-7f00-8000-000000000000";
    const CWD: &str = "/project";
    const MODEL_SPEC: &str = "anthropic/claude-test";
    const OTHER_SPEC: &str = "other/model";
    const CONTEXT_SIZE: u32 = 42_000;
    const PROMPT: &str = "fix the login bug";
    const OBSERVATION: &str = "build failed";
    const TITLE: &str = "a title the user set";
    const PLAN_PATH: &str = "/plans/plan.md";
    const NO_EMPTY_FILE: &str = "a session with no transcript must not be on disk";
    const NO_EMPTY_LATEST: &str = "an empty session must not be what --continue resolves to";
    const NOT_WIPED: &str = "an empty history must not replace the transcript it came from";
    const UNTOUCHED: &str = "a run that changed nothing must not rewrite the log";
    const RETRIED: &str = "a turn a failed write dropped has to reach disk on the next one";
    const REPAIRED: &str = "a turn that ran on the repaired transcript has to store it repaired";
    const ORPHAN_TOOL_ID: &str = "tool-nobody-called";
    const ORPHAN_RESULT: &str = "ok";
    const FRAME_SYSTEM: &str = "system the session was sent under";
    const FRAME_FINGERPRINT: &str = "0123456789abcdef";
    const FRAME_MCP_TOOL: &str = "srv__tool";
    const FRAME_KEPT: &str = "a resumed process must send the prefix the transcript was sent under";
    const THINKING: StoredThinking = StoredThinking::Adaptive;
    const PRICED_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
    const PRICED_ID: &str = "claude-sonnet-4-20250514";
    const OTHER_ID: &str = "other-model";
    const UNKNOWN_SPEC: &str = "no-such-provider/no-such-model";
    const ONE_MILLION: u32 = 1_000_000;
    const TURN_COST: f64 = 0.5;
    const SETTLED: &str = "the older spending must be priced before exact rows join it";
    const TURN_INPUT_TOKENS: u32 = 1_000;
    const TURNS: u32 = 2;
    const SETTINGS_TRACKED: &str = "readers and the file must report what the last turn ran under";
    const USAGE_SUMMED: &str =
        "every turn's bill must add up on the session, in memory and on disk";
    const CALL_ID: &str = "tool-1";
    const CALL_TOOL: &str = "bash";
    const CALL_OUTPUT: &str = "ok";
    const CALL_SUMMARY: &str = "ran the build";
    const CALL_SAVED: &str = "a headless turn must save each call's output and data";

    fn session_id() -> MakiId {
        SESSION_ID.parse().unwrap()
    }

    fn log_path(tmp: &TempDir) -> PathBuf {
        tmp.path()
            .join(SESSIONS_DIR)
            .join(format!("{}.jsonl", session_id()))
    }

    /// Stores a prompt followed by a tool result with no call in front of it,
    /// which is what a transcript cut off mid-turn leaves behind, and reopens
    /// it. [`History::restored`] drops the orphan, so the reopened run holds a
    /// repaired copy that differs from the file.
    fn reopen_with_orphan(tmp: &TempDir) -> SessionTrack {
        let orphan = Message {
            role: Role::User,
            content: vec![ContentBlock::tool_result(
                ORPHAN_TOOL_ID,
                ORPHAN_RESULT,
                false,
            )],
            ..Default::default()
        };
        push_turn(&mut track_on(tmp), MODEL_SPEC, |params| {
            params.history.push(Message::user(PROMPT.into()));
            params.history.push(orphan);
        });
        SessionTrack::open(
            Resumed::stored(SessionRef::from(session_id()), load(tmp)),
            claim(tmp),
            state_dir(tmp),
            CWD,
        )
    }

    fn state_dir(tmp: &TempDir) -> StateDir {
        StateDir::from_path(tmp.path().to_path_buf())
    }

    fn load(tmp: &TempDir) -> StoredSession {
        StoredSession::load(session_id(), &state_dir(tmp)).unwrap()
    }

    fn resumed() -> Resumed {
        Resumed::empty(SessionRef::from(session_id()))
    }

    fn claim(tmp: &TempDir) -> SessionClaim {
        SessionClaim::acquire(session_id(), &state_dir(tmp)).expect("nothing else holds it")
    }

    fn track_on(tmp: &TempDir) -> SessionTrack {
        SessionTrack::open(resumed(), claim(tmp), state_dir(tmp), CWD)
    }

    fn input(mode: AgentMode) -> AgentInput {
        let defaults = SessionDefaults {
            fast: true,
            workflow: false,
            thinking: Some(THINKING),
        };
        AgentInput::from_defaults(
            PROMPT.into(),
            mode,
            Vec::new(),
            defaults,
            InputSource::Headless,
        )
    }

    fn run_params<'a>(turn: &'a mut SessionTurn<'_>) -> AgentRunParams<'a> {
        turn.run_params(
            EventSender::new(flume::unbounded().0, 0),
            Arc::new(|_, _| RunContext::fixed(String::new(), RequestTools::default())),
        )
    }

    fn push_turn(track: &mut SessionTrack, spec: &str, edit: impl FnOnce(AgentRunParams<'_>)) {
        let mut turn = track.turn(spec.to_owned(), &input(AgentMode::Build));
        edit(run_params(&mut turn));
    }

    fn push_prompt(track: &mut SessionTrack, spec: &str, text: &str) {
        push_turn(track, spec, |params| {
            params.history.push(Message::user(text.into()))
        });
    }

    /// A run killed before its first turn has to leave nothing behind, neither
    /// a file under its id nor something for `--continue` to land on. `maki -p`
    /// is the reason: it is short, scripted and interrupted often, and the
    /// previous session in the directory has to stay the one `-c` finds.
    #[test_case(false ; "a run that never took a turn")]
    #[test_case(true ; "a turn that produced no messages")]
    fn a_run_with_no_transcript_leaves_nothing_behind(took_turn: bool) {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        if took_turn {
            push_turn(&mut track, MODEL_SPEC, |_| {});
        }
        drop(track);

        assert!(
            StoredSession::load(session_id(), &state_dir(&tmp)).is_err(),
            "{NO_EMPTY_FILE}"
        );
        assert!(
            StoredSession::claim_latest(CWD, &state_dir(&tmp))
                .expect("an empty store is readable")
                .is_none(),
            "{NO_EMPTY_LATEST}"
        );
    }

    /// The same guard from the other side: once a transcript is stored, a turn
    /// that ends with an empty history leaves it alone instead of emptying the
    /// only copy of it.
    #[test]
    fn an_empty_history_does_not_wipe_a_stored_transcript() {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        push_prompt(&mut track, MODEL_SPEC, PROMPT);
        push_turn(&mut track, MODEL_SPEC, |params| params.history.truncate(0));

        assert_eq!(load(&tmp).messages().len(), 1, "{NOT_WIPED}");
    }

    /// What a driver pushes through `run_params` is on disk once the turn ends,
    /// under the id, cwd, spec, title and measured size a resumed run reads
    /// back. Nothing here calls a save, ending the turn is the save.
    ///
    /// The observation is in there because it is the message kind a transcript
    /// can lose silently: it is not part of the model's reply, so nothing
    /// downstream complains when it fails to round trip.
    #[test]
    fn a_finished_turn_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        let messages = vec![
            Message::user(PROMPT.into()),
            Message::observation(OBSERVATION.into()),
        ];
        push_turn(&mut track, MODEL_SPEC, |params| {
            for message in &messages {
                params.history.push(message.clone());
            }
            *params.gauge = ContextGauge::restored(CONTEXT_SIZE);
        });

        let loaded = load(&tmp);
        assert_eq!(loaded.id, session_id());
        assert_eq!(loaded.cwd, CWD);
        assert_eq!(loaded.model, MODEL_SPEC);
        assert_eq!(loaded.title, generate_title(&messages));
        assert_eq!(loaded.messages().len(), 2);
        assert!(loaded.messages()[1].is_observation());
        assert_eq!(
            loaded.meta.context_size, CONTEXT_SIZE,
            "a resumed session seeds its gauge from this, so it has to be stored"
        );
    }

    /// `updated_at` counts whole seconds, so a rewrite in the same second as
    /// the seed could leave the bytes as they were. The orphan closes that
    /// gap: the reopened history has already dropped it, so any rewrite at
    /// all changes the file.
    #[test_case(0 ; "a run that took no turn")]
    #[test_case(1 ; "a turn that changed nothing")]
    fn a_run_that_changed_nothing_leaves_the_file_alone(turns: usize) {
        let tmp = TempDir::new().unwrap();
        let mut track = reopen_with_orphan(&tmp);
        let before = std::fs::read(log_path(&tmp)).unwrap();

        for _ in 0..turns {
            push_turn(&mut track, OTHER_SPEC, |_| {});
        }
        drop(track);

        assert_eq!(
            std::fs::read(log_path(&tmp)).unwrap(),
            before,
            "{UNTOUCHED}"
        );
    }

    #[test]
    fn a_turn_that_changed_something_writes_the_repair_with_it() {
        let tmp = TempDir::new().unwrap();
        push_prompt(&mut reopen_with_orphan(&tmp), OTHER_SPEC, "second");

        let has_tool_result = load(&tmp)
            .messages()
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
        assert!(!has_tool_result, "{REPAIRED}");
    }

    /// The second turn changes nothing, so only the flag the failed write
    /// left set can make it save.
    #[test]
    fn a_failed_save_is_retried_by_the_next_turn() {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        // A directory where the rewrite puts its temp file: `File::create`
        // cannot replace it, so the save fails before anything lands.
        let blocker = log_path(&tmp).with_extension("jsonl.tmp");
        std::fs::create_dir(&blocker).unwrap();
        push_prompt(&mut track, MODEL_SPEC, PROMPT);
        assert!(
            StoredSession::load(session_id(), &state_dir(&tmp)).is_err(),
            "the blocked write cannot have landed"
        );

        std::fs::remove_dir(&blocker).unwrap();
        push_turn(&mut track, MODEL_SPEC, |_| {});
        drop(track);

        assert_eq!(load(&tmp).messages().len(), 1, "{RETRIED}");
    }

    /// The next process continues the transcript instead of starting one
    /// beside it, which is the whole point of `-c` and `-r`. The title and plan
    /// are set only in memory, so they reach disk only if the run writes back
    /// into the session it was handed rather than reading the file again.
    #[test]
    fn reopening_resumes_the_stored_session() {
        let tmp = TempDir::new().unwrap();
        push_prompt(&mut track_on(&tmp), MODEL_SPEC, PROMPT);
        let mut stored = load(&tmp);
        stored.set_title(TITLE.to_owned());
        stored.meta.plan_path = Some(PLAN_PATH.to_owned());

        let mut track = SessionTrack::open(
            Resumed::stored(SessionRef::from(session_id()), stored),
            claim(&tmp),
            state_dir(&tmp),
            CWD,
        );
        push_prompt(&mut track, OTHER_SPEC, PROMPT);

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
        assert_eq!(loaded.model, OTHER_SPEC);
        assert_eq!(loaded.title, TITLE);
        assert_eq!(loaded.meta.plan_path.as_deref(), Some(PLAN_PATH));
    }

    /// A turn that never touches the frame still saves the transcript, and
    /// must save the frame next to it. Otherwise every resume starts a new
    /// prefix and loses the cache and the thinking.
    #[test]
    fn the_frame_survives_a_resume_and_the_next_turn() {
        let tmp = TempDir::new().unwrap();
        let frame = Arc::new(StoredFrame::new(
            FRAME_SYSTEM.into(),
            FRAME_FINGERPRINT.into(),
            Some(PromptFacts {
                cwd: CWD.into(),
                ..Default::default()
            }),
            vec![serde_json::json!({ "name": FRAME_MCP_TOOL })],
        ));
        let resumed = Resumed {
            history: vec![Message::user(PROMPT.into())],
            frame: Some(Arc::clone(&frame)),
            ..resumed()
        };
        let mut track = SessionTrack::open(resumed, claim(&tmp), state_dir(&tmp), CWD);
        push_prompt(&mut track, MODEL_SPEC, OBSERVATION);
        drop(track);

        let stored = load(&tmp);
        assert_eq!(stored.frame(), Some(&frame), "{FRAME_KEPT}");
        let reopened = SessionTrack::open(
            Resumed::stored(SessionRef::from(session_id()), stored),
            claim(&tmp),
            state_dir(&tmp),
            CWD,
        );
        assert_eq!(reopened.history.frame(), Some(&frame), "{FRAME_KEPT}");
    }

    /// `maki.session.messages` under a headless driver reads what the track
    /// publishes, so the restored transcript and every turn after it have to
    /// be there, and gone once the run ends. The id is minted so no other test
    /// holding the shared one can publish over it.
    #[test]
    fn open_publishes_the_live_transcript_until_dropped() {
        let tmp = TempDir::new().unwrap();
        let id = MakiId::generate();
        let resumed = Resumed {
            history: vec![Message::user(PROMPT.into())],
            ..Resumed::empty(SessionRef::from(id))
        };
        let claim = SessionClaim::acquire(id, &state_dir(&tmp)).expect("nothing else holds it");
        let mut track = SessionTrack::open(resumed, claim, state_dir(&tmp), CWD);

        let restored = live_history(id).expect("an open track is live");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].user_text(), Some(PROMPT));

        push_prompt(&mut track, MODEL_SPEC, OBSERVATION);
        let pushed = live_history(id).expect("an open track is live");
        assert_eq!(pushed.len(), 2);
        assert_eq!(pushed[1].user_text(), Some(OBSERVATION));

        drop(track);
        assert!(live_history(id).is_none());
    }

    /// Export and `read` of a headless session answer from this before the
    /// first save, so it has to follow the turn: the model it runs on, the
    /// title it earned, and whether one is running.
    #[test]
    fn open_publishes_the_header_and_each_turn_moves_it() {
        let tmp = TempDir::new().unwrap();
        let id = MakiId::generate();
        let claim = SessionClaim::acquire(id, &state_dir(&tmp)).expect("nothing else holds it");
        let mut track = SessionTrack::open(
            Resumed::empty(SessionRef::from(id)),
            claim,
            state_dir(&tmp),
            CWD,
        );

        let opened = live_header(id).expect("an open track publishes its header");
        assert_eq!(opened.cwd, CWD);
        assert!(!opened.working);

        let turn = track.turn(MODEL_SPEC.into(), &input(AgentMode::Build));
        let running = live_header(id).unwrap();
        assert_eq!(running.model, MODEL_SPEC);
        assert!(running.working);
        drop(turn);

        push_prompt(&mut track, MODEL_SPEC, PROMPT);
        let finished = live_header(id).unwrap();
        assert!(!finished.working);
        assert_eq!(
            finished.title,
            generate_title(&[Message::user(PROMPT.into())])
        );

        drop(track);
        assert!(live_header(id).is_none());
    }

    fn billed(model: &str, usage: TokenUsage, cost: Option<f64>) -> TurnCompleteEvent {
        TurnCompleteEvent {
            message: Message::default(),
            usage,
            model: model.into(),
            cost,
            subsidised_list_cost: None,
            subsidised_by: None,
            context_size: None,
            context_window: 0,
        }
    }

    #[test]
    fn moved_work_is_reported_once_and_saved_after_the_last_turn() {
        let tmp = TempDir::new().unwrap();
        let id = MakiId::generate();
        let dir = state_dir(&tmp);
        let claim = SessionClaim::acquire(id, &dir).unwrap();
        let mailbox = SessionMailbox::register(id);
        let mut track = SessionTrack::with_background(
            Resumed::empty(SessionRef::from(id)),
            claim,
            dir.clone(),
            mailbox,
            Arc::default(),
            CWD,
        );
        let moved = {
            let mut turn = track.turn(MODEL_SPEC.into(), &input(AgentMode::Build));
            run_params(&mut turn)
                .history
                .push(Message::user(PROMPT.into()));
            RunLedger::child(&turn.ledger())
        };
        moved.bill(&billed(OTHER_ID, million_input(), Some(TURN_COST)), None);
        assert_eq!(
            track
                .turn(MODEL_SPEC.into(), &input(AgentMode::Build))
                .ledger()
                .totals()
                .cost,
            Some(TURN_COST)
        );
        assert_eq!(
            track
                .turn(MODEL_SPEC.into(), &input(AgentMode::Build))
                .ledger()
                .totals()
                .cost,
            None
        );
        moved.bill(&billed(OTHER_ID, million_input(), Some(TURN_COST)), None);
        drop(track);
        let saved = StoredSession::load(id, &dir).unwrap();
        assert_eq!(saved.token_usage.input, ONE_MILLION * 2);
        assert_eq!(saved.usage_by_model()[OTHER_ID].cost, Some(TURN_COST * 2.0));
    }

    #[test]
    fn receipts_remain_pending_until_the_model_reads_their_results() {
        let tmp = TempDir::new().unwrap();
        let id = MakiId::generate();
        let dir = state_dir(&tmp);
        let claim = SessionClaim::acquire(id, &dir).unwrap();
        let mailbox = SessionMailbox::register(id);
        let mut track = SessionTrack::with_background(
            Resumed::empty(SessionRef::from(id)),
            claim,
            dir.clone(),
            mailbox.clone(),
            Arc::default(),
            CWD,
        );
        mailbox.track(CALL_ID);
        push_prompt(&mut track, MODEL_SPEC, PROMPT);
        assert_eq!(
            StoredSession::load(id, &dir)
                .unwrap()
                .meta
                .pending_background,
            [CALL_ID]
        );
        mailbox.settle(
            CALL_ID,
            Some(crate::BackgroundNotice {
                text: CALL_OUTPUT.into(),
                display: None,
                wake: true,
                end: crate::TaskEnd::Finished,
            }),
        );
        push_prompt(&mut track, MODEL_SPEC, PROMPT);
        assert_eq!(
            StoredSession::load(id, &dir)
                .unwrap()
                .meta
                .pending_background,
            [CALL_ID]
        );
        push_turn(&mut track, MODEL_SPEC, |params| {
            for notice in mailbox.drain() {
                params.history.push(notice);
            }
        });
        assert!(
            StoredSession::load(id, &dir)
                .unwrap()
                .meta
                .pending_background
                .is_empty()
        );
    }

    #[test]
    fn a_rewind_keeps_the_events_and_inbox_of_a_surviving_task() {
        let tmp = TempDir::new().unwrap();
        let id = MakiId::generate();
        let dir = state_dir(&tmp);
        let claim = SessionClaim::acquire(id, &dir).unwrap();
        let mut track = SessionTrack::open(Resumed::empty(SessionRef::from(id)), claim, dir, CWD);
        push_prompt(&mut track, MODEL_SPEC, PROMPT);
        push_turn(&mut track, MODEL_SPEC, |params| {
            params.history.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    CALL_ID,
                    CALL_TOOL,
                    serde_json::json!({}),
                )],
                ..Default::default()
            });
            params.history.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: CALL_ID.into(),
                    content: CALL_OUTPUT.into(),
                    is_error: false,
                    loaded_tools: Vec::new(),
                }],
                ..Default::default()
            });
        });
        let event = Envelope {
            event: AgentEvent::Nudge,
            run_id: 0,
            subagent: Some(SubagentInfo {
                parent_tool_use_id: CALL_ID.into(),
                name: CALL_TOOL.into(),
                prompt: None,
                model: None,
                opts: None,
                answer_tx: None,
                inbox: Some(Arc::default()),
                parent_call: None,
            }),
        };
        let control = track.control();
        assert!(control.record_task(&event, None));
        push_prompt(&mut track, MODEL_SPEC, PROMPT);
        track.rewind(3, PROMPT, 1).unwrap();
        assert!(control.record_task(&event, None));
        assert!(control.task_inbox(CALL_ID).is_ok());
    }

    fn million_input() -> TokenUsage {
        TokenUsage {
            input: ONE_MILLION,
            ..Default::default()
        }
    }

    /// A stored session on a priced model, whose file holds `rows`.
    fn priced_file(tmp: &TempDir, rows: &[(&str, Option<f64>)]) -> SessionTrack {
        let mut stored = StoredSession::new(PRICED_SPEC, CWD);
        stored.id = session_id();
        stored.push_message(Message::user(PROMPT.into()));
        stored.token_usage = million_input();
        for (model, cost) in rows {
            stored.add_model_usage(model, million_input().billed(*cost));
        }
        stored.save(&claim(tmp), &state_dir(tmp)).unwrap();
        SessionTrack::open(
            Resumed::stored(SessionRef::from(session_id()), load(tmp)),
            claim(tmp),
            state_dir(tmp),
            CWD,
        )
    }

    /// The rows are what a resume prices, so a headless turn writes its own,
    /// and an older file's spending is settled before they join it.
    #[test_case(&[] ; "counts_with_no_rows")]
    #[test_case(&[(PRICED_ID, None)] ; "a_row_with_no_cost")]
    fn a_headless_turn_saves_its_rows_and_keeps_the_older_spending(rows: &[(&str, Option<f64>)]) {
        let tmp = TempDir::new().unwrap();
        let recorded = Model::from_spec(PRICED_SPEC).unwrap();
        let estimate = recorded.list_cost(&million_input(), false);
        let mut track = priced_file(&tmp, rows);

        push_turn(&mut track, PRICED_SPEC, |params| {
            params.history.push(Message::user(PROMPT.into()));
        });
        track
            .turn(PRICED_SPEC.into(), &input(AgentMode::Build))
            .ledger()
            .bill(&billed(OTHER_ID, million_input(), Some(TURN_COST)), None);
        drop(track);

        let saved = load(&tmp);
        let saved_rows = saved.usage_by_model();
        assert_eq!(saved_rows[PRICED_ID].cost, estimate, "{SETTLED}");
        assert_eq!(saved_rows[OTHER_ID].cost, Some(TURN_COST));
        assert_eq!(saved_rows[OTHER_ID].input, ONE_MILLION);
    }

    /// A file whose recorded model no longer resolves is priced against the
    /// model the turn ran on, as an ACP load would price it.
    #[test]
    fn an_unresolvable_recorded_model_falls_back_to_the_turns() {
        let tmp = TempDir::new().unwrap();
        let mut stored = StoredSession::new(UNKNOWN_SPEC, CWD);
        stored.id = session_id();
        stored.push_message(Message::user(PROMPT.into()));
        stored.token_usage = million_input();
        stored.save(&claim(&tmp), &state_dir(&tmp)).unwrap();
        let mut track = SessionTrack::open(
            Resumed::stored(SessionRef::from(session_id()), load(&tmp)),
            claim(&tmp),
            state_dir(&tmp),
            CWD,
        );

        track
            .turn(PRICED_SPEC.into(), &input(AgentMode::Build))
            .ledger()
            .bill(&billed(OTHER_ID, million_input(), Some(TURN_COST)), None);
        drop(track);

        let fallback = Model::from_spec(PRICED_SPEC).unwrap();
        let saved = load(&tmp);
        assert_eq!(
            saved.usage_by_model()[PRICED_ID].cost,
            fallback.list_cost(&million_input(), false),
            "{SETTLED}"
        );
    }

    /// A paid summary that came back empty leaves the transcript as it was,
    /// and its bill still reaches the file under the transcript as loaded.
    #[test]
    fn a_turn_that_only_spent_saves_its_bill() {
        let tmp = TempDir::new().unwrap();
        let mut track = reopen_with_orphan(&tmp);
        let usage = TokenUsage {
            input: TURN_INPUT_TOKENS,
            ..Default::default()
        };

        track
            .compaction(MODEL_SPEC.into())
            .ledger()
            .bill(&billed(MODEL_SPEC, usage, None), None);
        drop(track);

        let saved = load(&tmp);
        assert_eq!(saved.token_usage.input, TURN_INPUT_TOKENS, "{USAGE_SUMMED}");
        assert_eq!(saved.messages().len(), 2, "{NOT_WIPED}");
    }

    /// A headless driver keeps no other record of its settings or its spend,
    /// so the turn has to put them on the session, where `read`, `export` and
    /// the next resume look.
    #[test]
    fn each_turn_records_its_settings_and_adds_its_usage() {
        let tmp = TempDir::new().unwrap();
        let id = MakiId::generate();
        let claim = SessionClaim::acquire(id, &state_dir(&tmp)).expect("nothing else holds it");
        let mut track = SessionTrack::open(
            Resumed::empty(SessionRef::from(id)),
            claim,
            state_dir(&tmp),
            CWD,
        );
        let usage = TokenUsage {
            input: TURN_INPUT_TOKENS,
            ..Default::default()
        };
        for _ in 0..TURNS {
            let mut turn = track.turn(MODEL_SPEC.into(), &input(AgentMode::Plan(PLAN_PATH.into())));
            turn.ledger().bill(&billed(MODEL_SPEC, usage, None), None);
            run_params(&mut turn)
                .history
                .push(Message::user(PROMPT.into()));
        }

        let header = live_header(id).unwrap();
        let saved = StoredSession::load(id, &state_dir(&tmp)).unwrap();
        assert_eq!(saved.meta.plan_path.as_deref(), Some(PLAN_PATH));
        let stored = SessionHeader::of(&saved);
        for header in [&*header, &stored] {
            assert_eq!(header.mode, Some(StoredMode::Plan), "{SETTINGS_TRACKED}");
            assert_eq!(header.thinking, Some(THINKING), "{SETTINGS_TRACKED}");
            assert!(header.fast, "{SETTINGS_TRACKED}");
            assert_eq!(
                header.usage.input,
                TURN_INPUT_TOKENS * TURNS,
                "{USAGE_SUMMED}"
            );
        }
    }

    #[test]
    fn rewind_commits_history_and_queue_together_and_keeps_the_claim() {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        push_prompt(&mut track, MODEL_SPEC, PROMPT);
        push_prompt(&mut track, MODEL_SPEC, OBSERVATION);
        let control = track.control();
        control.enqueue(&queued_prompt()).unwrap();
        let blocker = log_path(&tmp).with_extension("jsonl.tmp");
        std::fs::create_dir(&blocker).unwrap();
        assert!(track.rewind(1, OBSERVATION, 1).is_err());
        assert_eq!(control.snapshot().messages(), load(&tmp).messages());
        assert_eq!(control.queue()[0].status, QueueStatus::Queued);
        std::fs::remove_dir(blocker).unwrap();
        assert!(track.rewind(1, PROMPT, 1).is_err());
        assert_eq!(
            track.rewind(1, OBSERVATION, 1).unwrap()[0].status,
            QueueStatus::Cancelled
        );
        assert_eq!(load(&tmp).messages().len(), 1);
        track.rewind(0, PROMPT, 1).unwrap();
        control.set_title(TITLE.into()).unwrap();
        assert!(!log_path(&tmp).exists());
        assert_eq!(control.snapshot().meta.context_size, 0);
        assert!(SessionClaim::acquire(session_id(), &state_dir(&tmp)).is_err());
        push_prompt(&mut track, MODEL_SPEC, OBSERVATION);
        assert_eq!(load(&tmp).messages(), &[Message::user(OBSERVATION.into())]);
    }

    #[test]
    fn task_removal_and_rewind_refuse_late_events() {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        push_prompt(&mut track, MODEL_SPEC, PROMPT);
        let control = track.control();
        let info = SubagentInfo {
            parent_tool_use_id: CALL_ID.into(),
            name: CALL_TOOL.into(),
            prompt: None,
            model: None,
            opts: None,
            answer_tx: None,
            inbox: Some(Arc::default()),
            parent_call: None,
        };
        let event = Envelope {
            event: AgentEvent::ToolDone(Box::new(finished_call())),
            subagent: Some(info.clone()),
            run_id: 0,
        };
        assert!(control.record_task(&event, None));
        assert!(control.remove_task(CALL_ID).is_err());
        control
            .task_inbox(CALL_ID)
            .unwrap()
            .push(input(AgentMode::Build));
        assert_eq!(info.inbox.unwrap().texts(), [PROMPT]);
        control.record_task(
            &Envelope {
                event: AgentEvent::SubagentHistory {
                    tool_use_id: CALL_ID.into(),
                    messages: vec![Message::user(PROMPT.into())],
                },
                subagent: None,
                run_id: 0,
            },
            None,
        );
        assert_eq!(load(&tmp).tool_data()[CALL_ID].summary, CALL_SUMMARY);
        control.remove_task(CALL_ID).unwrap();
        assert!(!control.record_task(&event, None));
        track.rewind(0, PROMPT, 1).unwrap();
        let late = Envelope {
            subagent: Some(SubagentInfo {
                parent_tool_use_id: ORPHAN_TOOL_ID.into(),
                ..event.subagent.unwrap()
            }),
            ..event
        };
        assert!(!control.record_task(&late, None));
        assert!(control.snapshot().subagents().is_empty());
        assert!(control.record_task(
            &Envelope {
                run_id: 1,
                subagent: Some(SubagentInfo {
                    parent_tool_use_id: CALL_ID.into(),
                    ..late.subagent.unwrap()
                }),
                ..late
            },
            None
        ));
    }

    #[test]
    fn host_title_and_snapshot_during_a_turn_keep_messages_and_tool_data() {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        let control = track.control();
        let mut turn = track.turn(MODEL_SPEC.into(), &input(AgentMode::Build));
        let params = run_params(&mut turn);
        params.history.push(Message::user(PROMPT.into()));
        params
            .history
            .call_capture()
            .unwrap()
            .record(&finished_call());

        control.set_title(TITLE.into()).unwrap();
        let snapshot = control.snapshot();
        assert_eq!(snapshot.title, TITLE);
        assert_eq!(snapshot.messages().len(), 1);
        assert_eq!(snapshot.tool_data()[CALL_ID].summary, CALL_SUMMARY);
        assert_eq!(load(&tmp).tool_data()[CALL_ID].summary, CALL_SUMMARY);
        params.history.push(Message::user(OBSERVATION.into()));
        drop(turn);

        let saved = load(&tmp);
        assert_eq!(saved.title, TITLE);
        assert_eq!(saved.messages().len(), 2);
        assert_eq!(saved.tool_data()[CALL_ID].summary, CALL_SUMMARY);
    }

    #[test]
    fn host_title_preserves_uncommitted_restore_repairs() {
        let tmp = TempDir::new().unwrap();
        let track = reopen_with_orphan(&tmp);
        let original = load(&tmp).messages().to_vec();
        track.control().set_title(TITLE.into()).unwrap();
        let saved = load(&tmp);
        assert_eq!(saved.title, TITLE);
        assert_eq!(saved.messages(), original);
    }

    #[test]
    fn a_failed_host_title_change_is_not_reported_as_saved() {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        push_prompt(&mut track, MODEL_SPEC, PROMPT);
        let control = track.control();
        let title = control.snapshot().title;
        let blocker = log_path(&tmp).with_extension("jsonl.tmp");
        std::fs::create_dir(&blocker).unwrap();
        assert!(control.set_title(TITLE.into()).is_err());
        assert_eq!(control.snapshot().title, title);
        assert_eq!(load(&tmp).title, title);
        std::fs::remove_dir(&blocker).unwrap();
        control.set_title(TITLE.into()).unwrap();
        assert_eq!(load(&tmp).title, TITLE);
    }

    fn queued_prompt() -> SessionInput {
        SessionInput::Prompt(input(AgentMode::Build))
    }

    #[test]
    fn accepted_work_survives_restart_without_replaying_a_started_item() {
        let tmp = TempDir::new().unwrap();
        let track = track_on(&tmp);
        let control = track.control();
        let running = control.enqueue(&queued_prompt()).unwrap();
        assert_eq!(
            control
                .enqueue_with_id(&queued_prompt(), running.id)
                .unwrap(),
            running
        );
        assert_eq!(control.queue().len(), 1);
        control.take_queued(None, None).unwrap().unwrap();
        let waiting = control.enqueue(&queued_prompt()).unwrap();
        drop(control);
        drop(track);
        let track = SessionTrack::open(
            Resumed::stored(SessionRef::from(session_id()), load(&tmp)),
            claim(&tmp),
            state_dir(&tmp),
            CWD,
        );
        let control = track.control();
        let recovered = control.recover_queue(SessionDefaults::default()).unwrap();
        assert_eq!(
            recovered
                .iter()
                .find(|work| work.id == running.id)
                .unwrap()
                .status,
            QueueStatus::Interrupted
        );
        assert!(load(&tmp).meta.accepted_work[0].input.is_some());
        let (_, receipts) = control.take_queued(None, None).unwrap().unwrap();
        assert_eq!(
            receipts
                .iter()
                .map(|receipt| receipt.id)
                .collect::<Vec<_>>(),
            [waiting.id]
        );
        control.finish_queue(QueueStatus::Done, None).unwrap();
        assert!(control.take_queued(None, None).unwrap().is_none());
        assert!(load(&tmp).meta.accepted_work[1].input.is_none());
    }

    #[test]
    fn a_failed_queue_write_neither_accepts_nor_runs_work() {
        let tmp = TempDir::new().unwrap();
        let track = track_on(&tmp);
        let control = track.control();
        let first = control.enqueue(&queued_prompt()).unwrap();
        let blocker = log_path(&tmp).with_extension("jsonl.tmp");
        std::fs::create_dir(&blocker).unwrap();
        assert!(control.enqueue(&queued_prompt()).is_err());
        assert!(control.take_queued(None, None).is_err());
        assert!(control.cancel_queued(first.id).is_err());
        assert_eq!(control.queue(), load(&tmp).meta.accepted_work);
        assert_eq!(control.queue().len(), 1);
        assert_eq!(control.queue()[0].status, QueueStatus::Queued);
        std::fs::remove_dir(&blocker).unwrap();
        control.cancel_queued(first.id).unwrap();
        assert!(control.take_queued(None, None).unwrap().is_none());
        assert_eq!(
            load(&tmp).meta.accepted_work[0].status,
            QueueStatus::Cancelled
        );
    }

    #[test]
    fn host_queue_batches_messages_and_keeps_mode_and_compaction_boundaries() {
        let tmp = TempDir::new().unwrap();
        let track = track_on(&tmp);
        let control = track.control();
        for _ in 0..2 {
            control.enqueue(&queued_prompt()).unwrap();
        }
        control
            .enqueue(&SessionInput::Prompt(input(AgentMode::Plan(
                PLAN_PATH.into(),
            ))))
            .unwrap();
        control
            .enqueue(&SessionInput::Compact(Compaction {
                instructions: None,
                workflow: false,
            }))
            .unwrap();
        let (trigger, cancel) = crate::CancelToken::new();
        trigger.cancel();
        assert!(
            control
                .take_queued(Some(&AgentMode::Build), Some(&cancel))
                .unwrap()
                .is_none()
        );
        let (run, receipts) = control.take_queued(None, None).unwrap().unwrap();
        assert_eq!(receipts.len(), 2);
        let SessionInput::Prompt(run) = run else {
            panic!("expected queued messages")
        };
        assert_eq!(run.earlier.len(), 1);
        control.finish_queue(QueueStatus::Done, None).unwrap();
        assert!(
            control
                .take_queued(Some(&AgentMode::Build), None)
                .unwrap()
                .is_none()
        );
        let (_, receipts) = control.take_queued(None, None).unwrap().unwrap();
        assert_eq!(receipts.len(), 1);
        control.finish_queue(QueueStatus::Done, None).unwrap();
        let (run, _) = control
            .take_queued(Some(&AgentMode::Build), None)
            .unwrap()
            .unwrap();
        assert!(matches!(run, SessionInput::Compact(_)));
    }

    #[test]
    fn legacy_pending_text_is_migrated_once_with_its_session_settings() {
        let tmp = TempDir::new().unwrap();
        push_prompt(&mut track_on(&tmp), MODEL_SPEC, PROMPT);
        let mut stored = load(&tmp);
        stored.meta.queued_messages.push(OBSERVATION.into());
        stored.save(&claim(&tmp), &state_dir(&tmp)).unwrap();
        let track = SessionTrack::open(
            Resumed::stored(SessionRef::from(session_id()), stored),
            claim(&tmp),
            state_dir(&tmp),
            CWD,
        );
        let control = track.control();
        let recovered = control.recover_queue(SessionDefaults::default()).unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(load(&tmp).meta.queued_messages.is_empty());
        assert_eq!(
            control.recover_queue(SessionDefaults::default()).unwrap()[0].id,
            recovered[0].id
        );
        let (run, _) = control.take_queued(None, None).unwrap().unwrap();
        let SessionInput::Prompt(run) = run else {
            panic!("expected queued message")
        };
        assert_eq!(run.message, OBSERVATION);
        assert_eq!(run.thinking, THINKING.into());
        assert!(run.fast);
    }

    #[test]
    fn a_failed_run_keeps_waiting_input_without_executing_it() {
        let tmp = TempDir::new().unwrap();
        let track = track_on(&tmp);
        let control = track.control();
        control.enqueue(&queued_prompt()).unwrap();
        control.take_queued(None, None).unwrap().unwrap();
        let waiting = control.enqueue(&queued_prompt()).unwrap();
        control
            .finish_queue(QueueStatus::Error, Some(OBSERVATION.into()))
            .unwrap();
        let work = control
            .queue()
            .into_iter()
            .find(|work| work.id == waiting.id)
            .unwrap();
        assert_eq!(work.status, QueueStatus::Interrupted);
        assert!(work.input.is_some());
        assert!(control.take_queued(None, None).unwrap().is_none());
    }

    #[test]
    fn a_failed_turn_save_cannot_publish_a_successful_queue_outcome() {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        let control = track.control();
        control.enqueue(&queued_prompt()).unwrap();
        control.take_queued(None, None).unwrap().unwrap();
        std::fs::create_dir(log_path(&tmp).with_extension("jsonl.tmp")).unwrap();
        push_prompt(&mut track, MODEL_SPEC, PROMPT);
        assert!(track.finish_queue(QueueStatus::Done, None).is_err());
        assert_eq!(
            load(&tmp).meta.accepted_work[0].status,
            QueueStatus::Running
        );
    }

    fn finished_call() -> ToolDoneEvent {
        let data = ToolData::new(
            Arc::from(CALL_TOOL),
            DEFAULT_DATA_VERSION,
            serde_json::json!({}),
            CALL_SUMMARY.into(),
        );
        ToolDoneEvent {
            id: CALL_ID.into(),
            tool: Arc::from(CALL_TOOL),
            output: Arc::new(ToolOutput::Plain(CALL_OUTPUT.into())),
            is_error: false,
            annotation: None,
            written_path: None,
            call: None,
            data: Some(Arc::new(data)),
            backgrounded: false,
        }
    }

    /// The headless and ACP drivers keep no other record of a call, so the
    /// turn that ran it saves it, and a resumed session keeps it through
    /// its next save.
    #[test]
    fn a_turn_saves_its_calls_with_their_data() {
        let tmp = TempDir::new().unwrap();
        push_turn(&mut track_on(&tmp), MODEL_SPEC, |params| {
            params.history.push(Message::user(PROMPT.into()));
            let capture = params
                .history
                .call_capture()
                .expect("a track captures calls");
            capture.record(&finished_call());
        });
        let saved = load(&tmp);
        assert!(saved.tool_outputs().contains_key(CALL_ID), "{CALL_SAVED}");
        assert_eq!(
            saved.tool_data()[CALL_ID].summary,
            CALL_SUMMARY,
            "{CALL_SAVED}"
        );

        let mut track = SessionTrack::open(
            Resumed::stored(SessionRef::from(session_id()), saved),
            claim(&tmp),
            state_dir(&tmp),
            CWD,
        );
        push_prompt(&mut track, MODEL_SPEC, OBSERVATION);
        drop(track);

        let resaved = load(&tmp);
        assert_eq!(resaved.messages().len(), 2);
        assert_eq!(
            resaved.tool_data()[CALL_ID].summary,
            CALL_SUMMARY,
            "{CALL_SAVED}"
        );
    }

    /// A call that lands after a turn saved is still saved by the next turn,
    /// even one that moves no message.
    #[test]
    fn a_call_captured_after_a_save_reaches_the_next_turn() {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        let mut retained = None;
        push_turn(&mut track, MODEL_SPEC, |params| {
            params.history.push(Message::user(PROMPT.into()));
            retained = params.history.call_capture();
        });
        assert!(!load(&tmp).tool_outputs().contains_key(CALL_ID));

        retained
            .expect("a track captures calls")
            .record(&finished_call());
        push_turn(&mut track, MODEL_SPEC, |_| {});

        assert!(
            load(&tmp).tool_outputs().contains_key(CALL_ID),
            "{CALL_SAVED}"
        );
    }

    /// A save that only captured calls started and that failed is retried
    /// by the next turn, though neither turn moves a message.
    #[test]
    fn a_failed_save_of_captured_calls_is_retried() {
        let tmp = TempDir::new().unwrap();
        let mut track = track_on(&tmp);
        let mut retained = None;
        push_turn(&mut track, MODEL_SPEC, |params| {
            params.history.push(Message::user(PROMPT.into()));
            retained = params.history.call_capture();
        });
        let blocker = log_path(&tmp).with_extension("jsonl.tmp");
        std::fs::create_dir(&blocker).unwrap();
        retained
            .expect("a track captures calls")
            .record(&finished_call());
        push_turn(&mut track, MODEL_SPEC, |_| {});
        assert!(!load(&tmp).tool_outputs().contains_key(CALL_ID));

        std::fs::remove_dir(&blocker).unwrap();
        push_turn(&mut track, MODEL_SPEC, |_| {});

        assert!(load(&tmp).tool_outputs().contains_key(CALL_ID), "{RETRIED}");
    }

    fn hosted_track(tmp: &TempDir, settings: &SharedSettings) -> SessionTrack {
        SessionTrack::open(resumed(), claim(tmp), state_dir(tmp), CWD)
            .with_settings(Arc::clone(settings))
    }

    fn host_settings(fast: bool) -> SessionSettings {
        SessionSettings {
            thinking: Some(StoredThinking::Off),
            fast,
            workflow: true,
            yolo: Some(true),
        }
    }

    /// A toggle that came while the run was out is newer than the prompt the
    /// run carried, so the turn's save must not put the older value back.
    #[test]
    fn a_hosts_newer_settings_win_over_the_turns_own() {
        let tmp = TempDir::new().unwrap();
        let settings: SharedSettings = Arc::new(ArcSwap::from_pointee(host_settings(false)));
        let mut track = hosted_track(&tmp, &settings);
        {
            let mut turn = track.turn(MODEL_SPEC.into(), &input(AgentMode::Build));
            run_params(&mut turn)
                .history
                .push(Message::user(PROMPT.into()));
            settings.store(Arc::new(host_settings(true)));
        }

        assert_eq!(SessionSettings::of(&load(&tmp)), host_settings(true));
    }

    #[test]
    fn an_idle_save_writes_no_file_for_an_empty_session() {
        let tmp = TempDir::new().unwrap();
        let settings: SharedSettings = Arc::new(ArcSwap::from_pointee(host_settings(true)));
        let mut track = hosted_track(&tmp, &settings);
        track.save_settings();
        drop(track);
        assert!(!log_path(&tmp).exists(), "{NO_EMPTY_FILE}");
    }

    /// A host can accept a change and close the session before the driver
    /// saw it idle; the track saves it as it drops.
    #[test]
    fn dropping_the_track_saves_unsaved_settings() {
        let tmp = TempDir::new().unwrap();
        let settings: SharedSettings = Arc::new(ArcSwap::from_pointee(host_settings(false)));
        let mut track = hosted_track(&tmp, &settings);
        {
            let mut turn = track.turn(MODEL_SPEC.into(), &input(AgentMode::Build));
            run_params(&mut turn)
                .history
                .push(Message::user(PROMPT.into()));
        }
        settings.store(Arc::new(host_settings(true)));
        drop(track);

        assert_eq!(SessionSettings::of(&load(&tmp)), host_settings(true));
    }

    /// Copying a change into the meta is not saving it: a turn that writes
    /// nothing leaves it unsaved, and the track's drop still has to save it.
    #[test]
    fn a_change_a_no_op_turn_copied_is_saved_on_drop() {
        let tmp = TempDir::new().unwrap();
        let settings: SharedSettings = Arc::new(ArcSwap::from_pointee(host_settings(false)));
        let mut track = hosted_track(&tmp, &settings);
        {
            let mut turn = track.turn(MODEL_SPEC.into(), &input(AgentMode::Build));
            run_params(&mut turn)
                .history
                .push(Message::user(PROMPT.into()));
        }
        settings.store(Arc::new(host_settings(true)));
        drop(track.turn(MODEL_SPEC.into(), &input(AgentMode::Build)));
        drop(track);

        assert_eq!(SessionSettings::of(&load(&tmp)), host_settings(true));
    }

    /// A toggle before any turn must not commit the repair the history made:
    /// only a turn that ran on the repaired copy may store it.
    #[test]
    fn an_idle_settings_save_keeps_the_transcript_as_loaded() {
        let tmp = TempDir::new().unwrap();
        let track = reopen_with_orphan(&tmp);
        let on_disk = load(&tmp).messages().to_vec();
        let settings = host_settings(true).shared();
        let mut track = track.with_settings(Arc::clone(&settings));

        track.save_settings();

        let saved = load(&tmp);
        assert_eq!(saved.messages(), on_disk.as_slice(), "{NOT_WIPED}");
        assert_eq!(SessionSettings::of(&saved), host_settings(true));
    }
}
