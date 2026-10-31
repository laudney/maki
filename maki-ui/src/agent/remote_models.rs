//! A remote agent's model menu as it published it. The values are the
//! agent's own and stay opaque: nothing here resolves one against this
//! machine's providers.

use agent_client_protocol_schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOptions,
};

const MODEL_CONFIG_ID: &str = "model";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModelMenu {
    pub(crate) config_id: String,
    pub(crate) current: Option<String>,
    pub(crate) choices: Vec<ModelChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelChoice {
    pub(crate) value: String,
    pub(crate) name: String,
}

/// A pick names the option it was made from, so it can never be sent
/// against an option the agent has since replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelPick {
    pub(crate) config_id: String,
    pub(crate) value: String,
}

impl ModelMenu {
    /// The select option the agent filed under the model category, else the
    /// one named `model`. Groups are flattened in the agent's order.
    pub(crate) fn from_options(options: &[SessionConfigOption]) -> Option<Self> {
        let option = options
            .iter()
            .find(|o| o.category == Some(SessionConfigOptionCategory::Model))
            .or_else(|| options.iter().find(|o| o.id.to_string() == MODEL_CONFIG_ID))?;
        let SessionConfigKind::Select(select) = &option.kind else {
            return None;
        };
        let flat: Vec<_> = match &select.options {
            SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect(),
            SessionConfigSelectOptions::Grouped(groups) => {
                groups.iter().flat_map(|group| &group.options).collect()
            }
            _ => Vec::new(),
        };
        Some(Self {
            config_id: option.id.to_string(),
            current: Some(select.current_value.to_string()),
            choices: flat
                .into_iter()
                .map(|choice| ModelChoice {
                    value: choice.value.to_string(),
                    name: choice.name.clone(),
                })
                .collect(),
        })
    }

    /// The name the agent gave its current model, or the value without one.
    pub(crate) fn current_name(&self) -> Option<&str> {
        let current = self.current.as_deref()?;
        Some(
            self.choices
                .iter()
                .find(|choice| choice.value == current)
                .map_or(current, |choice| choice.name.as_str()),
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;

    const FIRST: &str = "vendor/first";
    const SECOND: &str = "vendor/second";
    const FIRST_NAME: &str = "First model";

    fn options(value: Value) -> Vec<SessionConfigOption> {
        serde_json::from_value(value).unwrap()
    }

    fn select(id: &str, category: Option<&str>, options: Value) -> Value {
        let mut option = json!({
            "id": id, "name": "Model", "type": "select",
            "currentValue": FIRST, "options": options,
        });
        if let Some(category) = category {
            option["category"] = json!(category);
        }
        option
    }

    fn flat() -> Value {
        json!([{"value": FIRST, "name": FIRST_NAME}, {"value": SECOND, "name": SECOND}])
    }

    fn values(menu: &ModelMenu) -> Vec<&str> {
        menu.choices.iter().map(|c| c.value.as_str()).collect()
    }

    #[test_case(json!([select("llm", Some("model"), flat())]), "llm" ; "by_category")]
    #[test_case(json!([select("model", None, flat())]), "model" ; "by_id")]
    #[test_case(json!([select("other", None, flat()), select("llm", Some("model"), flat())]), "llm" ; "category_wins")]
    fn finds_the_model_option(value: Value, config_id: &str) {
        let menu = ModelMenu::from_options(&options(value)).unwrap();
        assert_eq!(menu.config_id, config_id);
        assert_eq!(menu.current.as_deref(), Some(FIRST));
        assert_eq!(values(&menu), [FIRST, SECOND]);
        assert_eq!(menu.current_name(), Some(FIRST_NAME));
    }

    #[test]
    fn groups_flatten_in_order() {
        let grouped = json!([
            {"group": "a", "name": "A", "options": [{"value": FIRST, "name": FIRST_NAME}]},
            {"group": "b", "name": "B", "options": [{"value": SECOND, "name": SECOND}]},
        ]);
        let menu =
            ModelMenu::from_options(&options(json!([select("model", None, grouped)]))).unwrap();
        assert_eq!(values(&menu), [FIRST, SECOND]);
    }

    #[test_case(json!([]) ; "no_options")]
    #[test_case(json!([select("thinking", None, flat())]) ; "no_model_option")]
    fn no_model_option_is_no_menu(value: Value) {
        assert!(ModelMenu::from_options(&options(value)).is_none());
    }
}
