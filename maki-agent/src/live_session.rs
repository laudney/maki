use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};

use arc_swap::ArcSwap;
use event_listener::{Event, EventListener};
use maki_config::ModelPolicy;
use maki_providers::{DeliveryReceipt, Message, RequestOptions, ResultDelivery, Timeouts};
use maki_storage::id::{MakiId, SessionRef};
use thiserror::Error;

use crate::agent::managed_tools::ForegroundTools;
use crate::cancel::{CancelMap, CancelToken};
use crate::mcp::McpSession;
use crate::permissions::{PermissionManager, TaggedAnswer};
use crate::tools::{FileAccess, ToolContext, ToolRegistry, interpreter_ctx};
use crate::{AgentConfig, AgentMode, EventSender, ModelSlot, ToolOutputLines};

pub const BACKGROUND_PLAN_WRITE_DENIED: &str =
    "the chat is in plan mode, so background sessions can't change files";
const MAILBOX_CAPACITY: usize = 100;
const NOTICE_DISPLAY_MAX_BYTES: usize = 256;
const PROMOTED_TOOL_CAPACITY: usize = 64;
/// Nobody listens on an unattended context's events, so its run id is never read.
const UNATTENDED_RUN_ID: u64 = 0;

static LIVE_SESSIONS: LazyLock<Mutex<HashMap<MakiId, Weak<LiveSession>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Every frontend registers the mailbox, but only an interactive agent loop
/// publishes a seed. The seed is held weakly, so it dies with its loop. The
/// mailbox lives on through a respawn, because the new loop registers before
/// the old one lets go.
#[derive(Default)]
struct LiveSession {
    mailbox: Mutex<Mailbox>,
    seed: Mutex<SeedRegistration>,
    foreground_tools: Mutex<Weak<ForegroundTools>>,
    tool_replies: Mutex<HashMap<String, (Weak<ForegroundTools>, usize)>>,
    promoted_tools: AtomicUsize,
}

#[derive(Default)]
struct SeedRegistration {
    seed: Weak<AgentSeed>,
    generation: Option<CancelToken>,
    headless_planning: Option<HeadlessPlanning>,
}

pub(crate) enum HeadlessPlanning {
    Run(bool),
    // A client mode switch takes precedence over a run's already captured input.
    Client(bool),
}

/// One queued report for a live session: the observation the model will read
/// at the next run. Its `display_text`, if any, is the transcript line shown
/// once the loop accepts it.
#[derive(Clone, Debug)]
pub struct Notice {
    pub message: Message,
    pub owner_cancel: Option<CancelToken>,
}

impl Notice {
    fn is_live(&self) -> bool {
        self.owner_cancel
            .as_ref()
            .is_none_or(|owner| !owner.is_cancelled())
    }

    pub fn text(text: String) -> Self {
        Self {
            message: Message::observation(text),
            owner_cancel: None,
        }
    }
}

#[derive(Default)]
struct Mailbox {
    pending: VecDeque<(Notice, bool)>,
    /// Pinged by a wake-flagged notify, so a waiter acts on it without
    /// polling.
    signal: Event,
}

#[derive(Debug, Error)]
pub enum MailboxError {
    #[error("session not live: {0}")]
    NotLive(MakiId),
    #[error("notification owner ended")]
    OwnerEnded,
}

#[derive(Clone)]
pub struct SessionMailbox {
    session_id: MakiId,
    live: Arc<LiveSession>,
}

