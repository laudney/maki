use std::any::Any;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
#[cfg(any(test, feature = "test-support"))]
use tokio::runtime::{EnterGuard, Handle, RuntimeFlavor};

use include_dir::include_dir;
use maki_agent::permissions::{PluginRuleStore, carries_builtin_defaults};
use maki_agent::tools::ToolRegistry;
use maki_config::{GatedFile, PluginFileConfig, PluginsConfig, ProjectConfig, RawConfig};
use maki_lua_core::bundled_plugins;
use maki_lua_core::handle::{CoreHandle, SHUTDOWN_TIMEOUT};
pub use maki_lua_core::host::SKIPPED_PLUGIN_WARNING;
use maki_lua_core::host::{ComponentPolicy, HostSteps, LoadKind};
use maki_providers::plugin::{DeclAuthority, keep_granted};

use crate::api::options::{PluginOptionSpecs, PluginOpts};
use crate::api::pack::Declared;
use crate::background::{BackgroundTasks, Stop};
use crate::bundled::Bundled;
use crate::domains::agent::CollectPromptSlots;
use crate::domains::facade::InstallSessionSnapshot;
use crate::domains::{self, AgentGrant, AgentHandles};
use crate::error::PluginError;
use crate::pack::{
    self, ComponentApprovals, DiscoveredPackage, Discovery, InstallReport, Interaction,
};
use crate::plugin_permissions::{
    Component, PluginPermissions, check_plugin_compatibility, load_plugin_granted_providers,
    load_plugin_permissions,
};
use crate::provider_adapter;
use crate::runtime::{
    self, ConfigScope, DomainGrant, LoadChunk, LoadContext, LoadStep, LuaThread, Request,
    RequestLanes,
};
use maki_agent::prompt::ResolvedPrompts;

#[cfg(any(test, feature = "test-support"))]
pub(crate) use maki_lua_core::host::HOST_RUNTIME;
/// A `#[tokio::test]` runtime is single-threaded and only advances while the
/// test awaits, so a host on it could not wake a sleeping Lua task while the
/// test thread blocks on the host; only a multi-thread runtime is kept.
#[cfg(any(test, feature = "test-support"))]
fn enter_test_runtime() -> Option<EnterGuard<'static>> {
    let on_workers = Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == RuntimeFlavor::MultiThread);
    (!on_workers).then(|| HOST_RUNTIME.enter())
}

#[cfg(not(any(test, feature = "test-support")))]
fn enter_test_runtime() {}

const USER_PLUGIN: &str = "user";
const INIT_LUA: &str = "init.lua";

/// Tests assert on this exact text, so a wording tweak here updates them too.
pub const PERMISSION_NAME_WARNING: &str = "inherits maki's permission rules for the builtin \
     tool of the same name, together with any \"always allow\" you saved";

/// How far the user's config files, `init.lua` and `ui.lua`, may reach.
/// `--no-plugins` turns them off, and a project folder nobody vouched for
/// stops at the global files.
///
/// The project variant carries the paths instead of a trust verdict, so the
/// only way to build one is a `Some` out of [`ProjectConfig::gated_path`]
/// and "trusted" cannot disagree with "which file".
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InitFiles {
    Disabled,
    Global,
    GlobalAndProject { init_lua: PathBuf },
}

impl InitFiles {
    pub fn resolve(project_config: &ProjectConfig, no_plugins: bool) -> Self {
        if no_plugins {
            return InitFiles::Disabled;
        }
        project_config
            .gated_path(GatedFile::InitLua)
            .map_or(Self::Global, |init_lua| Self::GlobalAndProject { init_lua })
    }

    /// The project files in {dir}, a trusted project's `.maki`, for tests
    /// and embedders that vouch for it themselves.
    pub fn project_in(dir: &Path) -> Self {
        InitFiles::GlobalAndProject {
            init_lua: dir.join(INIT_LUA),
        }
    }
}

