//! A prompt's size, estimated from message bytes.

use serde_json::Value;
use std::io;

use crate::message::{ContentBlock, Message, is_deferred_tool};

#[doc(hidden)]
pub const CHARS_PER_TOKEN: usize = 4;
/// Flat per image, because all we have is the encoded blob, and its size
/// tracks compression rather than the tile count the provider bills. A
/// screenshot at the sizes `adapt_images_for_model` allows lands near
/// this. Counting nothing understated exactly the transcripts most likely to
/// overflow.
const TOKENS_PER_IMAGE: u32 = 1_500;

/// Counts message content only. The system prompt and the tool schemas, a five
/// figure baseline on a full tool set, are not in here, so this must never
/// replace a context size the provider measured.
pub fn estimate_message_tokens(messages: &[Message]) -> u32 {
    if messages.is_empty() {
        return 0;
    }
    let (total_bytes, images) =
        messages
            .iter()
            .flat_map(|m| &m.content)
            .fold((0usize, 0u32), |(bytes, images), block| match block {
                ContentBlock::Text { text } => (bytes + text.len(), images),
                ContentBlock::ToolResult { content, .. } => (bytes + content.len(), images),
                ContentBlock::ToolUse { input, .. } => (bytes + json_len(input), images),
                ContentBlock::Thinking { thinking, .. } => (bytes + thinking.len(), images),
                ContentBlock::Image { .. } => (bytes, images + 1),
                ContentBlock::RedactedThinking { .. } | ContentBlock::Unknown(_) => (bytes, images),
            });
    (total_bytes.max(CHARS_PER_TOKEN) / CHARS_PER_TOKEN) as u32 + images * TOKENS_PER_IMAGE
}

/// [`estimate_message_tokens`] plus the system prompt and the serialized tool
/// schemas, which a server checking `prompt + max_tokens <= context_window`
/// counts too. A deferred definition travels in the request but not in the
/// context, so it is not counted.
pub fn estimate_prompt_tokens(messages: &[Message], system: &str, tools: &Value) -> u32 {
    let tools_len = match tools.as_array() {
        Some(arr) => arr
            .iter()
            .filter(|t| !is_deferred_tool(t))
            .map(json_len)
            .sum(),
        None => json_len(tools),
    };
    let overhead = (system.len() + tools_len) / CHARS_PER_TOKEN;
    estimate_message_tokens(messages).saturating_add(overhead as u32)
}

/// Serialized length without building the string, since this runs over the
/// whole transcript and the tool catalog before every request.
fn json_len(value: &Value) -> usize {
    struct Counter(usize);
    impl io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => counter.0,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::message::{DEFER_LOADING_KEY, ImageMediaType, ImageSource, Role};

    const TEXT_BYTES: usize = 4_000;
    const TEXT_TOKENS: u32 = (TEXT_BYTES / CHARS_PER_TOKEN) as u32;

    fn text_message(len: usize) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "x".repeat(len),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn the_prompt_estimate_covers_the_system_prompt_and_the_tool_schemas() {
        let messages = [text_message(TEXT_BYTES)];
        let system = "s".repeat(TEXT_BYTES);
        assert!(
            estimate_prompt_tokens(&messages, &system, &json!([{"name": "read"}]))
                > estimate_message_tokens(&messages) + TEXT_TOKENS
        );
    }

    #[test]
    fn deferred_tool_schemas_are_not_counted_in_the_prompt_estimate() {
        let messages = [text_message(TEXT_BYTES)];
        let schema = json!({"description": "d".repeat(TEXT_BYTES)});
        let eager = json!([{"name": "srv__big", "input_schema": schema}]);
        let mut deferred = eager.clone();
        deferred[0][DEFER_LOADING_KEY] = json!(true);

        let without = estimate_prompt_tokens(&messages, "", &json!([]));
        assert_eq!(estimate_prompt_tokens(&messages, "", &deferred), without);
        assert!(estimate_prompt_tokens(&messages, "", &eager) > without);
    }

    #[test]
    fn images_are_charged_even_though_their_bytes_are_not() {
        let with_image = [Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                source: ImageSource::new(ImageMediaType::Png, "".into()),
            }],
            ..Default::default()
        }];
        let empty_text = [text_message(0)];
        assert_eq!(
            estimate_message_tokens(&with_image) - estimate_message_tokens(&empty_text),
            TOKENS_PER_IMAGE
        );
    }
}
