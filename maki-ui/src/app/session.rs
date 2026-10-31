use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::agent::remote_settings::Setting;
use crate::app::tasks::{BackgroundMoves, TaskOutcome, TaskParent, tab_ending};
use crate::chat::{Chat, DONE_TEXT, history_to_display};
use crate::components::command::PROMPT_NEEDS;
use crate::components::input::{COMMAND_PREFIX, sent_text};
use crate::components::rewind_picker::{RewindEntry, drops_receipt};
use crate::components::{Action, BackgroundBadge, BackgroundOutcome};
use crate::text_buffer::Writers;
use maki_lua_core::agent_autocmd::HostEvent;
use maki_lua_core::plugin_permissions::{PluginPermissions, PluginReach};
use maki_types::message::{Message, TaskEnd};
use maki_types::thinking::RequestOptions;
use maki_types::usage::TokenUsage;

use maki_storage::id::MakiId;
use maki_storage::remote_drafts::RemoteDraft;
use maki_storage::sessions::{SessionMeta, StoredWriters};
use maki_types::event::SessionEndReason;
use maki_wire::session::SessionOperation;

use crate::{AppSession, OpenSession};

use super::session_state::SessionState;
use super::{App, MAIN_CHAT, Mode, PlanState, Sending, Status, render_place};

/// The shortest gap between two writes that carry only UI state.
const SOFT_SAVE_DELAY: Duration = Duration::from_millis(1000);

const FORK_FAILED_MSG: &str = "Fork failed";

/// Every user turn was sent by the user or by a plugin that could send a
/// prompt, and we do not keep which. So a turn that Enter would run rather
/// than send comes back as written by a nameless plugin holding only what a
/// prompt takes, see [`PROMPT_NEEDS`].
fn turn_writers(text: &str) -> Option<StoredWriters> {
    let sent = sent_text(text);
    let runs = sent.starts_with(COMMAND_PREFIX) || Sending::of(sent, false).runs();
    runs.then_some(StoredWriters::Agent)
}

/// Grants held by fewer than all of {writers} cannot be vouched for once
/// their names are gone, so only what a prompt takes is kept, and only when
/// each of them held it.
fn stored_writers(writers: &Writers) -> Option<StoredWriters> {
    if writers.is_empty() {
        return None;
    }
    let each_sends_prompts = writers
        .iter()
        .all(|writer| PROMPT_NEEDS.iter().all(|&perm| writer.is_allowed(perm)));
    Some(if each_sends_prompts {
        StoredWriters::Agent
    } else {
        StoredWriters::Unknown
    })
}

fn unknown_writer(writers: StoredWriters) -> PluginReach {
    let mut permissions = PluginPermissions::denied();
    for &perm in PROMPT_NEEDS {
        permissions.set(perm, writers == StoredWriters::Agent);
    }
    PluginReach::new(permissions)
}

/// The one content check. `App::checkpoint` saves a session only when this
/// holds, and the shutdown report asks the same question, so the report and the
/// disk never disagree. A draft, a queue or plan mode used to count too, and
/// every tab that had one showed up in the picker as another empty session.
pub(crate) fn session_has_content(session: &AppSession) -> bool {
    !session.messages().is_empty()
}

fn moved_outcomes(
    messages: &[Message],
    pending: &[String],
    queued_end: impl Fn(&str) -> Option<TaskEnd>,
) -> Vec<(String, BackgroundOutcome)> {
    let mut outcomes = HashMap::new();
    for message in messages {
        if let Some(settled) = &message.settles {
            outcomes.insert(settled.task.as_str(), settled.end.into());
        }
        for id in &message.receipts {
            outcomes
                .entry(id.as_str())
                .or_insert(BackgroundOutcome::Cancelled);
        }
    }
    for id in pending {
        let end = queued_end(id).unwrap_or(TaskEnd::Lost);
        outcomes.insert(id, end.into());
    }
    outcomes
        .into_iter()
        .map(|(id, outcome)| (id.to_owned(), outcome))
        .collect()
}

impl App {
    /// Stricter than `session_has_content`. A draft or plan mode is not worth a
    /// file, but it is still worth keeping when the picker wants this tab for
    /// another session.
    pub(crate) fn is_blank(&self) -> bool {
        let session = &self.state.session;
        let input = self.unsent();
        !session_has_content(session) && input.is_empty() && self.state.mode == Mode::Build
    }

