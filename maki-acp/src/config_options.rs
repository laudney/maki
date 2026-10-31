//! The four setting options: they publish the session's intent and its
//! effective labels, and set them for any client.

use agent_client_protocol_schema::v1::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
};
use maki_agent::session::SessionSettings;
use maki_providers::model::Model;
use maki_providers::{ModelThinking, ThinkingConfig};
use maki_storage::sessions::StoredThinking;
use maki_wire::settings::{FAST_CONFIG_ID, THINKING_CONFIG_ID, WORKFLOW_CONFIG_ID, YOLO_CONFIG_ID};

const OFF_LABEL: &str = "off";

/// What a model makes of a thinking value, as the local status bar shows it.
fn effective_label(thinking: ThinkingConfig, model: &Model) -> String {
    thinking
        .clamped(model)
        .shown(model)
        .status_label()
        .map_or_else(|| OFF_LABEL.to_owned(), |label| label.into_owned())
}

/// The session's intent, with each thinking value named for what the model
/// makes of it. Fast is offered only when the model can do it, as `/fast`
/// refuses it locally.
pub fn options(model: &Model, settings: &SessionSettings, yolo: bool) -> Vec<SessionConfigOption> {
    let current = ThinkingConfig::from(settings.thinking);
    let current_value = current.to_string();
    let mut values: Vec<String> = model
        .thinking_options()
        .into_iter()
        .map(|option| option.name.to_owned())
        .collect();
    if !values.contains(&current_value) {
        values.push(current_value.clone());
    }
    let choices: Vec<SessionConfigSelectOption> = values
        .into_iter()
        .map(|value| {
            let thinking = StoredThinking::parse_setting(&value)
                .map(ThinkingConfig::from)
                .unwrap_or_default();
            SessionConfigSelectOption::new(value, effective_label(thinking, model))
        })
        .collect();
    let thinking =
        SessionConfigOption::select(THINKING_CONFIG_ID, "Thinking", current_value, choices)
            .category(SessionConfigOptionCategory::ThoughtLevel);
    let fast = model
        .supports_fast()
        .then(|| SessionConfigOption::boolean(FAST_CONFIG_ID, "Fast", settings.fast));
    [thinking]
        .into_iter()
        .chain(fast)
        .chain([
            SessionConfigOption::boolean(WORKFLOW_CONFIG_ID, "Workflow", settings.workflow),
            SessionConfigOption::boolean(YOLO_CONFIG_ID, "Yolo", yolo),
        ])
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A model that thinks and cannot go fast; the test asserts both.
    const THINKING_SPEC: &str = "anthropic/claude-sonnet-4-20250514";

    fn ids(options: &[SessionConfigOption]) -> Vec<String> {
        options.iter().map(|option| option.id.to_string()).collect()
    }

    #[test]
    fn options_name_each_thinking_value_and_offer_fast_only_when_supported() {
        let model = Model::from_spec(THINKING_SPEC).unwrap();
        assert!(model.supports_thinking() && !model.supports_fast());
        let settings = SessionSettings {
            thinking: Some(StoredThinking::Budget { tokens: 1234 }),
            ..SessionSettings::default()
        };
        let options = options(&model, &settings, true);
        assert_eq!(ids(&options), ["thinking", "workflow", "yolo"]);
        let json = serde_json::to_value(&options[0]).unwrap();
        assert_eq!(json["currentValue"], "1234");
        assert_eq!(json["category"], "thought_level");
        let values = json["options"].as_array().unwrap();
        assert!(values.iter().any(|value| value["value"] == "off"));
        assert_eq!(
            values.last().unwrap(),
            &json!({"value": "1234", "name": "1234 tokens"})
        );
        assert_eq!(
            serde_json::to_value(&options[2]).unwrap()["currentValue"],
            true
        );
    }
}
