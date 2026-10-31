//! Draws the history a remote agent replayed for a loaded session, through
//! the path a live run takes, so the rows read as they did live. Nothing here
//! starts a run, fires a hook or writes the local store.

use std::sync::Arc;

use agent_client_protocol_schema::v1::{
    ContentBlock, ContentChunk, ImageContent, SessionUpdate, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields,
};
use maki_types::event::AgentEvent;
use maki_types::message::{ImageMediaType, ImageSource};
use maki_wire::display::DisplayMapper;
use maki_wire::extensions::Agreed;
use maki_wire::usage::{self, SessionUsage};

use crate::chat::Chat;
use crate::components::{DisplayMessage, DisplayRole};

/// The user's turn being gathered: a prompt replays as a text chunk and
/// one chunk per image, which a typed prompt draws as one bubble.
#[derive(Default)]
struct UserTurn {
    text: String,
    images: Vec<ImageSource>,
}

impl UserTurn {
    fn push(&mut self, content: ContentBlock) {
        match content {
            ContentBlock::Text(text) => self.text.push_str(&text.text),
            ContentBlock::Image(image) => self.images.extend(image_source(image)),
            _ => {}
        }
    }

    fn flush(&mut self, chat: &mut Chat) {
        if self.text.is_empty() && self.images.is_empty() {
            return;
        }
        let turn = std::mem::take(self);
        chat.show_user_message(turn.text, turn.images);
    }
}

/// The session usage the replay ends on, which seeds the tab's usage slot.
pub(crate) fn seed(replay: &[SessionUpdate], extensions: &Agreed) -> Option<SessionUsage> {
    replay.iter().rev().find_map(|update| match update {
        SessionUpdate::UsageUpdate(update) => {
            let wire = extensions
                .usage
                .then(|| usage::read(update.meta.as_ref()))
                .flatten()
                .and_then(Result::ok);
            Some(SessionUsage::from_update(update, wire.as_ref()))
        }
        _ => None,
    })
}

pub(crate) fn draw(chat: &mut Chat, replay: Vec<SessionUpdate>, extensions: &Agreed) {
    let mut mapper = DisplayMapper::new(extensions);
    let mut user = UserTurn::default();
    let mut open = Vec::new();
    let mut thinking = false;
    for update in replay {
        // Live, thinking leaves pending text unflushed, because a turn
        // thinks before it speaks. History can speak first, so pending text
        // is drawn before the thinking that follows it. Only these two
        // chunks move the flag: an update that draws nothing keeps it.
        match update {
            SessionUpdate::AgentThoughtChunk(_) if !thinking => {
                chat.flush();
                thinking = true;
            }
            SessionUpdate::AgentMessageChunk(_) => thinking = false,
            _ => {}
        }
        match update {
            SessionUpdate::UserMessageChunk(chunk) => user.push(chunk.content),
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Image(image),
                ..
            }) => {
                user.flush(chat);
                chat.flush();
                chat.push(DisplayMessage::with_images(
                    DisplayRole::Assistant,
                    String::new(),
                    image_source(image).into_iter().collect(),
                ));
            }
            update => {
                user.flush(chat);
                for event in mapper.map(update) {
                    track(&mut open, &event);
                    chat.handle_event(event);
                }
            }
        }
    }
    user.flush(chat);
    // A call the history left without a result is drawn finished with no
    // output, as the local load draws it.
    for id in open {
        let done = ToolCallUpdate::new(
            id,
            ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
        );
        for event in mapper.map(SessionUpdate::ToolCallUpdate(done)) {
            chat.handle_event(event);
        }
    }
    chat.flush();
    chat.forget_times();
}

fn track(open: &mut Vec<String>, event: &AgentEvent) {
    match event {
        AgentEvent::ToolPending { id, .. } => open.push(id.clone()),
        AgentEvent::ToolDone(done) => open.retain(|id| *id != done.id),
        _ => {}
    }
}