    /// The event loop runs this once per frame per session. It syncs whatever
    /// the session mirrors from live state, then writes only if a mutator
    /// really changed something. No dirty flags and no per-event save calls,
    /// so there is nothing left to forget.
    pub(crate) fn checkpoint(&mut self) {
        self.checkpoint_with(SOFT_SAVE_DELAY);
    }

    /// A checkpoint for the paths that get no later frame, so a draft typed a
    /// keystroke ago still reaches disk: shutdown, and swapping the session out
    /// from under the tab.
    pub(crate) fn checkpoint_now(&mut self) {
        self.checkpoint_with(Duration::ZERO);
    }

    pub(super) fn checkpoint_with(&mut self, soft_delay: Duration) {
        self.checkpoint_remote(soft_delay);
    }

    /// What the user typed and has not handed over: the main chat's draft,
    /// and the queue's text, or the queue a lost run handed back.
    pub(crate) fn unsent(&self) -> RemoteDraft {
        let (draft, writers) = self.main_draft();
        let has_draft = !draft.is_empty();
        RemoteDraft {
            input_draft_writers: stored_writers(writers).filter(|_| has_draft),
            input_draft: has_draft.then_some(draft),
            queued_messages: if self.recoverable_queue.is_empty() {
                self.queue.text_messages()
            } else {
                self.recoverable_queue.clone()
            },
        }
    }

    pub(super) fn save_input_history(&self) {
        if let Err(e) = self.input_box.history().save() {
            tracing::warn!(error = %e, "input history save failed");
        }
    }

    /// Call only once `state.session` is final: the chats it builds are
    /// stamped with that session for life. A plugin task of that same session
    /// stays if it is still running: ending it here would leave a task shown
    /// as done that keeps streaming and that Esc can no longer reach.
    pub(super) fn reset_ui_chrome(&mut self) {
        let session = self.state.session.id;
        self.reset_ui_chrome_keeping(|chat| chat.is_live_plugin_task_of(session));
    }

    /// [`Self::reset_ui_chrome`] that keeps every task tab {keep} accepts, as
    /// it is. The main chat always goes.
    fn reset_ui_chrome_keeping(&mut self, keep: impl Fn(&Chat) -> bool) {
        // Brings the main draft back to the box before the chats holding it go.
        self.set_active_chat(MAIN_CHAT);
        self.chats
            .retain(|chat| chat.task_id().is_some() && keep(chat));
        let main = Chat::new(
            self.state.session.id,
            None,
            "Main".into(),
            self.ui_config.clone(),
            self.lua_event_handle.clone(),
        );
        self.chats.insert(MAIN_CHAT, main);
        self.active_chat = MAIN_CHAT;
        self.chat_index.clear();
        self.deleted_tasks.clear();
        self.status = Status::Idle;
        self.clear_exit_request();
        self.queue.clear();
        self.recoverable_queue.clear();
        self.close_all_overlays();
        self.status_bar.clear_flash();
        self.last_esc = None;
        self.last_quit = None;
        self.background_moves = BackgroundMoves::default();
        self.early_finals.clear();
        self.forget_untasked_parts();
        self.restoring = Arc::new(AtomicBool::new(false));
        self.plan_form.reset();
        // The draft this chrome belonged to is gone, and the Lua thread still
        // owes answers about it. Retiring them here is what keeps a handler
        // that comes back after a `/new`, a rewind or a tab switch from
        // implementing a plan in a session the user never picked in.
        self.plan_answers.abandon();
    }

