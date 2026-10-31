use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::{ArcSwap, Guard};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpServerStatus {
    Connecting,
    Running,
    Disabled,
    Failed(String),
    NeedsAuth {
        #[serde(skip)]
        url: Option<String>,
    },
}

impl McpServerStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Running | Self::Connecting)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    pub transport_kind: String,
    pub tool_count: usize,
    pub prompt_count: usize,
    pub status: McpServerStatus,
    #[serde(skip)]
    pub config_path: PathBuf,
    #[serde(skip)]
    pub url: Option<String>,
    #[serde(skip)]
    pub oauth: Option<OauthClientConfig>,
    #[serde(skip)]
    pub ca_file: Option<PathBuf>,
}

/// Without `client_id` the client is registered dynamically, so the
/// `callback_*` fields still shape the redirect URI.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct OauthClientConfig {
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Fixed loopback port so the redirect URI can be pre-registered.
    #[serde(default)]
    pub callback_port: Option<u16>,
    /// Loopback path of the redirect URI (defaults to `/mcp/oauth/callback`).
    #[serde(default)]
    pub callback_path: Option<String>,
    /// Loopback hostname of the redirect URI (defaults to `127.0.0.1`).
    #[serde(default)]
    pub callback_hostname: Option<String>,
    /// `https` is for providers that refuse http redirect URIs. Maki has no TLS
    /// listener, so the user pastes the redirect URL from the browser instead.
    #[serde(default)]
    pub callback_scheme: CallbackScheme,
}

#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CallbackScheme {
    #[default]
    Http,
    Https,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct McpPromptInfo {
    pub display_name: String,
    pub qualified_name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArg>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct McpPromptArg {
    pub name: String,
    pub description: String,
    pub required: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct McpSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
    pub infos: Vec<McpServerInfo>,
    pub prompts: Vec<McpPromptInfo>,
    #[serde(skip)]
    pub pids: Vec<u32>,
    pub generation: u64,
}

/// Read-only view of the latest published `McpSnapshot`. Handing this out instead of the
/// raw `ArcSwap` keeps outside code from publishing snapshots of its own.
#[derive(Clone)]
pub struct McpSnapshotReader(Arc<ArcSwap<McpSnapshot>>);

impl McpSnapshotReader {
    pub fn over(snapshot: Arc<ArcSwap<McpSnapshot>>) -> Self {
        Self(snapshot)
    }

    pub fn empty() -> Self {
        Self::from_snapshot(McpSnapshot::default())
    }

    pub fn from_snapshot(snapshot: McpSnapshot) -> Self {
        Self(Arc::new(ArcSwap::from_pointee(snapshot)))
    }

    pub fn load(&self) -> Guard<Arc<McpSnapshot>> {
        self.0.load()
    }

    pub fn load_full(&self) -> Arc<McpSnapshot> {
        self.0.load_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_status_contains_display_data_without_host_authority() {
        let snapshot = McpSnapshot {
            infos: vec![McpServerInfo {
                name: "files".into(),
                transport_kind: "stdio".into(),
                tool_count: 2,
                prompt_count: 0,
                status: McpServerStatus::Running,
                config_path: PathBuf::new(),
                url: None,
                ca_file: None,
                oauth: Some(OauthClientConfig::default()),
            }],
            pids: vec![42],
            ..McpSnapshot::default()
        };
        let value = serde_json::to_value(snapshot).unwrap();
        assert!(value.get("pids").is_none());
        assert!(value["infos"][0].get("oauth").is_none());
        assert!(value["infos"][0].get("url").is_none());
        assert!(value["infos"][0].get("config_path").is_none());
        let restored: McpSnapshot = serde_json::from_value(value).unwrap();
        assert!(restored.pids.is_empty());
        assert!(restored.infos[0].oauth.is_none());
        assert_eq!(restored.infos[0].tool_count, 2);
        assert!(restored.infos[0].status.is_active());
    }
}