static BUNDLED: Bundled = Bundled::new(bundled_plugins![
    "sessions" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/sessions"),
    "thinking" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/thinking"),
    "index" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/index"),
    "webfetch" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/webfetch"),
    "websearch" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/websearch"),
    "bash" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/bash"),
    "batch" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/batch"),
    "grep" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/grep"),
    "glob" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/glob"),
    "skill" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/skill"),
    "question" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/question"),
    "todo_write" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/todo_write"),
    "read" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/read"),
    "write" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/write"),
    "edit" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/edit"),
    // Below the tools it pre-approves: `memory` allows writes into its own
    // state dir, and a rule can only name a registered tool.
    "memory" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/memory"),
    "task" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/task"),
    "code_execution" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/code_execution"),
    "completion" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/completion"),
    "view_image" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/view_image"),
    "lib" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/lib"),
    "list" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/list"),
    // The rest register no tool. They declare a provider maki ships, on
    // the same surface a third-party plugin declares one with, which is what
    // keeps that surface honest.
    "synthetic" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/synthetic"),
    "deepseek" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/deepseek"),
    "mistral" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/mistral"),
    "tensorx" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/tensorx"),
    "regolo" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/regolo"),
    "requesty" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/requesty"),
    "openrouter" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/openrouter"),
]);

/// The plugins this binary ships, for a caller of a core function that runs
/// without a plugin host.
pub fn bundled() -> &'static Bundled {
    &BUNDLED
}

/// [`pack::discover`] against this binary's bundled names.
pub fn discover(site: &Path) -> Discovery {
    pack::discover(site, &BUNDLED)
}

/// [`pack::discover_installed`] against this binary's bundled names.
pub fn discover_installed(no_plugins: bool) -> Discovery {
    pack::discover_installed(no_plugins, &BUNDLED)
}

/// [`pack::install_declared`] against this binary's bundled names.
pub fn install_declared(specs: &[Declared], interaction: Interaction) -> InstallReport {
    pack::install_declared(specs, interaction, &[Component::Agent], &BUNDLED)
}

/// Permissions and agent grant for a bundled plugin.
pub fn bundled_grant(name: &str) -> Result<(PluginPermissions, Box<dyn Any + Send>), PluginError> {
    let permissions = BUNDLED.find(name)?.permissions(Component::Agent)?;
    let grant = AgentGrant::new(DeclAuthority::Bundled, Arc::from([]));
    Ok((permissions, Box::new(grant)))
}

struct AgentPolicy {
    registry: Arc<ToolRegistry>,
}

impl ComponentPolicy for AgentPolicy {
    fn grant(&self, kind: LoadKind) -> DomainGrant {
        Box::new(match kind {
            // The one load that ships inside the binary, and so the one that
            // may declare a provider under a built-in slug.
            LoadKind::Bundled => AgentGrant::new(DeclAuthority::Bundled, Arc::from([])),
            // The user's own code runs with every permission the config
            // dir's `plugin.toml` grants, so it may register any slug maki
            // does not ship without a separate grant.
            LoadKind::Config => AgentGrant::new(DeclAuthority::Config, Arc::from([])),
            LoadKind::Package(granted_providers) => {
                AgentGrant::new(DeclAuthority::ThirdParty, granted_providers)
            }
        })
    }

    /// Keep the granted slugs owned even if the load fails, so the catalog
    /// client cannot take over the slug and receive the key the user saved
    /// for it.
    fn before_package_load(&self, package: &str, granted_providers: &[String]) {
        keep_granted(package, granted_providers);
    }

    /// The names the plugin registered that maki's own permission defaults are
    /// keyed on. Taking such a name is allowed, and a drop-in replacement may
    /// want the builtin's rules, but the user has to be told which rules the
    /// plugin just inherited. One warning lists them all, because the TUI
    /// flashes a single warning and a per-tool one would drop the rest.
    fn load_warning(&self, plugin: &str) -> Option<String> {
        let snapshot = self.registry.iter();
        let names: Vec<String> = snapshot
            .iter()
            .filter(|t| t.plugin.as_ref() == plugin)
            .filter(|t| carries_builtin_defaults(t.name()))
            .map(|t| format!("`{}`", t.name()))
            .collect();
        if names.is_empty() {
            return None;
        }
        Some(format!(
            "{plugin}: registered {}, so it {PERMISSION_NAME_WARNING}",
            names.join(", ")
        ))
    }
}