    pub(crate) fn restore_display(&mut self) {
        let restoring = Arc::new(AtomicBool::new(true));
        self.restoring = restoring.clone();

        let display_msgs = history_to_display(
            self.state.session.messages(),
            self.state.session.tool_outputs(),
            self.state.session.tool_data(),
            &self.ui_config.tool_output_lines,
        );
        self.main_chat().load_messages(display_msgs);
        self.chats[MAIN_CHAT].route_loaded(self.state.session.tool_data());
        let cost = self.state.cost;
        let context_size = self.state.context_size;
        let main = self.main_chat();
        main.cost = cost;
        main.context_size = context_size;
        let meta = &self.state.session.meta;
        if let Some(draft) = meta.input_draft.clone() {
            let writers = meta.input_draft_writers;
            self.restore_input(&draft, writers);
        }

        let place = render_place(&self.state.session.cwd, self.backend.is_native());
        self.chats[MAIN_CHAT].keep_place(place);

        for sa in self.state.session.subagents().to_vec() {
            // A subagent reaches disk when it spawns but its transcript only
            // when it ends, so one without an entry here never got to finish:
            // leftovers from a kill mid-turn. It has nothing to show, and
            // restoring it would park a task no agent backs at the top of the
            // picker, running forever. The retained metadata below drops it.
            let Some(messages) = self.state.session.subagent_messages().get(&sa.tool_use_id) else {
                continue;
            };
            if self.chat_of_task(&sa.tool_use_id).is_ok() {
                continue;
            }
            let display = history_to_display(
                messages,
                self.state.session.tool_outputs(),
                self.state.session.tool_data(),
                &self.ui_config.tool_output_lines,
            );
            // Plugin tasks are never routed through `chat_index`.
            if sa.plugin.is_none() {
                self.chat_index
                    .insert(sa.tool_use_id.clone(), self.chats.len());
            }
            let mut chat = Chat::new(
                self.state.session.id,
                Some(&sa.tool_use_id),
                sa.name,
                self.ui_config.clone(),
                self.lua_event_handle.clone(),
            );
            chat.parent = sa
                .parent_call
                .filter(|id| id != &sa.tool_use_id)
                .map(|id| TaskParent::Started(Arc::from(id)));
            chat.plugin = sa.plugin;
            chat.cost = sa.cost;
            chat.model_id = sa.model;
            chat.opts = sa.thinking.map(|thinking| RequestOptions {
                thinking: thinking.into(),
                fast: sa.fast,
            });
            chat.load_messages(display);
            chat.route_loaded(self.state.session.tool_data());
            // The session file keeps the transcript but never how it ended,
            // so a reload admits that instead of guessing.
            chat.mark_finished(TaskOutcome::Unknown, DONE_TEXT);
            chat.keep_place(render_place(
                &self.state.session.cwd,
                self.backend.is_native(),
            ));

            self.chats.push(chat);
        }

        let restored = self
            .chats
            .iter()
            .filter_map(|chat| chat.task_id())
            .map(|id| id.to_string())
            .collect::<HashSet<_>>();
        let tasks = self
            .state
            .session
            .subagents()
            .iter()
            .filter(|task| restored.contains(&task.tool_use_id))
            .cloned()
            .collect();
        self.state.session_mut().set_subagents(tasks);

        self.rebadge_moves();
        restoring.store(false, std::sync::atomic::Ordering::Relaxed);
        self.restoring
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn rebadge_moves(&mut self) {
        let session = &self.state.session;
        let queued_end = |id: &str| {
            session.subagent_messages().get(id).and_then(|messages| {
                messages
                    .iter()
                    .rev()
                    .filter_map(|message| message.settles.as_ref())
                    .find(|settled| settled.task == id)
                    .map(|settled| settled.end)
            })
        };
        let mut badges = Vec::new();
        for (idx, chat) in self.chats.iter().enumerate() {
            let (messages, pending) = if idx == MAIN_CHAT {
                (session.messages(), &session.meta.pending_background[..])
            } else {
                match chat
                    .task_id()
                    .and_then(|id| session.subagent_messages().get(&**id))
                {
                    Some(messages) => (&messages[..], &[][..]),
                    None => continue,
                }
            };
            badges.extend(
                moved_outcomes(messages, pending, queued_end)
                    .into_iter()
                    .map(|(id, outcome)| (idx, id, outcome)),
            );
        }
        for (idx, id, outcome) in badges {
            let outcome = match self.chat_of_task(&id) {
                Ok(tab) if !self.chats[tab].is_finished() => None,
                Ok(tab) => {
                    let (ending, text) = tab_ending(outcome);
                    self.chats[tab].mark_finished(ending, text);
                    Some(outcome)
                }
                Err(_) => Some(outcome),
            };
            let badge = BackgroundBadge {
                id: id.clone(),
                started_at: None,
                outcome,
            };
            self.chats[idx].set_tool_background(&id, badge);
        }
    }

    #[cfg(test)]
    pub(crate) fn restore_resumed_session(&mut self) {
        self.restore_display();
    }

    pub(super) fn apply_stored_yolo(&self, meta: &SessionMeta) {
        self.yolo.set_session_yolo(meta.yolo);
    }

    /// `/new` swaps a fresh session under this tab, `Ctrl-N` spawns a new tab
    /// that rebuilds its whole state from this meta, so both gestures keep
    /// exactly what is listed here. Written out field by field so a new
    /// `SessionMeta` field has to pick a side: settings that say how the user
    /// works ride along, anything a finished turn produced stays behind, or the
    /// new session writes over work it never did.
    pub(crate) fn blank_session(&self) -> OpenSession {
        let model = self
            .backend
            .is_native()
            .then_some(self.remote_settings.as_ref())
            .flatten()
            .and_then(|models| {
                models.wanted().map(|pick| pick.value.clone()).or_else(|| {
                    models
                        .models
                        .load()
                        .as_ref()
                        .and_then(|menu| menu.current.clone())
                })
            })
            .unwrap_or_else(|| self.state.session.model.clone());
        let yolo = self
            .remote_settings
            .as_ref()
            .filter(|_| self.backend.is_native())
            .map(|settings| {
                let intent = settings.intent.load();
                (settings.lock().revision(&Setting::Yolo.key()) != 0)
                    .then_some(intent.yolo)
                    .or(self.state.session.meta.yolo)
            })
            .unwrap_or_else(|| self.yolo.persisted_yolo());
        let mut open = OpenSession::fresh(&model, &self.state.session.cwd, &self.storage);
        open.session.meta = SessionMeta {
            mode: Some(self.state.mode.into()),
            thinking: Some(self.state.thinking.into()),
            fast: self.state.fast,
            workflow: self.state.workflow,
            plan_path: None,
            plan_written: false,
            session_rules: Vec::new(),
            context_size: 0,
            input_draft: None,
            input_draft_writers: None,
            queued_messages: Vec::new(),
            accepted_work: Vec::new(),
            yolo,
            pending_background: Vec::new(),
        };
        open
    }

    pub(super) fn reset_session(&mut self) -> Vec<Action> {
        self.checkpoint_now();
        self.state.token_usage = TokenUsage::default();
        self.state.cost = None;
        self.state.context_size = 0;
        self.state.plan = PlanState::None;
        if self.state.mode == Mode::Plan {
            self.enter_plan();
        }
        // Fire before the swap. A handler cleaning up after the session
        // that just ended needs its id, and the stamp always reads
        // whichever session is current. A remote agent fires its own.
        if self.backend.is_native() {
            self.lua_event_handle
                .core
                .fire(Some(&self.state.session.id), HostEvent::SessionReset {});
            self.lua_event_handle
                .core
                .end_session(self.state.session.id, SessionEndReason::Reset);
        }
        // Swapping the claim is what gives the old session back: the snapshot
        // `checkpoint_now` just queued holds its own clone until it lands.
        let OpenSession { session, claim } = self.blank_session();
        self.apply_stored_yolo(&session.meta);
        self.state.session = Arc::new(session);
        self.state.claim = claim;
        self.reset_ui_chrome();
        maki_otel::emit::session_started(
            maki_otel::emit::START_FRESH,
            Some(&self.state.session.id.to_string()),
        );
        vec![Action::RestartAgent]
    }

    /// `/fork` copies the session under a fresh id. The event loop opens the
    /// copy as a session of its own and focuses it, so this one keeps running
    /// in the background, untouched.
    pub(super) fn fork_session(&mut self) -> Vec<Action> {
        self.fork_session_with(None)
    }

    pub(super) fn fork_to(&mut self, entry: RewindEntry) -> Vec<Action> {
        self.fork_session_with(Some((entry.turn_index, entry.prompt_text)))
    }

    fn fork_session_with(&mut self, turn: Option<(usize, String)>) -> Vec<Action> {
        let turn = turn.map(|(index, prompt)| {
            let writers = turn_writers(&prompt);
            (index, prompt, writers)
        });
        let fork = self
            .remote_link
            .as_ref()
            .ok_or_else(|| FORK_FAILED_MSG.to_owned())
            .and_then(|link| {
                link.request::<serde_json::Value>(|session_id| SessionOperation::Fork {
                    session_id,
                    turn,
                })
            })
            .and_then(|reply| {
                reply["sessionId"]
                    .as_str()
                    .ok_or_else(|| FORK_FAILED_MSG.to_owned())?
                    .parse::<MakiId>()
                    .map_err(|error| error.to_string())
            })
            .and_then(|id| OpenSession::load(id, &self.storage).map_err(|error| error.to_string()));
        match fork {
            Ok(open) => vec![Action::OpenFork(Box::new(open))],
            Err(error) => {
                self.flash(error);
                vec![]
            }
        }
    }

    fn background_task_names(&self) -> Vec<(Arc<str>, String)> {
        self.running_background_tasks()
            .filter_map(|chat| Some((Arc::clone(chat.task_id()?), chat.name.clone())))
            .collect()
    }

    pub(super) fn open_rewind_picker(&mut self) -> Vec<Action> {
        if let Some(link) = &self.remote_link {
            let loaded = link.request(|session_id| SessionOperation::Snapshot { session_id });
            match loaded {
                Ok(snapshot) => self.adopt_remote_transcript(snapshot),
                Err(error) => {
                    self.flash(error);
                    return vec![];
                }
            }
        }

        match self
            .rewind_picker
            .open(self.state.session.messages(), &self.background_task_names())
        {
            Ok(()) => vec![],
            Err(msg) => {
                self.status_bar.flash(msg);
                vec![]
            }
        }
    }

    /// A task whose receipt the rewind drops is stopped first: its result
    /// would answer a call the model no longer made. The mailbox forgets
    /// every such receipt before that, so neither the result of a stop nor a
    /// notice still queued reaches the next run. Its late events find no tab
    /// to bring back. Every other task keeps its tab as it was, ending and
    /// all, and its receipt row in the rebuilt main chat keeps its badge.
    pub(super) fn rewind_to(&mut self, entry: RewindEntry) -> Vec<Action> {
        vec![Action::Rewind(entry)]
    }

    pub(crate) fn finish_remote_rewind(&mut self, snapshot: AppSession, entry: RewindEntry) {
        let badges = self.chats[MAIN_CHAT].background_badges();
        self.run_id += 1;
        let dropped: HashSet<String> = self
            .chats
            .iter()
            .filter_map(Chat::task_id)
            .filter(|id| drops_receipt(self.state.session.messages(), entry.turn_index, id))
            .map(|id| id.to_string())
            .collect();
        self.adopt_remote_transcript(snapshot);
        self.reset_ui_chrome_keeping(|chat| {
            chat.task_id().is_some_and(|id| !dropped.contains(&**id))
                && (chat.plugin.is_some() || chat.is_finished())
        });
        self.deleted_tasks.extend(dropped);
        self.restore_display();
        for badge in badges {
            let id = badge.id.clone();
            self.chats[MAIN_CHAT].set_tool_background(&id, badge);
        }
        self.fire_session_event(HostEvent::SessionReset {});
        self.restore_input(&entry.prompt_text, turn_writers(&entry.prompt_text));
    }

    pub(crate) fn restore_remote_transcript(&mut self, snapshot: AppSession) {
        self.adopt_remote_transcript(snapshot);
        self.reset_ui_chrome();
        self.restore_display();
        if let Some(draft) = self.state.session.meta.input_draft.clone() {
            self.restore_input(&draft, self.state.session.meta.input_draft_writers);
        }
    }

    fn adopt_remote_transcript(&mut self, mut snapshot: AppSession) {
        snapshot.id = self.state.session.id;
        snapshot.meta.queued_messages.clear();
        self.state.context_size = snapshot.meta.context_size;
        self.state.session = Arc::new(snapshot);
    }

    /// Puts back a draft from disk or a rewound turn. Its {writers} come back
    /// as one nameless plugin holding what was kept of their grants, see
    /// [`turn_writers`] for a turn.
    pub(super) fn restore_input(&mut self, text: &str, writers: Option<StoredWriters>) {
        self.input_box.set_input(text);
        if let Some(writers) = writers {
            self.input_box.buffer.add_writer(&unknown_writer(writers));
        }
        self.input_box.buffer.move_to_end();
    }

    /// The event loop owns the policy and the providers, so it resolves the
    /// model and this only adopts it.
    pub(crate) fn apply_loaded_session(&mut self, open: OpenSession) {
        let previous = self.state.session.id;
        self.checkpoint_now();
        self.apply_stored_yolo(&open.session.meta);
        self.state = SessionState::from_session(open);
        if previous != self.state.session.id {
            self.lua_event_handle
                .core
                .end_session(previous, SessionEndReason::Load);
        }
        self.reset_ui_chrome();
        self.restore_display();
    }
}
