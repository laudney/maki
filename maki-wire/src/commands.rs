//! Agent commands. The agent advertises them in `available_commands_update`,
//! and a prompt whose only content is `/<name> [input]` runs one, which any
//! ACP client that shows agent commands can send.

use agent_client_protocol_schema::v1::{
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, ContentBlock, Meta,
    SessionUpdate, TextContent, UnstructuredCommandInput,
};
use serde_json::Value;

use crate::extensions::META_KEY;

pub const COMPACT: &str = "compact";
const COMPACT_DESCRIPTION: &str = "Summarize and compact conversation history (optional guidance)";
const COMPACT_HINT: &str = "instructions";
const PREFIX: char = '/';
/// On a compaction's `in_progress`: the user asked for it, so a client
/// draws no automatic line for it.
const MANUAL_KEY: &str = "manual";

pub fn advertised() -> SessionUpdate {
    let compact = AvailableCommand::new(COMPACT, COMPACT_DESCRIPTION).input(
        AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(COMPACT_HINT)),
    );
    SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![compact]))
}

/// The instructions of a prompt that is `/compact` alone, `None` for any
/// other prompt.
pub fn compaction(prompt: &[ContentBlock]) -> Option<Option<String>> {
    let [ContentBlock::Text(text)] = prompt else {
        return None;
    };
    let rest = text
        .text
        .trim()
        .strip_prefix(PREFIX)?
        .strip_prefix(COMPACT)?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let instructions = rest.trim();
    Some((!instructions.is_empty()).then(|| instructions.to_owned()))
}

pub fn compaction_prompt(instructions: Option<&str>) -> Vec<ContentBlock> {
    let text = match instructions {
        Some(instructions) => format!("{PREFIX}{COMPACT} {instructions}"),
        None => format!("{PREFIX}{COMPACT}"),
    };
    vec![ContentBlock::Text(TextContent::new(text))]
}

pub fn manual_meta() -> Meta {
    Meta::from_iter([(
        META_KEY.to_owned(),
        Value::Object(serde_json::Map::from_iter([(
            MANUAL_KEY.to_owned(),
            Value::Bool(true),
        )])),
    )])
}

pub fn is_manual(meta: &Meta) -> bool {
    meta.get(META_KEY)
        .and_then(|maki| maki.get(MANUAL_KEY))
        .and_then(Value::as_bool)
        .unwrap_or_default()
}

pub fn offers_compaction(update: &AvailableCommandsUpdate) -> bool {
    update
        .available_commands
        .iter()
        .any(|command| command.name == COMPACT)
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::{ImageContent, ResourceLink};
    use test_case::test_case;

    use super::*;

    const GUIDANCE: &str = "keep the API notes";

    fn text(text: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(TextContent::new(text))]
    }

    #[test_case("/compact", Some(None) ; "bare")]
    #[test_case("  /compact  ", Some(None) ; "padded")]
    #[test_case("/compact keep the API notes", Some(Some(GUIDANCE)) ; "with_instructions")]
    #[test_case("/compact\nkeep the API notes", Some(Some(GUIDANCE)) ; "instructions_on_the_next_line")]
    #[test_case("/compaction", None ; "a_longer_word")]
    #[test_case("please /compact", None ; "not_at_the_start")]
    #[test_case("compact", None ; "no_slash")]
    fn a_text_prompt_is_a_compaction_only_when_it_names_one(
        prompt: &str,
        expected: Option<Option<&str>>,
    ) {
        assert_eq!(
            compaction(&text(prompt)),
            expected.map(|instructions| instructions.map(str::to_owned))
        );
    }

    #[test]
    fn a_prompt_with_more_than_the_command_is_a_prompt() {
        let mut prompt = text("/compact");
        prompt.push(ContentBlock::Image(ImageContent::new("data", "image/png")));
        assert_eq!(compaction(&prompt), None);
        let link = vec![ContentBlock::ResourceLink(ResourceLink::new(
            "compact",
            "file:///compact",
        ))];
        assert_eq!(compaction(&link), None);
    }

    #[test_case(None ; "bare")]
    #[test_case(Some(GUIDANCE) ; "with_instructions")]
    fn the_clients_prompt_reads_back_as_its_compaction(instructions: Option<&str>) {
        assert_eq!(
            compaction(&compaction_prompt(instructions)),
            Some(instructions.map(str::to_owned))
        );
    }

    #[test]
    fn the_manual_mark_reads_back() {
        assert!(is_manual(&manual_meta()));
        assert!(!is_manual(&Meta::new()));
    }

    #[test]
    fn the_advertisement_offers_compaction() {
        let SessionUpdate::AvailableCommandsUpdate(update) = advertised() else {
            panic!("not a command list");
        };
        assert!(offers_compaction(&update));
        assert!(!offers_compaction(
            &AvailableCommandsUpdate::new(Vec::new())
        ));
    }
}
