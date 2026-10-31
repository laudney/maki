mod api;
mod background;
mod domains;
mod hook;
mod loader;
mod provider_adapter;
pub mod session_snapshot;

pub use maki_lua_core::handle::CoreHandle;
pub use maki_lua_core::{agent_autocmd, chunks, docs, docs_render, language};
use maki_lua_core::{bundled, domain, error, pack, plugin_permissions, provider_hooks, runtime};

pub use api::r#fn::set_shell_preference;
pub use api::net::set_allowed_private_hosts;
pub use api::options::{OptionSpec, OptionType, PluginOptionSpecs};
pub use api::pack::Declared;
pub use api::provider::set_terminal_prompt;
pub use bundled::Bundled;
pub use docs::{DocKind, FnDoc, ModuleDoc, ParamDoc};
pub use domains::agent::moves::MoveOutcome;
pub use domains::api_docs;
pub use domains::facade::SessionSnapshotFn;
pub use error::PluginError;
pub use loader::{
    AgentHandle, InitFiles, PERMISSION_NAME_WARNING, PluginHost, SKIPPED_PLUGIN_WARNING, bundled,
    bundled_grant, discover, discover_installed, install_declared,
};
pub use maki_lua_core::api::fs::publish_walks;
pub use maki_types::event::SessionEndReason;
pub use pack::{
    DeleteTarget, DiscoveredPackage, Discovery, InstallReport, Interaction, MANAGED_GROUP, Origin,
    PackCommand, PackContext, PackPlan, PackPreparation, PackReport, PlannedOp, UpdateOptions,
    UpdateTarget, apply_pack_plan, approvals_path, installed_names, lockfile_path,
    prepare_pack_command, sanitize_message, site_dir,
};
pub use plugin_permissions::{
    Component, Missing, PackageRequests, Permission, PluginPermissions, PluginReach, Requested,
    SENDS_PROMPT,
};
pub use runtime::{KILL_GRACE, MAX_INFLIGHT_TOOLS, SLICE_WARN};
pub use session_snapshot::{SessionQueueSnapshot, SessionSnapshot};

pub mod test_support {

    use std::process::Command;
    use std::sync::Arc;

    pub use crate::api::autocmd::content_omitted_message;
    pub use crate::api::util::dispatch::MAX_HOOK_DEPTH;
    use crate::plugin_permissions::NetEgress;
    use crate::provider_hooks;

    pub use crate::runtime::layer_skipped_message;
    use url::Url;

    #[cfg(any(test, feature = "test-support"))]
    pub fn runtime() -> &'static tokio::runtime::Runtime {
        &crate::loader::HOST_RUNTIME
    }

    /// An egress that declared no host of its own and owns provider {slug}.
    fn provider_owner(slug: &str) -> NetEgress {
        let egress = NetEgress::new(Some(Arc::from([])));
        egress.owns(slug.to_owned());
        egress
    }

    /// Whether a plugin owning provider {slug} reaches {url} as that
    /// provider's origin, under the provider hooks this process installed.
    pub fn owner_vouches(slug: &str, url: &Url) -> bool {
        provider_owner(slug).vouches(url)
    }

    /// Whether a plugin owning provider {slug}, and declaring no host, may
    /// reach `host:port`, under the provider hooks this process installed.
    pub fn owner_allows(slug: &str, host: &str, port: u16) -> bool {
        provider_owner(slug).allows(host, port)
    }

    pub fn sanitize_child_env(command: &mut Command) {
        provider_hooks::sanitize_child_env(command);
    }
}