pub struct PluginHost {
    inner: LuaThread,
    agent: AgentHandles,
    plugin_rules: Arc<PluginRuleStore>,
    policy: AgentPolicy,
    bundled: &'static Bundled,
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        let Some(handle) = self.inner.join.take() else {
            return;
        };
        // Start the shutdown first, or the join below waits for all
        // queued bulk work to drain.
        self.begin_shutdown();
        let (done_tx, done_rx) = flume::bounded(1);
        std::thread::spawn(move || {
            let _ = done_tx.send(handle.join().is_err());
        });
        match done_rx.recv_timeout(SHUTDOWN_TIMEOUT) {
            Ok(true) => tracing::warn!("lua thread panicked on shutdown"),
            Err(_) => tracing::warn!("lua thread did not stop within timeout, detaching"),
            Ok(false) => {}
        }
    }
}

impl PluginHost {
    pub fn new(registry: Arc<ToolRegistry>) -> Result<Self, PluginError> {
        Self::start(registry, true)
    }

    /// `jit: false` (the `--no-jit` flag) runs plugin Lua on the O1
    /// interpreter with full debug info. Applied at VM creation, so
    /// every chunk gets it, init.lua files included.
    pub fn start(registry: Arc<ToolRegistry>, jit: bool) -> Result<Self, PluginError> {
        Self::start_with(registry, jit, &BUNDLED)
    }

    fn start_with(
        registry: Arc<ToolRegistry>,
        jit: bool,
        bundled: &'static Bundled,
    ) -> Result<Self, PluginError> {
        let _runtime = enter_test_runtime();
        provider_adapter::install();
        let plugin_rules = Arc::new(PluginRuleStore::default());
        let lanes = RequestLanes::new();
        let (domain, agent) =
            domains::compose(&lanes, Arc::clone(&registry), Arc::clone(&plugin_rules));
        let lua = runtime::spawn(lanes, domain, Arc::clone(&agent.layered), bundled, jit)?;
        Ok(Self {
            inner: lua,
            agent,
            plugin_rules,
            policy: AgentPolicy { registry },
            bundled,
        })
    }

