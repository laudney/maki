//! Queue for messages typed while the agent is busy, and the subagent
//! inbox a message typed in a subagent's chat goes to instead.

use maki_types::inbox::InboxSnapshot;
use std::borrow::Cow;

use maki_lua_core::agent_autocmd::HostEvent;
use maki_types::input::{AgentInput, AgentMode, InputSource};
use maki_types::message::ImageSource;
use maki_wire::session::SessionOperation;

use super::{Action, App, Status};

use crate::agent::capabilities::CONNECTION_LOST;
use crate::agent::shared_queue::{Compaction, QueueItem, QueueSender, QueuedInput};
use crate::components::queue_panel::QueueEntry;
use crate::theme;

pub(crate) use crate::agent::shared_queue::QueuedMessage;

pub(crate) const EMPTY_PROMPT_ERR: &str = "prompt is empty";
pub(crate) const NO_QUEUE_ERR: &str = "session cannot queue messages";
pub(crate) const NO_INBOX_ERR: &str = "this task takes no messages";

pub(crate) enum SubmitOutcome {
    Started(Vec<Action>),
    Queued,
    Rejected(&'static str),
}

#[derive(Default)]
pub(crate) struct MessageQueue {
    shared: Option<QueueSender>,
    focus: Option<usize>,
}

impl MessageQueue {
    pub(crate) fn set_shared(&mut self, shared: QueueSender) {
        self.shared = Some(shared);
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.shared.as_ref().is_none_or(|s| s.is_empty())
    }

    pub(crate) fn len(&self) -> usize {
        self.shared.as_ref().map_or(0, |s| s.len())
    }

    pub(crate) fn remove(&mut self, index: usize) {
        if let Some(ref shared) = self.shared
            && shared.remove(index).is_some()
        {
            self.clamp_focus();
        }
    }

    pub(crate) fn clear(&mut self) {
        if let Some(ref shared) = self.shared {
            shared.clear();
        }
        self.focus = None;
    }

    pub(crate) fn take_messages(&mut self) -> Vec<(String, Vec<ImageSource>)> {
        self.focus = None;
        self.shared
            .as_ref()
            .map_or_else(Vec::new, QueueSender::take_messages)
    }

    pub(crate) fn take_rejected(&mut self) -> Vec<(String, Vec<ImageSource>)> {
        self.shared
            .as_ref()
            .map_or_else(Vec::new, QueueSender::take_rejected)
    }

    pub(crate) fn focus(&self) -> Option<usize> {
        self.focus
    }

    pub(crate) fn set_focus(&mut self) {
        self.set_focus_at(0);
    }

    pub(crate) fn unfocus(&mut self) {
        self.focus = None;
    }

    pub(crate) fn move_focus_up(&mut self) {
        if let Some(sel) = self.focus
            && sel > 0
        {
            self.focus = Some(sel - 1);
        }
    }

    pub(crate) fn move_focus_down(&mut self) {
        if let Some(sel) = self.focus {
            let len = self.len();
            if sel + 1 < len {
                self.focus = Some(sel + 1);
            }
        }
    }

    pub(crate) fn remove_focused(&mut self) {
        if let Some(sel) = self.focus {
            self.remove(sel);
        }
    }

    pub(crate) fn panel_len(&self) -> usize {
        self.shared.as_ref().map_or(0, |s| s.panel_len())
    }

    pub(crate) fn panel_entries(&self) -> Vec<QueueEntry<'static>> {
        self.shared.as_ref().map_or(vec![], |s| s.panel_entries())
    }

    pub(crate) fn text_messages(&self) -> Vec<String> {
        self.shared.as_ref().map_or(vec![], |s| s.text_messages())
    }

    fn clamp_focus(&mut self) {
        let len = self.len();
        self.focus = match self.focus {
            Some(_) if len == 0 => None,
            Some(sel) if sel >= len => Some(len - 1),
            other => other,
        };
    }

