//! The settings extension and the ids of the four setting options.
//! Thinking, fast and workflow ride on each prompt, as a local `AgentInput`
//! carries them, so the agent clamps them to the model it runs. Yolo is a
//! permission policy, so it is only an option, applied at once.

use agent_client_protocol_schema::v1::Meta;
use maki_storage::sessions::StoredThinking;
use maki_types::thinking::ThinkingConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extensions::META_KEY;

pub const SETTINGS_KEY: &str = "settings";
pub const SETTINGS_VERSION: u64 = 1;
pub const THINKING_CONFIG_ID: &str = "thinking";
pub const FAST_CONFIG_ID: &str = "fast";
pub const WORKFLOW_CONFIG_ID: &str = "workflow";
pub const YOLO_CONFIG_ID: &str = "yolo";

/// A prompt's settings on the wire. Thinking is a `StoredThinking` setting
/// string, the form `/thinking` and the config file use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireSettings {
    pub thinking: String,
    pub fast: bool,
    pub workflow: bool,
}

impl WireSettings {
    pub fn new(thinking: ThinkingConfig, fast: bool, workflow: bool) -> Self {
        Self {
            thinking: thinking.to_string(),
            fast,
            workflow,
        }
    }

    pub fn thinking(&self) -> Result<StoredThinking, String> {
        StoredThinking::parse_setting(&self.thinking).map_err(|e| e.to_string())
    }

    pub fn meta(&self) -> Meta {
        let mut maki = Meta::new();
        maki.insert(
            SETTINGS_KEY.into(),
            serde_json::to_value(self).unwrap_or_default(),
        );
        Meta::from_iter([(META_KEY.to_owned(), Value::Object(maki))])
    }
}

/// `None` when the prompt carries no extension.
pub fn read(meta: Option<&Meta>) -> Option<Result<WireSettings, String>> {
    let value = meta?.get(META_KEY)?.get(SETTINGS_KEY)?;
    Some(WireSettings::deserialize(value).map_err(|e| format!("unreadable settings: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_settings_round_trip_through_the_meta() {
        let wire = WireSettings::new(ThinkingConfig::Adaptive, true, false);
        assert_eq!(read(Some(&wire.meta())).unwrap().unwrap(), wire);
        assert_eq!(wire.thinking().unwrap(), StoredThinking::Adaptive);
        assert!(read(None).is_none());
    }
}