/// A plugin that starts a subagent outside of any tool call has no context
/// to clone, so it builds one from this.
pub struct AgentSeed {
    pub session_id: SessionRef,
    owner_cancel: CancelToken,
    subagent_cancels: Arc<CancelMap<String>>,
    pub model_slot: Arc<ArcSwap<ModelSlot>>,
    pub config: AgentConfig,
    pub tool_output_lines: ToolOutputLines,
    pub permissions: Arc<PermissionManager>,
    pub mcp: Option<McpSession>,
    pub timeouts: Timeouts,
    pub file_access: Arc<FileAccess>,
    pub model_policy: Arc<ModelPolicy>,
    /// The chat's thinking and whether it plans, as the user has them right
    /// now. The UI keeps them fresh even before the first message, so
    /// background work never thinks harder than the user asked. We keep a bool
    /// and not the mode on purpose. Handed to a background run, the mode would
    /// bring the plan prompt along, plus a free pass to write the plan file.
    chat_settings: Mutex<(RequestOptions, bool)>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One line, cut on a character boundary, or `None` when nothing is left.
fn notice_label(text: &str) -> Option<String> {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let label = line[..line.floor_char_boundary(NOTICE_DISPLAY_MAX_BYTES)].trim_end();
    (!label.is_empty()).then(|| label.to_owned())
}

impl SessionMailbox {
    pub(crate) fn current(session_id: MakiId, owner: &CancelToken) -> Option<Self> {
        let live = lock(&LIVE_SESSIONS).get(&session_id)?.upgrade()?;
        let mailbox = Self { session_id, live };
        mailbox.owns_generation(owner).then_some(mailbox)
    }

    pub(crate) fn owns_generation(&self, owner: &CancelToken) -> bool {
        !owner.is_cancelled()
            && lock(&self.live.seed)
                .generation
                .as_ref()
                .is_some_and(|current| current.same_token(owner))
    }

    pub(crate) fn publish_tools(&self, tools: &Arc<ForegroundTools>) {
        let generation = lock(&self.live.seed);
        if !generation
            .generation
            .as_ref()
            .is_some_and(|current| tools.owned_by(current))
        {
            return;
        }
        *lock(&self.live.foreground_tools) = Arc::downgrade(tools);
    }

    /// Select the unfinished attached calls of this exact main-agent run. An
    /// opted-in call is stopped and answers with its receipt, and other calls
    /// use generic promotion. Generic capacity failure changes no call.
    pub fn background_tools(&self, run_id: u64) -> usize {
        let tools = lock(&self.live.foreground_tools).upgrade();
        tools
            .filter(|tools| tools.run_id() == run_id)
            .map_or(0, |tools| tools.promote())
    }