    fn steps(&self) -> HostSteps<'_> {
        HostSteps::new(self.core_handle(), self.bundled, &self.policy)
    }

    /// The store that `maki.api.register_permission_rule` writes into. Hand
    /// it to every [`maki_agent::permissions::PermissionManager`] so plugin
    /// rules apply to all sessions.
    pub fn plugin_rules(&self) -> Arc<PluginRuleStore> {
        Arc::clone(&self.plugin_rules)
    }

    /// Stop the Lua thread from taking new work without joining it, so the
    /// caller can rebuild shared state (like the tool registry) while the
    /// old VM winds down on its own. The flag makes the watchdog abort
    /// in-flight callbacks, `Shutdown` on the priority lane skips ahead of
    /// queued bulk work, and swapping the senders for disconnected ones
    /// makes every later host call fail right at the send; `&mut self`
    /// rules out a call racing the swap. `Drop` still joins the thread.
    pub fn begin_shutdown(&mut self) {
        self.agent.background.lose_all();
        self.inner.shutdown.store(true, Ordering::Release);
        let _ = self.inner.prio_tx.send(Request::Shutdown);
        self.inner.tx = flume::unbounded().0;
        self.inner.prio_tx = flume::unbounded().0;
    }

    /// Boots the runtime and loads every bundled plugin into `registry`, the
    /// ones that ship switched off included. For callers like tests and
    /// docgen that want the full builtin set without building a config, and
    /// which have to document an opt-in plugin's options too.
    pub fn with_all_builtins(registry: Arc<ToolRegistry>) -> Result<Self, PluginError> {
        let opt_in: HashMap<String, PluginFileConfig> = maki_config::OPTIONAL_BUILTINS
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    PluginFileConfig {
                        enabled: Some(true),
                        ..PluginFileConfig::default()
                    },
                )
            })
            .collect();
        let mut host = Self::new(registry)?;
        host.load_builtins(&PluginsConfig::from_plugins(opt_in))?;
        Ok(host)
    }

    /// Runs global and project `init.lua` files and returns their config.
    ///
    /// `warnings` collects non-fatal startup problems (an incompatible
    /// `plugin.toml` skips that directory's Lua) for the caller to surface.
    pub fn load_init_files(
        &self,
        init_files: InitFiles,
        warnings: &mut Vec<String>,
    ) -> Result<Option<RawConfig>, PluginError> {
        self.load_init_files_from_dirs(
            init_files,
            maki_storage::paths::config_search_dirs(),
            warnings,
        )
    }

    fn load_init_files_from_dirs(
        &self,
        init_files: InitFiles,
        global_dirs: impl IntoIterator<Item = PathBuf>,
        warnings: &mut Vec<String>,
    ) -> Result<Option<RawConfig>, PluginError> {
        if init_files == InitFiles::Disabled {
            return Ok(None);
        }

        let steps = self.steps();
        let global_dirs: Vec<PathBuf> = global_dirs.into_iter().collect();
        let mut merged: Option<RawConfig> = None;

        for global_dir in &global_dirs {
            steps.run_config_file(
                &global_dir.join(INIT_LUA),
                ConfigScope::Global,
                &mut merged,
                warnings,
            )?;
            if merged.is_some() {
                break;
            }
        }
        if let InitFiles::GlobalAndProject { init_lua, .. } = &init_files {
            steps.run_config_file(init_lua, ConfigScope::Project, &mut merged, warnings)?;
        }
        Ok(merged)
    }

    pub fn load_builtins(&mut self, config: &PluginsConfig) -> Result<(), PluginError> {
        let result = self.steps().load_bundled(config, Component::Agent);
        // Armed even when a load failed, so a caller that only warns about the
        // error is not left interpreting for the rest of the session.
        let _ = self.inner.tx.send(Request::WarmJit);
        result
    }

    fn send_load(
        &self,
        name: Arc<str>,
        chunks: Vec<LoadChunk>,
        context: LoadContext,
    ) -> Result<(), PluginError> {
        self.steps().send_load(
            name,
            chunks.into_iter().map(LoadStep::from).collect(),
            context,
        )
    }

    /// Option specs declared by loaded plugins via `maki.api.register_options`,
    /// keyed by plugin name. Used by docgen.
    pub fn plugin_options(&self) -> Result<PluginOptionSpecs, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::CollectPluginOptions { reply: reply_tx })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)
    }

    /// Runs a source as the global `init.lua`.
    ///
    /// The one scope where `maki.pack.add` may declare packages, so it is its
    /// own method: deriving the privilege from a source name would let any
    /// caller reach it by spelling the name the right way.
    pub fn send_global_init_lua(
        &self,
        source: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        self.steps()
            .run_config_lua(source, ConfigScope::Global, plugin_dir)
            .map(|setup| setup.raw)
    }

    /// Runs a source as a config chunk named after itself. It gets the
    /// read-only `maki.pack` table.
    pub fn send_run_init_lua(
        &self,
        source: String,
        source_name: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        self.steps()
            .run_config_lua(source, ConfigScope::Named(source_name), plugin_dir)
            .map(|setup| setup.raw)
    }

    pub fn unload(&self, plugin: &str) -> Result<(), PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::ClearPlugin {
                plugin: Arc::from(plugin),
                reply: reply_tx,
            })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?;
        Ok(())
    }

    /// Runs a source with every permission granted, for tests and embedders.
    /// Lua arriving from disk must not: it goes through
    /// [`PluginHost::load_builtins`], [`PluginHost::load_packages`] or the
    /// `init.lua` path, each of which derives its grant from a manifest.
    pub fn load_source(&self, name: &str, source: &str) -> Result<(), PluginError> {
        self.load_source_with_opts(name, source, serde_json::Map::new())
    }

    pub fn load_source_with_opts(
        &self,
        name: &str,
        source: &str,
        opts: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), PluginError> {
        self.send_load(
            Arc::from(name),
            vec![LoadChunk::new(name, source)],
            LoadContext {
                opts: Arc::new(opts),
                ..LoadContext::plain(
                    None,
                    PluginPermissions::trusted(),
                    Box::new(AgentGrant::third_party()),
                )
            },
        )
    }

    pub fn load_source_with_permissions(
        &self,
        name: &str,
        source: &str,
        permissions: PluginPermissions,
    ) -> Result<(), PluginError> {
        self.send_load(
            Arc::from(name),
            vec![LoadChunk::new(name, source)],
            LoadContext::plain(None, permissions, Box::new(AgentGrant::third_party())),
        )
    }

    /// Runs a source with a provider grant, for tests and embedders that load
    /// a plugin registering providers outside the package system.
    pub fn load_source_with_grant(
        &self,
        name: &str,
        source: &str,
        permissions: PluginPermissions,
        granted_providers: Vec<String>,
    ) -> Result<(), PluginError> {
        let grant = AgentGrant::new(DeclAuthority::ThirdParty, Arc::from(granted_providers));
        let context = LoadContext::plain(None, permissions, Box::new(grant));
        self.send_load(Arc::from(name), vec![LoadChunk::new(name, source)], context)
    }

    pub fn load_plugin_file(&self, path: &Path) -> Result<(), PluginError> {
        let source = fs::read_to_string(path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let plugin_dir = path.parent().map(Path::to_path_buf);
        check_plugin_compatibility(USER_PLUGIN, plugin_dir.as_deref())?;
        let permissions = load_plugin_permissions(plugin_dir.as_deref(), Component::Agent);
        let granted_providers = load_plugin_granted_providers(plugin_dir.as_deref());
        // Test-only path today. Once user plugin dirs exist: derive a real
        // plugin name, since the hardcoded "user" would collide across files,
        // pass the `plugins.<name>` opts through, and teach the
        // unknown-plugin guards about user plugin names.
        let grant = AgentGrant::new(DeclAuthority::ThirdParty, Arc::from(granted_providers));
        let context = LoadContext::plain(plugin_dir, permissions, Box::new(grant));
        self.send_load(
            Arc::from(USER_PLUGIN),
            vec![LoadChunk::new(path.display().to_string(), source)],
            context,
        )
    }

    /// Packages declared by `maki.pack.add` in `init.lua` and `ui.lua`.
    ///
    /// Read after the init files have run, which is when the declared set is
    /// complete and before anything is installed.
    pub fn declared_packages(&self) -> Result<Vec<crate::api::pack::Declared>, PluginError> {
        self.steps().declared_packages()
    }

    /// Loads one external package directory as a single owner.
    ///
    /// Every `plugin/*.lua` becomes a chunk, and the chunks share one
    /// environment, so what one file registers the next can use. The whole set
    /// commits or none of it does.
    pub fn load_package(
        &self,
        name: &str,
        dir: &Path,
        permissions: PluginPermissions,
        opts: PluginOpts,
    ) -> Result<(), PluginError> {
        self.steps()
            .load_package(name, dir, permissions, Arc::from(Vec::new()), opts, None)
    }

    /// Refuses further `maki.packadd` calls, and returns anything the queue
    /// still holds.
    ///
    /// One call and not a read followed by a close, because a Lua task can
    /// record an activation between the two and closing would strand it.
    pub fn seal_pack_ops(&self) -> Result<Vec<String>, PluginError> {
        self.steps().seal_pack_ops()
    }

    /// Loads every package that should be loaded now: the `start/` ones, and
    /// the `opt/` ones that `maki.packadd` named.
    pub fn load_packages(
        &self,
        packages: &[DiscoveredPackage],
        config: &PluginsConfig,
    ) -> Vec<String> {
        self.load_declared_packages(packages, &[], config)
    }

    /// As `load_packages`, with the declarations that may carry a custom
    /// loader. A package with no matching declaration loads its `plugin/*.lua`.
    /// Each package loads the components this host loads that its approvals
    /// on disk let run.
    pub fn load_declared_packages(
        &self,
        packages: &[DiscoveredPackage],
        declared: &[crate::api::pack::Declared],
        config: &PluginsConfig,
    ) -> Vec<String> {
        self.steps().load_declared_packages(
            packages,
            declared,
            config,
            Component::Agent,
            &ComponentApprovals::read(&[Component::Agent]),
        )
    }

    pub fn core_handle(&self) -> CoreHandle {
        CoreHandle::new(self.inner.tx.clone(), self.inner.prio_tx.clone())
    }

    pub fn agent_handle(&self) -> AgentHandle {
        AgentHandle::new(self.core_handle(), self.agent.background.clone())
    }
}