    pub(crate) fn set_focus_at(&mut self, index: usize) {
        if index < self.len() {
            self.focus = Some(index);
        }
    }
}

impl App {
    /// The one queue-or-start decision, shared by the keyboard and Lua
    /// paths so they cannot drift. Expects raw text: interpretation (slash
    /// commands, `exit`, `!`) is the caller's job, or skipped on purpose.
    pub(crate) fn submit_prompt(&mut self, msg: QueuedMessage) -> SubmitOutcome {
        if msg.text.trim().is_empty() && msg.images.is_empty() {
            return SubmitOutcome::Rejected(EMPTY_PROMPT_ERR);
        }
        if !self.backend.accepts_prompts() {
            return SubmitOutcome::Rejected(CONNECTION_LOST);
        }
        if self.status == Status::Streaming {
            if self.queue_and_notify(msg) {
                SubmitOutcome::Queued
            } else {
                SubmitOutcome::Rejected(NO_QUEUE_ERR)
            }
        } else {
            SubmitOutcome::Started(self.start_from_queue(&msg))
        }
    }

    /// Keyboard path: nobody is around to receive an `Err`, so
    /// rejections flash on screen instead.
    pub(super) fn submit_or_queue(&mut self, msg: QueuedMessage) -> Vec<Action> {
        if !self.backend.accepts_prompts() {
            self.input_box.set_input(&msg.text);
            for image in msg.images {
                self.input_box.attach_image(image);
            }
            self.flash(CONNECTION_LOST.into());
            return vec![];
        }
        match self.submit_prompt(msg) {
            SubmitOutcome::Started(actions) => actions,
            SubmitOutcome::Queued => vec![],
            SubmitOutcome::Rejected(e) => {
                self.flash(e.into());
                vec![]
            }
        }
    }

    /// A message typed in a subagent's chat is for that subagent: it waits in
    /// the subagent's inbox until its loop reaches a turn boundary, and the
    /// `QueueItemConsumed` relayed from there draws it in this chat. The
    /// subagent runs its own settings, so the input carries them.
    pub(super) fn queue_for_subagent(&mut self, msg: QueuedMessage) -> Vec<Action> {
        let chat = &self.chats[self.active_chat];
        let (thinking, fast) = chat
            .opts
            .map_or((self.state.thinking, self.state.fast), |o| {
                (o.thinking, o.fast)
            });
        let input = AgentInput {
            message: msg.text,
            mode: AgentMode::Build,
            images: msg.images,
            preamble: Vec::new(),
            earlier: Vec::new(),
            thinking,
            fast,
            workflow: false,
            prompt: None,
            source: InputSource::Tui,
        };
        if chat.remote_inbox.is_some() {
            let rejected = input.clone();
            let id = chat.task_id().unwrap().to_string();
            let result = self
                .remote_link
                .as_ref()
                .ok_or(NO_INBOX_ERR.to_owned())
                .and_then(|link| {
                    link.request(|session_id| SessionOperation::TaskInput {
                        session_id,
                        id: id.clone(),
                        input: Box::new(input),
                    })
                });
            match result {
                Ok(snapshot) => self.update_task_queue(&id, snapshot),
                Err(error) => {
                    self.input_box.set_input(&rejected.message);
                    for image in rejected.images {
                        self.input_box.attach_image(image);
                    }
                    self.flash(error);
                }
            }
        } else {
            self.flash(NO_INBOX_ERR.into());
        }
        vec![]
    }

    pub(super) fn update_task_queue(&mut self, id: &str, snapshot: InboxSnapshot) {
        if let Ok(index) = self.chat_of_task(id) {
            let chat = &mut self.chats[index];
            if chat
                .remote_inbox
                .as_ref()
                .is_none_or(|old| old.revision <= snapshot.revision)
            {
                chat.remote_inbox = Some(snapshot);
            }
        }
    }