    pub(crate) fn reserve_tools(&self, count: usize) -> bool {
        self.live
            .promoted_tools
            .try_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live + count <= PROMOTED_TOOL_CAPACITY).then_some(live + count)
            })
            .is_ok()
    }

    pub(crate) fn release_tool(&self) {
        self.live.promoted_tools.fetch_sub(1, Ordering::AcqRel);
    }

    pub(crate) fn route_tool_reply(&self, id: String, tools: &Arc<ForegroundTools>, index: usize) {
        lock(&self.live.tool_replies).insert(id, (Arc::downgrade(tools), index));
    }

    pub(crate) fn retire_tool_replies(&self, tools: &Arc<ForegroundTools>, index: usize) {
        let tools = Arc::downgrade(tools);
        lock(&self.live.tool_replies)
            .retain(|_, (owner, call)| !owner.ptr_eq(&tools) || *call != index);
    }

    pub(crate) fn remove_tool_reply(&self, id: &str) {
        lock(&self.live.tool_replies).remove(id);
    }

    /// Sends a tagged permission reply to the invocation that asked for it.
    pub fn reply_tool(session_id: MakiId, raw_answer: &str) -> bool {
        let Some(answer) = TaggedAnswer::decode(raw_answer) else {
            return false;
        };
        let Some(live) = lock(&LIVE_SESSIONS)
            .get(&session_id)
            .and_then(Weak::upgrade)
        else {
            return false;
        };
        let route = lock(&live.tool_replies).get(&answer.request_id).cloned();
        route
            .and_then(|(owner, index)| owner.upgrade().map(|owner| (owner, index)))
            .is_some_and(|(owner, index)| owner.reply(index, &answer.request_id, raw_answer))
    }

    /// The original permission id and invocation-private input route for a
    /// frontend that presents permission requests outside the terminal UI.
    pub fn tool_permission(
        session_id: MakiId,
        request_id: &str,
    ) -> Option<(String, flume::Sender<String>)> {
        let live = lock(&LIVE_SESSIONS).get(&session_id)?.upgrade()?;
        let (owner, index) = lock(&live.tool_replies).get(request_id)?.clone();
        owner.upgrade()?.permission(index, request_id)
    }

    pub(crate) fn delivery_receipt(
        &self,
        delivery: ResultDelivery,
        owner: CancelToken,
    ) -> Arc<DeliveryReceipt> {
        let live = Arc::downgrade(&self.live);
        Arc::new(DeliveryReceipt::new(delivery, move || {
            !owner.is_cancelled()
                && live.upgrade().is_some_and(|live| {
                    lock(&live.seed)
                        .generation
                        .as_ref()
                        .is_some_and(|current| current.same_token(&owner))
                })
        }))
    }

    pub fn session_id(&self) -> MakiId {
        self.session_id
    }

    pub fn register(session_id: MakiId) -> Self {
        let mut sessions = lock(&LIVE_SESSIONS);
        if let Some(live) = sessions.get(&session_id).and_then(Weak::upgrade) {
            return Self { session_id, live };
        }

        let live = Arc::new(LiveSession::default());
        sessions.insert(session_id, Arc::downgrade(&live));
        Self { session_id, live }
    }

    /// The notice's `display_text` becomes its transcript line, shown and
    /// saved once an agent run accepts it. The model only ever reads its body.
    /// The owner guard rides on the message as a receipt, so it survives a
    /// frontend turning the notice into a run preamble. It only watches the
    /// owner: a plugin may report into another session than its owner's.
    pub fn notify(session_id: MakiId, mut notice: Notice, wake: bool) -> Result<(), MailboxError> {
        if !notice.is_live() {
            return Err(MailboxError::OwnerEnded);
        }
        let live = {
            let mut sessions = lock(&LIVE_SESSIONS);
            let Some(live) = sessions.get(&session_id).and_then(Weak::upgrade) else {
                sessions.remove(&session_id);
                return Err(MailboxError::NotLive(session_id));
            };
            live
        };
        let label = notice.message.display_text.take();
        notice.message.display_text = label.as_deref().and_then(notice_label);
        if let Some(owner) = &notice.owner_cancel
            && notice.message.delivery.is_none()
        {
            let owner = owner.clone();
            notice.message.delivery = Some(Arc::new(DeliveryReceipt::new(
                ResultDelivery::default(),
                move || !owner.is_cancelled(),
            )));
        }
        let mut state = lock(&live.mailbox);
        if state.pending.len() == MAILBOX_CAPACITY {
            state.pending.pop_front();
        }
        state.pending.push_back((notice, wake));
        if wake {
            state.signal.notify(usize::MAX);
        }
        Ok(())
    }

    pub fn drain(&self) -> Vec<Notice> {
        let mut state = lock(&self.live.mailbox);
        state
            .pending
            .drain(..)
            .map(|(notice, _)| notice)
            .filter(Notice::is_live)
            .collect()
    }

    pub fn claim_wake(&self) -> Vec<Notice> {
        let mut state = lock(&self.live.mailbox);
        state.pending.retain(|(notice, _)| notice.is_live());
        if !state.pending.iter().any(|(_, wake)| *wake) {
            return Vec::new();
        }
        state.pending.drain(..).map(|(notice, _)| notice).collect()
    }

    /// Registers interest in the next wake-flagged notice. Register it before
    /// checking the mailbox: a notify landing between the check and the wait
    /// would otherwise be missed.
    pub fn wake_listener(&self) -> EventListener {
        lock(&self.live.mailbox).signal.listen()
    }

    /// The registry only keeps a weak link, so the caller has to hold on to
    /// the seed. A rewind publishes a new seed, and it starts with the old chat
    /// settings, so background work never sees the defaults in the short gap
    /// before the UI sends them again. The file locks carry over too, because
    /// background sessions opened before the rewind still edit through the old
    /// ones, and their edits have to keep taking turns with the new loop's.
    pub fn publish_seed(&self, mut seed: AgentSeed) -> Arc<AgentSeed> {
        let mut published = lock(&self.live.seed);
        if let Some(previous) = published.seed.upgrade() {
            *lock(&seed.chat_settings) = *lock(&previous.chat_settings);
            seed.file_access = previous.file_access.sharing_locks();
        }
        let seed = Arc::new(seed);
        *published = SeedRegistration {
            seed: Arc::downgrade(&seed),
            generation: Some(seed.owner_cancel.clone()),
            headless_planning: None,
        };
        seed
    }

    /// Headless loops have a generation owner without an interactive seed.
    pub fn publish_generation(&self, generation: CancelToken) {
        *lock(&self.live.seed) = SeedRegistration {
            seed: Weak::new(),
            generation: Some(generation),
            headless_planning: Some(HeadlessPlanning::Run(false)),
        };
    }

    pub(crate) fn set_mode(&self, owner: &CancelToken, mode: &AgentMode) {
        self.set_planning(owner, HeadlessPlanning::Run(mode.plan_path().is_some()));
    }

    pub(crate) fn set_planning(&self, owner: &CancelToken, planning: HeadlessPlanning) {
        let mut published = lock(&self.live.seed);
        if !owner.is_cancelled()
            && published
                .generation
                .as_ref()
                .is_some_and(|current| current.same_token(owner))
            && let Some(current) = &mut published.headless_planning
            && !(matches!(current, HeadlessPlanning::Client(_))
                && matches!(&planning, HeadlessPlanning::Run(_)))
        {
            *current = planning;
        }
    }

    pub fn seed(&self) -> Option<Arc<AgentSeed>> {
        lock(&self.live.seed)
            .seed
            .upgrade()
            .filter(|seed| !seed.owner_cancel.is_cancelled())
    }
}