fn image_source(image: ImageContent) -> Option<ImageSource> {
    let media = ImageMediaType::from_mime(&image.mime_type)?;
    Some(ImageSource::new(media, Arc::from(image.data)))
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::{
        TextContent, ToolCall, ToolCallContent, ToolCallId, UsageUpdate,
    };
    use maki_config::UiConfig;
    use maki_storage::id::MakiId;
    use test_case::test_case;

    use super::*;
    use crate::components::ToolStatus;

    const PROMPT: &str = "what changed";
    const REPLY: &str = "the build is green";
    const THOUGHT: &str = "check the log";
    const CALL_ID: &str = "tu-1";
    const OPEN_CALL_ID: &str = "tu-2";
    const TOOL: &str = "bash";
    const HEADER: &str = "ls -la";
    const RESULT: &str = "file.rs";
    const IMAGE_DATA: &str = "b64data";
    const PNG: &str = "image/png";
    const USED: u64 = 1_234;
    const WINDOW: u64 = 200_000;
    const LIST_COST: f64 = 0.5;

    fn chat() -> Chat {
        Chat::new(
            MakiId::generate(),
            None,
            "Main".into(),
            UiConfig::default(),
            crate::EventHandle::disconnected_for_test(),
        )
    }

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text(TextContent::new(text))
    }

    fn image() -> ContentBlock {
        ContentBlock::Image(ImageContent::new(IMAGE_DATA, PNG))
    }

    fn call(id: &str) -> [SessionUpdate; 2] {
        [
            SessionUpdate::ToolCall(
                ToolCall::new(ToolCallId::from(id.to_owned()), TOOL)
                    .status(ToolCallStatus::Pending),
            ),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                id.to_owned(),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::InProgress)
                    .title(HEADER),
            )),
        ]
    }

    fn result(id: &str) -> SessionUpdate {
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id.to_owned(),
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![ToolCallContent::from(RESULT)]),
        ))
    }

    fn messages(chat: &Chat) -> Vec<&DisplayMessage> {
        (0..chat.message_count())
            .filter_map(|index| chat.message_at(index))
            .collect()
    }

    fn tool_status(message: &DisplayMessage) -> Option<ToolStatus> {
        match &message.role {
            DisplayRole::Tool(tool) => Some(tool.status),
            _ => None,
        }
    }

    #[test]
    fn a_conversation_draws_in_order_with_its_tool_rows_finished() {
        let mut chat = chat();
        let [pending, header] = call(CALL_ID);
        let [open_pending, open_header] = call(OPEN_CALL_ID);
        let replay = vec![
            SessionUpdate::UserMessageChunk(ContentChunk::new(text(PROMPT))),
            SessionUpdate::UserMessageChunk(ContentChunk::new(image())),
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(text(THOUGHT))),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(text(REPLY))),
            pending,
            header,
            result(CALL_ID),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(image())),
            open_pending,
            open_header,
        ];

        draw(&mut chat, replay, &Agreed::default());

        let shown = messages(&chat);
        assert_eq!(shown[0].role, DisplayRole::User);
        assert_eq!(shown[0].text, PROMPT);
        assert_eq!(shown[0].images.len(), 1);
        assert_eq!(shown[1].role, DisplayRole::Thinking);
        assert_eq!(shown[1].text, THOUGHT);
        assert_eq!(shown[2].role, DisplayRole::Assistant);
        assert_eq!(shown[2].text, REPLY);
        assert_eq!(tool_status(shown[3]), Some(ToolStatus::Success));
        assert_eq!(shown[3].text, format!("{HEADER}\n{RESULT}"));
        assert_eq!(shown[4].role, DisplayRole::Assistant);
        assert_eq!(shown[4].images.len(), 1);
        assert_eq!(tool_status(shown[5]), Some(ToolStatus::Success));
        assert_eq!(shown.len(), 6);
        assert!(shown.iter().all(|message| message.timestamp.is_none()));
        assert_eq!(chat.in_progress_count(), 0);
    }

    #[test_case(None ; "adjacent")]
    #[test_case(Some(SessionUpdate::UsageUpdate(UsageUpdate::new(USED, WINDOW))) ; "with_a_usage_update_between")]
    fn text_then_thinking_then_text_keeps_its_order(between: Option<SessionUpdate>) {
        let mut chat = chat();
        let mut replay = vec![SessionUpdate::AgentMessageChunk(ContentChunk::new(text(
            PROMPT,
        )))];
        replay.extend(between);
        replay.extend([
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(text(THOUGHT))),
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(text(THOUGHT))),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(text(REPLY))),
        ]);

        draw(&mut chat, replay, &Agreed::default());

        let shown: Vec<_> = messages(&chat)
            .into_iter()
            .map(|message| (message.role.clone(), message.text.clone()))
            .collect();
        assert_eq!(
            shown,
            [
                (DisplayRole::Assistant, PROMPT.to_owned()),
                (DisplayRole::Thinking, THOUGHT.repeat(2)),
                (DisplayRole::Assistant, REPLY.to_owned()),
            ]
        );
    }

    #[test]
    fn the_last_usage_update_seeds_the_session_usage() {
        let wire = usage::WireUsage {
            list_cost: Some(LIST_COST),
            ..usage::WireUsage::default()
        };
        let replay = vec![
            SessionUpdate::UsageUpdate(UsageUpdate::new(1, WINDOW)),
            usage::attach(
                SessionUpdate::UsageUpdate(UsageUpdate::new(USED, WINDOW)),
                &wire,
            ),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(text(REPLY))),
        ];
        let agreed = Agreed {
            usage: true,
            ..Agreed::default()
        };

        let seeded = seed(&replay, &agreed).expect("the replay reports its usage");

        assert_eq!((seeded.used, seeded.window), (USED as u32, WINDOW as u32));
        assert_eq!(seeded.list_cost, Some(LIST_COST));
        assert_eq!(seed(&replay, &Agreed::default()).unwrap().list_cost, None);
    }
}