    /// The panel above the input shows the queue of the chat in front: the
    /// session's for the main chat, the subagent's inbox otherwise.
    pub(super) fn active_queue_len(&self) -> usize {
        if self.is_main_chat() {
            return self.queue.panel_len();
        }
        let chat = &self.chats[self.active_chat];
        chat.remote_inbox
            .as_ref()
            .map_or(0, |inbox| inbox.texts.len())
    }

    pub(super) fn active_queue_entries(&self) -> Vec<QueueEntry<'static>> {
        if self.is_main_chat() {
            return self.queue.panel_entries();
        }
        let color = theme::current().foreground;
        let chat = &self.chats[self.active_chat];
        chat.remote_inbox
            .as_ref()
            .map_or_else(Vec::new, |inbox| inbox.texts.clone())
            .into_iter()
            .map(|text| QueueEntry {
                text: Cow::Owned(text),
                color,
            })
            .collect()
    }

    pub(super) fn pop_active_queue(&mut self) {
        if self.is_main_chat() {
            return self.queue.remove(0);
        }
        let chat = &self.chats[self.active_chat];
        if chat.remote_inbox.is_some() {
            let id = chat.task_id().unwrap().to_string();
            let result = self
                .remote_link
                .as_ref()
                .ok_or(NO_INBOX_ERR.to_owned())
                .and_then(|link| {
                    link.request(|session_id| SessionOperation::TaskPop {
                        session_id,
                        id: id.clone(),
                    })
                });
            match result {
                Ok(snapshot) => self.update_task_queue(&id, snapshot),
                Err(error) => self.flash(error),
            }
        }
    }

    /// Deferred path: the agent is busy, so park the message and let
    /// `QueueItemConsumed` draw it once the agent picks it up. Returns
    /// false when there is no shared queue, meaning the message was dropped.
    pub(super) fn queue_and_notify(&mut self, msg: QueuedMessage) -> bool {
        let Some(ref shared) = self.queue.shared else {
            return false;
        };
        let input = self.build_agent_input(&msg);
        shared.push(QueueItem::Message(QueuedInput {
            text: msg.text,
            input,
            run_id: self.run_id,
            displayed: false,
        }));
        true
    }

    pub(super) fn restore_queue(&mut self, queued: Vec<String>) {
        for text in queued {
            self.queue_and_notify(QueuedMessage {
                text,
                images: Vec::new(),
            });
        }
    }

    pub(super) fn queue_compact(&mut self, instructions: Option<String>) {
        let Some(ref shared) = self.queue.shared else {
            return;
        };
        shared.push(QueueItem::Compact(Compaction {
            run_id: self.run_id,
            instructions,
            workflow: self.state.workflow,
        }));
    }

    /// Agent reached a deferred message: time to draw the bubble. Restored
    /// queue items start runs without `start_run`, so this is where the app
    /// learns the agent is busy. Immediate-dispatch items skip this event,
    /// so no dedup needed.
    pub(super) fn on_queue_item_consumed(&mut self, text: String, images: Vec<ImageSource>) {
        self.status = Status::Streaming;
        self.main_chat().show_user_message(text, images);
    }

    /// Immediate path: kick off the agent and draw the bubble in the same
    /// frame, so the user sees their message land where it will stay.
    pub(super) fn start_from_queue(&mut self, msg: &QueuedMessage) -> Vec<Action> {
        let input = self.build_agent_input(msg);
        self.start_run(input, msg.text.clone())
    }

    /// The one place a fresh run starts: every path that emits
    /// `Action::SendMessage` must go through here so `run_id` bumps exactly
    /// once per run.
    pub(super) fn start_run(&mut self, input: AgentInput, display: String) -> Vec<Action> {
        self.run_id += 1;
        self.clear_exit_request();
        // New work supersedes text held for recovery after an agent error.
        self.recoverable_queue.clear();
        self.status = Status::Streaming;
        self.fire_session_event(HostEvent::TurnStart { text: &display });
        if !display.is_empty() || !input.images.is_empty() {
            self.main_chat()
                .show_user_message(display, input.images.clone());
        }
        vec![Action::SendMessage(Box::new(input))]
    }
}