impl Drop for SessionMailbox {
    fn drop(&mut self) {
        if Arc::strong_count(&self.live) != 1 {
            return;
        }
        let weak = Arc::downgrade(&self.live);
        let mut sessions = lock(&LIVE_SESSIONS);
        if Arc::strong_count(&self.live) == 1
            && sessions
                .get(&self.session_id)
                .is_some_and(|registered| registered.ptr_eq(&weak))
        {
            sessions.remove(&self.session_id);
        }
    }
}

impl AgentSeed {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: SessionRef,
        owner_cancel: CancelToken,
        subagent_cancels: Arc<CancelMap<String>>,
        model_slot: Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        permissions: Arc<PermissionManager>,
        mcp: Option<McpSession>,
        timeouts: Timeouts,
        model_policy: Arc<ModelPolicy>,
    ) -> Self {
        Self {
            session_id,
            owner_cancel,
            subagent_cancels,
            model_slot,
            config,
            tool_output_lines,
            permissions,
            mcp,
            timeouts,
            file_access: FileAccess::fresh(),
            model_policy,
            chat_settings: Mutex::default(),
        }
    }

    /// `None` when the session is closed or has no interactive loop, like a
    /// headless or ACP run.
    pub fn live(session_id: MakiId) -> Option<Arc<Self>> {
        Self::registered(session_id).filter(|seed| !seed.owner_cancel.is_cancelled())
    }

    /// Teardown needs the retired token too, while admission must use `live`.
    pub fn generation(session_id: MakiId) -> Option<CancelToken> {
        let live = lock(&LIVE_SESSIONS).get(&session_id)?.upgrade()?;
        lock(&live.seed).generation.clone()
    }

    fn registered(session_id: MakiId) -> Option<Arc<Self>> {
        let live = lock(&LIVE_SESSIONS).get(&session_id)?.upgrade()?;
        lock(&live.seed).seed.upgrade()
    }

    pub fn set_chat_settings(&self, opts: RequestOptions, mode: &AgentMode) {
        *lock(&self.chat_settings) = (opts, mode.plan_path().is_some());
    }

    pub fn chat_opts(&self) -> RequestOptions {
        lock(&self.chat_settings).0
    }

    /// Why background work for `chat` may not change a file right now.
    ///
    /// That work always runs in build mode, but plan mode still means "leave
    /// my files alone". So every write asks here, and a switch to plan mode
    /// also covers the next write of a prompt that is already running. A chat
    /// that is gone can't say it isn't planning, so that is a no too.
    pub fn background_write_block(chat: MakiId, owner: Option<CancelToken>) -> Option<String> {
        let planning = || {
            let owner = owner.as_ref()?;
            let live = lock(&LIVE_SESSIONS).get(&chat)?.upgrade()?;
            let published = lock(&live.seed);
            if owner.is_cancelled() || !published.generation.as_ref()?.same_token(owner) {
                return None;
            }
            published
                .seed
                .upgrade()
                .map(|seed| lock(&seed.chat_settings).1)
                .or(published.headless_planning.as_ref().map(|mode| match mode {
                    HeadlessPlanning::Run(planning) | HeadlessPlanning::Client(planning) => {
                        *planning
                    }
                }))
        };
        match planning() {
            Some(true) => Some(BACKGROUND_PLAN_WRITE_DENIED.to_owned()),
            Some(false) => None,
            None => Some(MailboxError::NotLive(chat).to_string()),
        }
    }

    /// No chat shows this work, so its events go to a channel nobody reads.
    /// Nobody can answer a permission prompt either, so there is no answer
    /// channel and `enforce` denies the call instead of waiting.
    pub fn tool_context(&self, cancel: CancelToken) -> ToolContext {
        let (sink, _) = flume::unbounded();
        let slot = self.model_slot.load();
        ToolContext {
            provider: Arc::clone(&slot.provider),
            model: Arc::new(slot.model.clone()),
            session_id: Some(self.session_id.clone()),
            background_for: Some(self.session_id.id()),
            mcp: self.mcp.clone(),
            config: self.config.clone(),
            tool_output_lines: self.tool_output_lines,
            timeouts: self.timeouts,
            opts: self.chat_opts(),
            model_policy: Arc::clone(&self.model_policy),
            subagent_cancels: Arc::clone(&self.subagent_cancels),
            ..interpreter_ctx(
                &AgentMode::Build,
                &EventSender::new(sink, UNATTENDED_RUN_ID)
                    .with_owner_cancel(self.owner_cancel.clone()),
                cancel,
                Arc::clone(&self.permissions),
                self.file_access.sharing_locks(),
                None,
                Arc::clone(ToolRegistry::global_arc()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use futures_lite::future::poll_once;
    use maki_providers::ThinkingConfig;
    use test_case::test_case;

    use super::*;
    use crate::tools::FileKey;
    use crate::tools::test_support::{future_mtime, stub_seed};

    const CHAT_OPTS: RequestOptions = RequestOptions {
        thinking: ThinkingConfig::Adaptive,
        fast: true,
    };
    const PLAN_FILE: &str = "/tmp/plan.md";
    const EDITED_FILE: &str = "foo.rs";
    const NOTICE_BODY: &str = "child finished";
    const LABEL_HEAD: &str = "Background agent finished:";
    const WIDE_CHAR: &str = "é";

    fn text(notice: &Notice) -> &str {
        notice.message.first_text_content().unwrap()
    }

    #[test]
    fn notify_cuts_the_label_to_one_line_on_a_char_boundary() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let wide = WIDE_CHAR.repeat(NOTICE_DISPLAY_MAX_BYTES);
        let kept = (NOTICE_DISPLAY_MAX_BYTES - LABEL_HEAD.len() - 1) / WIDE_CHAR.len();
        let expected = format!("{LABEL_HEAD} {}", WIDE_CHAR.repeat(kept));
        assert_eq!(
            expected.len(),
            NOTICE_DISPLAY_MAX_BYTES - 1,
            "the byte limit must fall inside a character"
        );
        let mut notice = Notice::text(NOTICE_BODY.into());
        notice.message.display_text = Some(format!("{LABEL_HEAD}\n {wide}"));

        SessionMailbox::notify(id, notice, false).unwrap();

        let notice = mailbox.drain().remove(0);
        assert_eq!(notice.message.display_text, Some(expected));
        assert_eq!(text(&notice), NOTICE_BODY);
    }

    #[test_case(None ; "absent")]
    #[test_case(Some(" \n ") ; "blank")]
    fn notify_leaves_a_notice_without_a_label_unlabelled(label: Option<&str>) {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let mut notice = Notice::text(NOTICE_BODY.into());
        notice.message.display_text = label.map(str::to_owned);

        SessionMailbox::notify(id, notice, false).unwrap();

        let notice = mailbox.drain().remove(0);
        assert_eq!(notice.message.display_text, None);
        assert_eq!(text(&notice), NOTICE_BODY);
    }

    #[test]
    fn background_context_keeps_its_exact_session_generation() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let (owner, owner_cancel) = CancelToken::new();
        let mut initial = stub_seed(id);
        initial.owner_cancel = owner_cancel;
        let seed = mailbox.publish_seed(initial);
        let ctx = seed.tool_context(CancelToken::none());
        let original_owner = ctx.event_tx.owner_cancel().unwrap();
        assert!(!original_owner.is_cancelled());
        let child_id = "plugin-child".to_owned();
        let (child_trigger, child_cancel) = CancelToken::new();
        ctx.subagent_cancels
            .insert_detached(child_id.clone(), child_trigger);
        seed.subagent_cancels.cancel(child_id);
        assert!(child_cancel.is_cancelled());

        owner.cancel();
        drop(seed);
        assert!(original_owner.is_cancelled());
        assert!(mailbox.seed().is_none());
        assert!(AgentSeed::live(id).is_none());
        assert!(
            AgentSeed::generation(id)
                .unwrap()
                .same_token(&original_owner)
        );

        let replacement = mailbox.publish_seed(stub_seed(id));
        let live = AgentSeed::live(id).unwrap();
        assert!(Arc::ptr_eq(&live, &replacement));
        assert!(original_owner.is_cancelled());
        assert!(
            !live
                .tool_context(CancelToken::none())
                .event_tx
                .owner_cancel()
                .unwrap()
                .is_cancelled()
        );
    }

    #[test]
    fn same_session_replacement_rejects_notices_before_delayed_cleanup() {
        let id = MakiId::generate();
        let old = SessionMailbox::register(id);
        let (owner, owner_cancel) = CancelToken::new();
        let mut stale = Notice::text("old child finished".into());
        stale.owner_cancel = Some(owner_cancel);
        SessionMailbox::notify(id, stale.clone(), true).unwrap();
        SessionMailbox::notify(id, Notice::text("quiet current notice".into()), false).unwrap();

        owner.cancel();
        let replacement = SessionMailbox::register(id);

        assert!(
            replacement.claim_wake().is_empty(),
            "a stale wake cannot wake a live quiet notice"
        );
        assert!(matches!(
            SessionMailbox::notify(id, stale, true),
            Err(MailboxError::OwnerEnded)
        ));
        assert_eq!(
            replacement.drain().iter().map(text).collect::<Vec<_>>(),
            ["quiet current notice"]
        );
        assert!(old.drain().is_empty());
        SessionMailbox::notify(id, Notice::text("current child finished".into()), true).unwrap();
        assert_eq!(
            replacement
                .claim_wake()
                .iter()
                .map(text)
                .collect::<Vec<_>>(),
            ["current child finished"]
        );
    }

    #[test]
    fn admission_guards_owned_notices_and_keeps_existing_receipts() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let (_owner, generation) = CancelToken::new();
        mailbox.publish_generation(generation.clone());
        let delivery = ResultDelivery::default();
        let (rejected_tx, rejected_rx) = flume::bounded(1);
        delivery.on_reject(move || {
            let _ = rejected_tx.try_send(());
        });
        let receipt = mailbox.delivery_receipt(delivery, generation.clone());
        let mut receipted = Notice::text(NOTICE_BODY.into());
        receipted.message.delivery = Some(Arc::clone(&receipt));
        receipted.owner_cancel = Some(generation.clone());
        let mut owned = Notice::text(NOTICE_BODY.into());
        owned.owner_cancel = Some(generation);

        for notice in [receipted, owned, Notice::text(NOTICE_BODY.into())] {
            SessionMailbox::notify(id, notice, false).unwrap();
        }

        let notices = mailbox.drain();
        let kept = notices[0].message.delivery.as_ref().unwrap();
        assert!(Arc::ptr_eq(kept, &receipt));
        assert!(notices[1].message.delivery.as_ref().unwrap().is_live());
        assert!(notices[2].message.delivery.is_none());
        kept.accept();
        drop(notices);
        assert!(rejected_rx.is_empty());
    }

    #[test]
    fn a_notice_owned_by_another_session_stays_live_until_its_owner_ends() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let (_generation, current) = CancelToken::new();
        mailbox.publish_generation(current);
        let (other_trigger, other_owner) = CancelToken::new();
        let mut notice = Notice::text(NOTICE_BODY.into());
        notice.owner_cancel = Some(other_owner);

        SessionMailbox::notify(id, notice, false).unwrap();

        let notices = mailbox.drain();
        let receipt = notices[0].message.delivery.as_ref().unwrap();
        assert!(receipt.is_live());
        drop(other_trigger);
        assert!(!receipt.is_live());
    }

    #[test]
    fn notifications_drain_in_order_and_clear_wake() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, Notice::text("first".into()), true).unwrap();
        SessionMailbox::notify(id, Notice::text("second".into()), true).unwrap();

        let notices = mailbox.drain();
        assert_eq!(
            notices.iter().map(text).collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(notices.iter().all(|n| n.message.is_observation()));
        assert!(mailbox.claim_wake().is_empty());
    }

    #[test]
    fn quiet_notifications_do_not_claim_a_wake() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, Notice::text("built".into()), false).unwrap();

        assert!(mailbox.claim_wake().is_empty());
        assert_eq!(mailbox.drain().len(), 1);
    }

    #[test]
    fn waking_notification_claims_all_pending_messages() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, Notice::text("quiet".into()), false).unwrap();
        SessionMailbox::notify(id, Notice::text("wake".into()), true).unwrap();

        let notices = mailbox.claim_wake();
        assert_eq!(
            notices.iter().map(text).collect::<Vec<_>>(),
            ["quiet", "wake"]
        );
        assert!(mailbox.drain().is_empty());
    }

    #[test]
    fn a_waking_notice_wakes_a_parked_listener() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let listener = mailbox.wake_listener();

        SessionMailbox::notify(id, Notice::text("done".into()), true).unwrap();

        assert!(
            smol::block_on(async {
                futures_lite::future::or(
                    async {
                        listener.await;
                        true
                    },
                    async {
                        smol::Timer::after(std::time::Duration::from_secs(5)).await;
                        false
                    },
                )
                .await
            }),
            "a wake-flagged notify must wake a parked frontend"
        );
    }

    #[test]
    fn a_quiet_notice_leaves_the_listener_parked() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let mut listener = mailbox.wake_listener();

        SessionMailbox::notify(id, Notice::text("built".into()), false).unwrap();

        assert!(
            smol::block_on(futures_lite::future::poll_once(&mut listener)).is_none(),
            "only a wake is worth starting a new run for"
        );
    }

    #[test]
    fn notifications_drop_the_oldest_message_at_capacity() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        for index in 0..=MAILBOX_CAPACITY {
            SessionMailbox::notify(id, Notice::text(index.to_string()), false).unwrap();
        }

        let notices = mailbox.drain();
        assert_eq!(notices.len(), MAILBOX_CAPACITY);
        assert_eq!(text(&notices[0]), "1");
        assert_eq!(text(notices.last().unwrap()), MAILBOX_CAPACITY.to_string());
    }

    #[test]
    fn registrations_for_the_same_id_share_state() {
        let id = MakiId::generate();
        let first = SessionMailbox::register(id);
        let second = SessionMailbox::register(id);
        SessionMailbox::notify(id, Notice::text("built".into()), false).unwrap();

        assert_eq!(second.drain().len(), 1);
        assert!(first.drain().is_empty());
    }

    #[test]
    fn dropping_the_last_registration_closes_the_mailbox() {
        let id = MakiId::generate();
        drop(SessionMailbox::register(id));

        assert!(!lock(&LIVE_SESSIONS).contains_key(&id));
        assert!(SessionMailbox::notify(id, Notice::text("late".into()), false).is_err());
    }

    #[test]
    fn dropping_one_registration_keeps_the_shared_mailbox() {
        let id = MakiId::generate();
        let first = SessionMailbox::register(id);
        let second = SessionMailbox::register(id);

        drop(first);

        assert!(lock(&LIVE_SESSIONS).contains_key(&id));
        SessionMailbox::notify(id, Notice::text("built".into()), false).unwrap();
        assert_eq!(second.drain().len(), 1);
    }

    #[test]
    fn stale_drop_does_not_remove_a_replacement() {
        let id = MakiId::generate();
        let stale = SessionMailbox::register(id);
        let replacement = SessionMailbox {
            session_id: id,
            live: Arc::default(),
        };
        lock(&LIVE_SESSIONS).insert(id, Arc::downgrade(&replacement.live));

        drop(stale);
        SessionMailbox::notify(id, Notice::text("built".into()), false).unwrap();

        assert_eq!(replacement.drain().len(), 1);
    }

    #[test]
    fn legacy_and_canonical_ids_address_the_same_mailbox() {
        let legacy: MakiId = "01965087-4c71-7f00-8000-000000000001".parse().unwrap();
        let canonical: MakiId = legacy.to_string().parse().unwrap();
        let mailbox = SessionMailbox::register(legacy);
        SessionMailbox::notify(canonical, Notice::text("built".into()), false).unwrap();

        assert_eq!(mailbox.drain().len(), 1);
    }

    #[test]
    fn seed_is_live_only_while_its_owner_holds_it() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        assert!(AgentSeed::live(id).is_none());

        let owned = mailbox.publish_seed(stub_seed(id));
        assert!(Arc::ptr_eq(&AgentSeed::live(id).unwrap(), &owned));

        drop(owned);
        assert!(AgentSeed::live(id).is_none());
        SessionMailbox::notify(id, Notice::text("built".into()), true).unwrap();
        assert_eq!(mailbox.claim_wake().len(), 1);
    }

    #[test]
    fn respawned_seed_wins_and_keeps_the_chat_settings() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let old = mailbox.publish_seed(stub_seed(id));
        old.set_chat_settings(CHAT_OPTS, &AgentMode::Plan(PLAN_FILE.into()));

        let new = mailbox.publish_seed(stub_seed(id));
        drop(old);

        let live = AgentSeed::live(id).unwrap();
        assert!(Arc::ptr_eq(&live, &new));
        let ctx = live.tool_context(CancelToken::none());
        assert_eq!(ctx.opts, CHAT_OPTS);
        assert_eq!(ctx.mode, AgentMode::Build);
        assert_eq!(ctx.background_for, Some(id));
        assert_eq!(
            AgentSeed::background_write_block(id, AgentSeed::generation(id)).as_deref(),
            Some(BACKGROUND_PLAN_WRITE_DENIED)
        );
    }

    /// A chat that is gone can't tell us it stopped planning.
    #[test]
    fn background_writes_stop_once_the_chat_is_gone() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let seed = mailbox.publish_seed(stub_seed(id));
        assert_eq!(
            AgentSeed::background_write_block(id, AgentSeed::generation(id)),
            None
        );

        drop(seed);
        assert_eq!(
            AgentSeed::background_write_block(id, AgentSeed::generation(id)),
            Some(MailboxError::NotLive(id).to_string())
        );
    }

    /// The loop reads a file, the user edits it, and a monitor reads it too.
    /// The loop must still be told its view is stale. After a rewind the
    /// monitor's edits must still wait for the new loop's.
    #[test]
    fn background_reads_stay_apart_but_locks_survive_a_respawn() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(EDITED_FILE);
        fs::write(&path, "").unwrap();
        let key = FileKey::new(&path);
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let old = mailbox.publish_seed(stub_seed(id));
        let background = old.tool_context(CancelToken::none());

        old.file_access.record_read(&key);
        future_mtime(&path);
        background.file_access.record_read(&key);
        assert!(old.file_access.check_before_edit(&key).is_err());

        let respawned = mailbox.publish_seed(stub_seed(id));
        drop(old);
        smol::block_on(async {
            let _held = respawned.file_access.acquire(&key).await;
            assert!(
                poll_once(background.file_access.acquire(&key))
                    .await
                    .is_none()
            );
        });
    }
}