/// The agent side's requests: restores, clicks, prompt slots, the session
/// snapshot and plugin session cancels.
#[derive(Clone)]
pub struct AgentHandle {
    core: CoreHandle,
    /// Shared with the Lua thread too: Esc on a plugin task closes its
    /// session from the event loop, with no Lua-thread hop.
    background: BackgroundTasks,
}

impl AgentHandle {
    fn new(core: CoreHandle, background: BackgroundTasks) -> Self {
        Self { core, background }
    }

    /// Esc in a plugin task's tab. Drops the session's registry entry, which
    /// fires its cancel, and the plugin's pending `:prompt()` returns
    /// "stopped by user". Synchronous on purpose: the registry is a mutex the
    /// Lua thread shares, so a busy Lua thread cannot delay the stop.
    pub fn cancel_plugin_session(&self, id: &str) {
        self.background.stop(id, Stop::User);
    }

    pub fn request_background(&self, tool_use_id: String) -> flume::Receiver<crate::MoveOutcome> {
        let (tx, rx) = flume::bounded(1);
        self.core
            .send_prio(Request::domain(crate::domains::agent::moves::MoveCall(
                tool_use_id,
                tx,
            )));
        rx
    }

    pub fn collect_prompt_slots(&self) -> ResolvedPrompts {
        let (tx, rx) = flume::bounded(1);
        self.core
            .send(Request::domain(CollectPromptSlots { reply: tx }));
        rx.recv().unwrap_or_default()
    }

