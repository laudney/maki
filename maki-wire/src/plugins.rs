use serde::{Deserialize, Serialize};

pub const KEY: &str = "plugins";
pub const VERSION: u64 = 1;
pub const METHOD: &str = "_maki/plugins";
pub const UNSUPPORTED: &str = "Plugin administration is not supported by this agent";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum PluginOperation {
    Reload,
    Update { args: String, bang: bool },
    Delete { args: String, bang: bool },
    Trust,
}

impl PluginOperation {
    pub fn command(&self) -> Option<(&str, &str, bool)> {
        match self {
            Self::Update { args, bang } => Some(("/packupdate", args, *bang)),
            Self::Delete { args, bang } => Some(("/packdel", args, *bang)),
            Self::Reload | Self::Trust => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRequest {
    #[serde(flatten)]
    pub operation: PluginOperation,
    #[serde(default)]
    pub review: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", content = "message", rename_all = "snake_case")]
pub enum PluginReply {
    Review(String),
    Complete(String),
}
