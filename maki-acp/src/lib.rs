pub mod config_options;
pub mod permissions;
pub mod server;
pub mod translate;

use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;
use maki_agent::headless::SessionPlugins;
use maki_agent::permissions::PluginRuleStore;
use maki_agent::prompt::ResolvedPrompts;
use maki_agent::session::StoredSession;
use maki_agent::{AgentConfig, CancelToken, SessionEndReason};
use maki_config::project::TrustMode;
use maki_config::{ModelPolicy, SessionDefaults, TrustConfig};
use maki_providers::Timeouts;
use maki_providers::model::Model;
use maki_storage::StateDir;
use maki_storage::id::MakiId;
use maki_storage::sessions::SessionClaim;
use maki_wire::plugins::{PluginReply, PluginRequest};
use tokio::runtime::Handle;

pub type SessionEndHook =
    Arc<dyn Fn(MakiId, SessionEndReason) -> BoxFuture<'static, ()> + Send + Sync>;

pub type PluginAdmin =
    Arc<dyn Fn(PluginRequest, AcpParams) -> Result<(AcpParams, PluginReply), String> + Send + Sync>;

#[derive(Clone)]
pub struct AcpParams {
    pub initial: Option<(StoredSession, SessionClaim)>,
    pub administration: Option<PluginAdmin>,
    pub plugins: Option<Arc<dyn SessionPlugins>>,
    pub model: Model,
    pub config: AgentConfig,
    pub timeouts: Timeouts,
    pub initial_wd: PathBuf,
    pub prompt_slots: Arc<ResolvedPrompts>,
    pub yolo: bool,
    /// ACP exposes no toggles of its own, so the `always_*` knobs are the whole
    /// answer for every prompt this server runs.
    pub defaults: SessionDefaults,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    /// How a session cwd with no stored trust decision is treated. `--trust`
    /// makes it [`TrustMode::Session`], which covers every cwd the client picks.
    pub trust_mode: TrustMode,
    /// Folders the global `init.lua` answered for in advance. ACP never asks,
    /// so this is the only yes a cwd with no stored decision can get.
    pub trust_policy: Arc<TrustConfig>,
    /// Called with the reason when an ACP session is replaced or the server
    /// exits. The hook answers with a future so its wait rides the executor
    /// instead of holding stdin for the whole `SessionEnd` grace period.
    pub on_session_end: Option<SessionEndHook>,
    pub storage: StateDir,
    /// `false` keeps every tool call blocking. Otherwise calls can move to the
    /// background and their results ride the next prompt. The server is long
    /// lived, so it never waits for them: closing the session stops them.
    pub background: bool,
    /// Firing it shuts the server down as end of input does, without waiting
    /// for the client to close stdin.
    pub stop: CancelToken,
}

pub fn run(params: AcpParams) -> color_eyre::Result<()> {
    Handle::current().block_on(server::serve(params))
}