    pub async fn collect_prompt_slots_async(&self) -> ResolvedPrompts {
        let (tx, rx) = flume::bounded(1);
        self.core
            .send(Request::domain(CollectPromptSlots { reply: tx }));
        rx.recv_async().await.unwrap_or_default()
    }

    /// Headless drivers install their own provider so `maki.session.read` has
    /// something to answer with instead of "no interactive UI attached". The UI
    /// leaves the slot empty and answers through its event loop, which owns the
    /// live session runtimes.
    pub fn install_session_snapshot(&self, provider: crate::SessionSnapshotFn) {
        self.core
            .send(Request::domain(InstallSessionSnapshot { provider }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::prompt::{PromptId, ResolvedPrompts, Slot, assemble};
    use maki_agent::tools::ToolRegistry;
    use test_case::test_case;

    fn slots_from(plugin: &str, src: &str) -> (PluginHost, ResolvedPrompts) {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(plugin, src).unwrap();
        let slots = host.agent_handle().collect_prompt_slots();
        (host, slots)
    }

    fn contents(slots: &ResolvedPrompts, prompt: PromptId, slot: Slot) -> Vec<&str> {
        slots
            .get(prompt, slot)
            .iter()
            .map(|e| e.content.as_str())
            .collect()
    }

    #[test]
    fn callback_string_lands_in_targeted_prompt_only() {
        let (_host, slots) = slots_from(
            "cb",
            r#"
            maki.api.register_prompt_hint({
                slot = "tool_usage",
                prompt = "general",
                content = function() return "from_cb" end,
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::General, Slot::ToolUsage),
            ["from_cb"]
        );
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
    }

    #[test]
    fn callback_returning_nil_contributes_nothing() {
        let (_host, slots) = slots_from(
            "nil_cb",
            r#"
            maki.api.register_prompt_hint({
                slot = "tool_usage",
                content = function() return nil end,
            })
            "#,
        );
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
    }

    /// A hint with no `prompt` is a default: it lands on every prompt that has the slot.
    #[test]
    fn static_no_prompt_lands_on_all_prompts_with_slot() {
        let (_host, slots) = slots_from(
            "static_hint",
            r#"
            maki.api.register_prompt_hint({
                slot = "efficient_tools",
                content = "index",
            })
            "#,
        );
        for &pid in PromptId::ALL {
            assert_eq!(contents(&slots, pid, Slot::EfficientTools), ["index"]);
        }
    }

    /// `conventions` lives on system and general but not research, so a default
    /// hint follows the slot and skips research.
    #[test]
    fn default_hint_skips_prompts_lacking_the_slot() {
        let (_host, slots) = slots_from(
            "conv",
            r#"
            maki.api.register_prompt_hint({
                slot = "conventions",
                content = "follow conventions",
            })
            "#,
        );
        for pid in [PromptId::System, PromptId::General] {
            assert_eq!(
                contents(&slots, pid, Slot::Conventions),
                ["follow conventions"]
            );
        }
        assert!(contents(&slots, PromptId::Research, Slot::Conventions).is_empty());
    }

    /// Targeting a prompt that does not have the slot quietly drops the hint.
    #[test]
    fn register_prompt_hint_ignores_missing_slot() {
        let (_host, slots) = slots_from(
            "drop",
            r#"
            maki.api.register_prompt_hint({
                slot = "after_instructions",
                prompt = "research",
                content = "never lands",
            })
            "#,
        );
        assert!(contents(&slots, PromptId::Research, Slot::AfterInstructions).is_empty());
        assert!(!assemble(PromptId::Research, &slots, "").contains("never lands"));
    }

    #[test]
    fn prompt_list_targets_each_listed_prompt() {
        const CONTENT: &str = "shared";
        let (_host, slots) = slots_from(
            "list",
            r#"
            maki.api.register_prompt_hint({
                slot = "tool_usage",
                prompt = { "system", "research" },
                content = "shared",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::ToolUsage),
            [CONTENT]
        );
        assert_eq!(
            contents(&slots, PromptId::Research, Slot::ToolUsage),
            [CONTENT]
        );
        assert!(contents(&slots, PromptId::General, Slot::ToolUsage).is_empty());
    }

    #[test_case(Slot::Identity ; "identity")]
    #[test_case(Slot::Tone ; "tone")]
    #[test_case(Slot::Conventions ; "static_section")]
    fn omitted_replacement_targets_only_the_main_prompt(slot: Slot) {
        const CONTENT: &str = "Custom policy";
        let (_host, prompts) = slots_from(
            "persona",
            &format!("maki.api.set_prompt({{ slot = \"{slot}\", content = {CONTENT:?} }})"),
        );
        assert!(assemble(PromptId::System, &prompts, "").contains(CONTENT));
        for pid in [PromptId::Research, PromptId::General] {
            assert_eq!(
                assemble(pid, &prompts, ""),
                assemble(pid, &ResolvedPrompts::default(), ""),
            );
        }
    }

    #[test]
    fn set_prompt_sets_identity() {
        let (_host, slots) = slots_from(
            "setter",
            r#"
            maki.api.set_prompt({
                slot = "identity",
                content = "New identity",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Identity),
            ["New identity"]
        );
    }

    #[test]
    fn set_prompt_explicit_system_prompt() {
        let (_host, slots) = slots_from(
            "setter",
            r#"
            maki.api.set_prompt({
                slot = "identity",
                prompt = "system",
                content = "Explicit identity",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Identity),
            ["Explicit identity"]
        );
        assert!(contents(&slots, PromptId::Research, Slot::Identity).is_empty());
        assert!(contents(&slots, PromptId::General, Slot::Identity).is_empty());
    }

    #[test]
    fn prompt_field_targets_specific_prompt() {
        let (_host, slots) = slots_from(
            "targeter",
            r#"
            maki.api.register_prompt_hint({
                slot = "tool_usage",
                prompt = "general",
                content = "General hint",
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::General, Slot::ToolUsage),
            ["General hint"]
        );
        assert!(contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
    }

    #[test]
    fn set_prompt_replaces_aggregate_default_and_preserves_hints() {
        const REPLACEMENT: &str = "Analyse information without coding guidance.";
        const HINT: &str = "Use the library catalogue.";
        let (_host, slots) = slots_from(
            "sections",
            r#"
            maki.api.set_prompt({ slot = "conventions", prompt = "system", content = "Analyse information without coding guidance." })
            maki.api.register_prompt_hint({ slot = "conventions", prompt = "system", content = "Use the library catalogue." })
            "#,
        );
        let rendered = assemble(PromptId::System, &slots, "");
        assert!(rendered.contains(&format!("{REPLACEMENT}\n{HINT}")));
        assert!(!rendered.contains("Never assume a library is available."));
        assert!(slots.has_replacements());
        assert!(!slots.policy_key().is_empty());
    }

    #[test]
    fn set_prompt_ignores_known_section_missing_from_template() {
        let (_host, slots) = slots_from(
            "absent",
            r#"maki.api.set_prompt({ slot = "identity", prompt = "plan", content = "never lands" })"#,
        );
        assert!(contents(&slots, PromptId::Plan, Slot::Identity).is_empty());
        assert!(!assemble(PromptId::Plan, &slots, "").contains("never lands"));
    }

    #[test]
    fn set_prompt_with_callback() {
        let (_host, slots) = slots_from(
            "setter_cb",
            r#"
            maki.api.set_prompt({
                slot = "identity",
                content = function() return "Dyn identity" end,
            })
            "#,
        );
        assert_eq!(
            contents(&slots, PromptId::System, Slot::Identity),
            ["Dyn identity"]
        );
    }

    #[test]
    fn singleton_callback_is_collected_once_per_snapshot() {
        let (host, first) = slots_from(
            "live",
            r#"
            local count = 0
            maki.api.set_prompt({ prompt = {"system", "research", "general"}, slot = "identity", content = function()
                count += 1
                return tostring(count)
            end })
            "#,
        );
        let second = host.agent_handle().collect_prompt_slots();
        for &pid in PromptId::ALL {
            assert_eq!(contents(&first, pid, Slot::Identity), ["1"]);
            assert_eq!(contents(&second, pid, Slot::Identity), ["2"]);
        }
        assert!(!first.has_replacements());
        assert!(!second.has_replacements());
    }

    #[test]
    fn empty_replacements_clear_default_content() {
        let (_host, slots) = slots_from(
            "clear",
            r#"
            maki.api.set_prompt({ prompt = "system", slot = "identity", content = "" })
            maki.api.set_prompt({ prompt = "system", slot = "conventions", content = "" })
            maki.api.set_prompt({ prompt = "plan", content = "" })
            "#,
        );
        const CODING_CONVENTION: &str = "Never assume a library is available.";
        let defaults = assemble(PromptId::System, &ResolvedPrompts::default(), "");
        assert!(defaults.contains(CODING_CONVENTION));
        let rendered = assemble(PromptId::System, &slots, "");
        assert!(!rendered.contains("You are Maki"));
        assert!(!rendered.contains(CODING_CONVENTION));
        assert_eq!(assemble(PromptId::Plan, &slots, ""), "");
        assert!(slots.has_template(PromptId::Plan));
    }

    #[test_case("system", PromptId::System ; "system")]
    #[test_case("research", PromptId::Research ; "research")]
    #[test_case("general", PromptId::General ; "general")]
    #[test_case("plan", PromptId::Plan ; "plan")]
    fn complete_replacement_has_no_implicit_sections(name: &str, id: PromptId) {
        const TEMPLATE: &str = "You help analyse information.";
        let (_host, slots) = slots_from(
            "author",
            &format!("maki.api.set_prompt({{ prompt = {name:?}, content = {TEMPLATE:?} }})"),
        );
        assert_eq!(assemble(id, &slots, "coding instructions"), TEMPLATE);
    }

    #[test]
    fn repeated_singleton_discards_callback_and_replaces_only_its_targets() {
        let (_host, slots) = slots_from(
            "singleton",
            r#"
            maki.api.set_prompt({ prompt = { "system", "general" }, slot = "identity", content = function() return "first" end })
            maki.api.set_prompt({ prompt = "system", slot = "identity", content = function() return "last" end })
            "#,
        );
        assert_eq!(contents(&slots, PromptId::System, Slot::Identity), ["last"]);
        assert_eq!(
            contents(&slots, PromptId::General, Slot::Identity),
            ["first"]
        );
    }

    #[test]
    fn replaced_singleton_callback_is_not_invoked() {
        let (_host, slots) = slots_from(
            "singleton",
            r#"
            local calls = 0
            maki.api.set_prompt({ slot = "identity", content = function() calls += 1; return "old" end })
            maki.api.set_prompt({ slot = "identity", content = function() return tostring(calls) end })
            "#,
        );
        assert_eq!(contents(&slots, PromptId::System, Slot::Identity), ["0"]);
    }
}
