//! The UI runtime and its component loader.

use std::any::Any;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use include_dir::include_dir;
use maki_config::{ConfigError, GatedFile, PluginsConfig, ProjectConfig, RawConfig, UiConfig};
use maki_lua_core::api::net::set_allowed_private_hosts;
use maki_lua_core::api::slot::LayeredTools;
use maki_lua_core::bundled::Bundled;
use maki_lua_core::bundled_plugins;
use maki_lua_core::handle::{CoreHandle, SHUTDOWN_TIMEOUT};
use maki_lua_core::host::{ComponentPolicy, HostSteps, LoadKind};
use maki_lua_core::runtime::{self, ConfigScope, DomainGrant, LuaThread, Request, RequestLanes};
#[cfg(any(test, feature = "test-support"))]
use maki_lua_core::runtime::{LoadChunk, LoadContext};

use crate::domains::ui::{UiDomain, UiHandles};
use crate::error::PluginError;
use crate::pack::{self, ComponentApprovals, DiscoveredPackage, Interaction, PackRoots};
use crate::plugin_permissions::{Component, PluginPermissions};
use crate::question_form::QUESTION_PLUGIN;

const LIB_PLUGIN: &str = "lib";
const UI_LUA: &str = "ui.lua";
const UI_COMPONENT: [Component; 1] = [Component::Ui];
const SKIPPED_PACKAGE_WARNING: &str = "skipping package";
const NO_SITE_WARNING: &str = "no data directory, so no package was installed or loaded";

static BUNDLED: Bundled = Bundled::new(bundled_plugins![
    "completion" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/completion"),
    "task" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/task"),
    "memory" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/memory"),
    "sessions" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/sessions"),
    "thinking" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/thinking"),
    QUESTION_PLUGIN => include_dir!("$CARGO_MANIFEST_DIR/../plugins/question"),
    "list" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/list"),
    "view_image" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/view_image"),
    "skill" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/skill"),
    "webfetch" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/webfetch"),
    "websearch" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/websearch"),
    "bash" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/bash"),
    "batch" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/batch"),
    "code_execution" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/code_execution"),
    "todo_write" => include_dir!("$CARGO_MANIFEST_DIR/../plugins/todo_write"),
    LIB_PLUGIN => include_dir!("$CARGO_MANIFEST_DIR/../plugins/lib"),
]);

pub fn prepare_pack_command(
    command: &pack::PackCommand,
    context: pack::PackContext,
) -> pack::PackPreparation {
    pack::prepare_pack_command(command, &context, &UI_COMPONENT, &BUNDLED)
}

/// A bundled plugin's `[ui]` table. A bundled load here carries no domain
/// grant: no UI domain reads one.
pub fn bundled_grant(name: &str) -> Result<(PluginPermissions, Box<dyn Any + Send>), PluginError> {
    Ok((
        BUNDLED.find(name)?.permissions(Component::Ui)?,
        Box::new(()),
    ))
}

/// The UI's side of the host steps. No UI domain reads a domain grant, no
/// UI load declares a provider, and nothing a UI plugin registers inherits
/// the agent's permission rules.
struct UiPolicy;

impl ComponentPolicy for UiPolicy {
    fn grant(&self, _kind: LoadKind) -> DomainGrant {
        Box::new(())
    }

    fn before_package_load(&self, _package: &str, _granted_providers: &[String]) {}

    fn load_warning(&self, _plugin: &str) -> Option<String> {
        None
    }
}

/// Which of the user's `ui.lua` files a client runs: none with
/// `--no-plugins`, and the global one alone in a project nobody trusted.
///
/// The project variant carries the path instead of a trust verdict, so the
/// only way to build one is a `Some` out of [`ProjectConfig::gated_path`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiFiles {
    Bundled,
    Disabled,
    Global,
    GlobalAndProject(PathBuf),
}

impl UiFiles {
    pub fn resolve(project_config: &ProjectConfig, no_plugins: bool) -> Self {
        if no_plugins {
            return Self::Disabled;
        }
        project_config
            .gated_path(GatedFile::UiLua)
            .map_or(Self::Global, Self::GlobalAndProject)
    }
}

/// Where a startup finds the user's own files: the directories the global
/// `ui.lua` is searched in, and the site and lockfile of the client's own
/// packages, which a client on another machine than its agent keeps apart.
pub struct UiDirs {
    pub config: Vec<PathBuf>,
    pub packs: io::Result<PackRoots>,
}

impl UiDirs {
    pub fn of_user() -> Self {
        Self {
            config: maki_storage::paths::config_search_dirs(),
            packs: PackRoots::of_user(),
        }
    }
}

/// What a startup that loaded its plugins gives a client: the host, what
/// `ui.lua` set that the event loop reads, and the warnings to show,
/// sanitized.
pub struct UiStart {
    pub host: UiHost,
    pub ui_config: UiConfig,
    pub input_history_size: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum UiStartError {
    #[error(transparent)]
    Plugin(#[from] PluginError),
    #[error("invalid ui.lua config: {0}")]
    Config(#[from] ConfigError),
}

/// {plugins} without the agent's builtins: the agent ships and loads them,
/// so one enabled here is not a plugin missing from this host.
fn shipped_here(plugins: &PluginsConfig) -> PluginsConfig {
    let mut shipped = plugins.clone();
    shipped.names.retain(|name| BUNDLED.is_bundled(name));
    shipped
}

/// The UI runtime,
/// with no agent domain: what a client runs. Its every tab is remote.
pub struct UiHost {
    inner: LuaThread,
    pub(crate) handles: UiHandles,
    pub(crate) layered: Arc<LayeredTools>,
}

impl UiHost {
    /// Starts the UI runtime and loads: the {files} `ui.lua` names, the bundled plugins it
    /// enables, the packages it declares, installed into the site of {dirs}
    /// and approved for their UI component on {interaction}'s terminal, and
    /// the UI entries of those and of the packages found there. With
    /// [`UiFiles::Disabled`] nothing loads, though `require` still reaches
    /// the bundled modules, so the question form opens.
    ///
    /// `jit: false` (the `--no-jit` flag) runs plugin Lua on the O1
    /// interpreter.
    pub fn start(
        interaction: Interaction,
        jit: bool,
        files: UiFiles,
        dirs: UiDirs,
    ) -> Result<UiStart, UiStartError> {
        let _runtime = maki_lua_core::host::enter_test_runtime();
        let host = Self::spawn(interaction, jit)?;
        let loaded = host.load_plugins(interaction, files, dirs);
        // Armed even when a load failed, so a caller that only reports it
        // is not left interpreting.
        let _ = host.inner.tx.send(Request::WarmJit);
        let (ui_config, input_history_size, warnings) = loaded?;
        Ok(UiStart {
            host,
            ui_config,
            input_history_size,
            warnings: warnings
                .iter()
                .map(|warning| pack::sanitize_message(warning))
                .collect(),
        })
    }

    fn spawn(interaction: Interaction, jit: bool) -> Result<Self, PluginError> {
        let _runtime = maki_lua_core::host::enter_test_runtime();
        let (ui, handles) = UiDomain::new(interaction);
        let layered = Arc::default();
        let inner = runtime::spawn(
            RequestLanes::new(),
            Box::new(ui),
            Arc::clone(&layered),
            &BUNDLED,
            jit,
        )?;
        maki_lua_core::api::fs::publish_walks(vec![CoreHandle::new(
            inner.tx.clone(),
            inner.prio_tx.clone(),
        )]);
        Ok(Self {
            inner,
            handles,
            layered,
        })
    }

    fn steps(&self) -> HostSteps<'_> {
        HostSteps::new(self.core_handle(), &BUNDLED, &UiPolicy)
    }

    fn load_plugins(
        &self,
        interaction: Interaction,
        files: UiFiles,
        dirs: UiDirs,
    ) -> Result<(UiConfig, usize, Vec<String>), UiStartError> {
        let mut warnings = Vec::new();
        if matches!(files, UiFiles::Disabled | UiFiles::Bundled) {
            let config = RawConfig::default().into_config(&[])?;
            if files == UiFiles::Bundled {
                self.steps()
                    .load_bundled(&shipped_here(&config.plugins), Component::Ui)?;
            }
            return Ok((config.ui, config.storage.input_history_size, warnings));
        }
        let steps = self.steps();
        let discovery = match &dirs.packs {
            Ok(roots) => pack::discover(&roots.site, &BUNDLED),
            Err(error) => {
                warnings.push(format!("{NO_SITE_WARNING}: {error}"));
                pack::Discovery::default()
            }
        };
        let mut names = discovery.known_names();
        warnings.extend(
            discovery
                .problems
                .iter()
                .map(|problem| format!("{SKIPPED_PACKAGE_WARNING}: {problem}")),
        );

        let ui = self.run_ui_files(&files, &dirs.config, &mut warnings)?;
        let declared = steps.declared_packages()?;
        names.extend(
            declared
                .iter()
                .map(|declaration| declaration.spec.name.clone()),
        );
        names.sort();
        names.dedup();
        let config = ui.unwrap_or_default().into_config(&names)?;
        config.validate()?;

        // Before any plugin can call `maki.net`, so the first request already
        // sees the hosts the user exempted from the private-address block.
        set_allowed_private_hosts(&config.net.allowed_private_hosts);
        steps.load_bundled(&shipped_here(&config.plugins), Component::Ui)?;

        let (available, approvals) = match &dirs.packs {
            Ok(roots) => {
                let installed = pack::install_declared_in(
                    roots,
                    &declared,
                    interaction,
                    &UI_COMPONENT,
                    &BUNDLED,
                );
                warnings.extend(installed.failures);
                let available: Vec<DiscoveredPackage> = discovery
                    .packages
                    .into_iter()
                    .chain(installed.packages)
                    .collect();
                (
                    available,
                    ComponentApprovals::read_in(&roots.site, &UI_COMPONENT),
                )
            }
            Err(_) => (Vec::new(), ComponentApprovals::default()),
        };
        for warning in steps.load_declared_packages(
            &available,
            &declared,
            &config.plugins,
            Component::Ui,
            &approvals,
        ) {
            // A UI the install pass held back is gated again where it loads,
            // and reported there in the same words.
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }
        warnings.extend(self.handles.key_lint.take_summary());
        Ok((config.ui, config.storage.input_history_size, warnings))
    }

    /// Runs the first global `ui.lua` in {config_dirs}, then the project's
    /// when {files} names it, and returns what they set.
    fn run_ui_files(
        &self,
        files: &UiFiles,
        config_dirs: &[PathBuf],
        warnings: &mut Vec<String>,
    ) -> Result<Option<RawConfig>, PluginError> {
        let steps = self.steps();
        let mut merged = None;
        let global = config_dirs
            .iter()
            .map(|dir| dir.join(UI_LUA))
            .find(|path| path.is_file());
        if let Some(path) = global {
            steps.run_config_file(&path, ConfigScope::UiGlobal, &mut merged, warnings)?;
        }
        if let UiFiles::GlobalAndProject(path) = files {
            steps.run_config_file(path, ConfigScope::UiProject, &mut merged, warnings)?;
        }
        Ok(merged)
    }

    pub fn core_handle(&self) -> CoreHandle {
        CoreHandle::new(self.inner.tx.clone(), self.inner.prio_tx.clone())
    }

    /// Stops the Lua thread from taking new work without joining it: the
    /// watchdog aborts in-flight callbacks, `Shutdown` skips queued bulk work,
    /// and every later call fails at the send. `Drop` still joins the thread.
    pub fn begin_shutdown(&mut self) {
        self.inner.shutdown.store(true, Ordering::Release);
        let _ = self.inner.prio_tx.send(Request::Shutdown);
        self.inner.tx = flume::unbounded().0;
        self.inner.prio_tx = flume::unbounded().0;
    }
}

impl Drop for UiHost {
    fn drop(&mut self) {
        let Some(join) = self.inner.join.take() else {
            return;
        };
        self.begin_shutdown();
        let (done, stopped) = flume::bounded(1);
        std::thread::spawn(move || {
            let _ = done.send(join.join().is_err());
        });
        match stopped.recv_timeout(SHUTDOWN_TIMEOUT) {
            Ok(true) => tracing::warn!("lua thread panicked on shutdown"),
            Err(_) => tracing::warn!("lua thread did not stop within timeout, detaching"),
            Ok(false) => {}
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl UiHost {
    /// The UI runtime with no bundled plugin loaded, for tests that load
    /// their own.
    pub fn load_builtins_for_test(&self, config: &PluginsConfig) -> Result<(), PluginError> {
        self.steps()
            .load_bundled(&shipped_here(config), Component::Ui)
    }

    pub fn bare() -> Self {
        Self::spawn(Interaction::Tty, true).unwrap()
    }

    /// Runs {source} as plugin {name} with every permission granted.
    pub fn load_source(&self, name: &str, source: &str) -> Result<(), PluginError> {
        self.load_source_with_permissions(name, source, PluginPermissions::trusted())
    }

    pub fn load_source_with_permissions(
        &self,
        name: &str,
        source: &str,
        permissions: PluginPermissions,
    ) -> Result<(), PluginError> {
        self.steps().send_load(
            Arc::from(name),
            vec![LoadChunk::new(name, source).into()],
            LoadContext::plain(None, permissions, Box::new(())),
        )
    }

    /// [`Self::load_source`] without waiting for it: the receiver answers
    /// once the load ran.
    pub fn send_load_source(
        &self,
        name: &str,
        source: &str,
    ) -> flume::Receiver<Result<(), PluginError>> {
        let (reply, answer) = flume::bounded(1);
        let _ = self.inner.tx.send(Request::LoadSource {
            name: Arc::from(name),
            steps: vec![LoadChunk::new(name, source).into()],
            context: LoadContext::plain(None, PluginPermissions::trusted(), Box::new(())),
            reply,
        });
        answer
    }

    /// The requests the Lua thread has not taken yet. Once a request sent
    /// before is taken, it runs before the thread awaits anything else.
    pub fn untaken_requests(&self) -> usize {
        self.inner.tx.len()
    }

    pub fn unload(&self, plugin: &str) -> Result<(), PluginError> {
        let (reply, answer) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::ClearPlugin {
                plugin: Arc::from(plugin),
                reply,
            })
            .map_err(|_| PluginError::HostDead)?;
        answer.recv().map_err(|_| PluginError::HostDead)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use crate::{RenderCall, RenderCtx, RenderReason, RenderReply};
    use maki_config::ToolOutputLines;
    use maki_lua_core::api::not_in_ui_runtime;
    use maki_lua_core::facade::{NO_LOCAL_ERR, SessionRequest};
    use maki_lua_core::host::{AGENT_ONLY_IN_UI_LUA, UNDECLARED_UI_ENTRY_WARNING};
    use maki_types::tool::{ChildStatus, ToolChild, ToolData, ToolOutput};
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::api::util::command::{TabRef, UiAction};
    use crate::key::Key;

    const MAX_SCRIPT_LINES: usize = 2000;
    const LAST_SCRIPT_LINE: &str = "last_script_line";
    const DEFERRED_TOOL: &str = "deferred";
    const DEFERRED_RENDERER: &str = r#"
maki.ui.register_renderer("deferred", {
  body = function()
    local buf = maki.ui.buf()
    maki.async.run(function() buf:line("result") end)
    return buf
  end,
})
"#;
    const RENDER_CALL: &str = "renderer-test";
    const RENDER_COMMAND: &str = "printf result";
    const RENDER_TEXT: &str = "result";
    const DIFF_BEFORE: &str = "old\n";
    const DIFF_AFTER: &str = "new\n";
    const NESTED_BATCH_ERROR: &str = "cannot nest batch inside batch";
    const CANCELLED_CHILD: &str = "cancelled";
    const FIRST_LINE: &str = "first line";
    const LAST_LINE: &str = "last line";
    const WAIT: Duration = Duration::from_secs(5);
    const BUNDLED_COMMANDS: [&str; 5] = ["/memory", "/rename", "/sessions", "/tasks", "/thinking"];
    const BUNDLED_KEYS: [(&str, &str); 2] = [("thinking", "<M-t>"), ("sessions", "<C-p>")];
    const DATA_VERSION: u32 = 2;
    const AGENT_CALLER: &str = "agent-caller";
    const OTHER: &str = "other";
    const OTHER_COMMAND: &str = "/other";
    const PROBE: &str = "probe";
    const PROBE_COMMAND: &str = "/probe";
    const AGENT_SESSION: &str = "agent-session-7";
    const REFUSED: &str = "not here";

    const CONFIG_DIR: &str = "config";
    const SITE_DIR: &str = "site";
    const LOCKFILE: &str = "pack-lock.json";
    const MANIFEST: &str = "plugin.toml";
    const UI_ENTRY: &str = "ui/init.lua";
    const PROJECT_UI_LUA: &str = ".maki/ui.lua";
    const PACKAGE: &str = "uipkg";
    const PACKAGE_SRC: &str = "https://example.com/uipkg";
    const PACKAGE_REV: &str = "0123456789abcdef0123456789abcdef01234567";
    const PACKAGE_COMMAND: &str = "/uipkg";
    const HAND_INSTALLED: &str = "pack/vendor/start/uipkg";
    const UI_MANIFEST: &str = "[ui]\n";
    const AGENT_ONLY_MANIFEST: &str = "[permissions]\n";
    const UI_APPROVAL_REQUIRED: &str = "uipkg: UI approval is required";
    const HISTORY_SIZE: usize = 70;
    const AGENT_SECTION: &str = "always_yolo";
    const NO_SCROLLBAR_UI_LUA: &str = "maki.setup({ ui = { scrollbar = false } })";
    const UNKNOWN_OPTION_ERR: &str = "unknown option";
    const OPTION_TYPE_ERR: &str = "expected boolean";

    fn declaring_ui_lua() -> String {
        format!(
            "maki.setup({{ ui = {{ scrollbar = false }}, \
             storage = {{ input_history_size = {HISTORY_SIZE} }}, {AGENT_SECTION} = true }})\n\
             maki.pack.add({{ {{ src = '{PACKAGE_SRC}', name = '{PACKAGE}' }} }})"
        )
    }

    /// A client's own files, each under one root: the config directory
    /// `ui.lua` is searched in, and the site and lockfile of its packages.
    struct UserFiles(TempDir);

    impl UserFiles {
        fn new() -> Self {
            let files = Self(TempDir::new().unwrap());
            fs::create_dir_all(files.config_dir()).unwrap();
            files
        }

        fn config_dir(&self) -> PathBuf {
            self.0.path().join(CONFIG_DIR)
        }

        fn site(&self) -> PathBuf {
            self.0.path().join(SITE_DIR)
        }

        fn write(&self, path: &Path, contents: &str) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        fn ui_lua(&self, source: &str) {
            self.write(&self.config_dir().join(UI_LUA), source);
        }

        fn ui_package(&self, dir: &Path, manifest: &str) {
            self.write(&dir.join(MANIFEST), manifest);
            self.write(&dir.join(UI_ENTRY), &command(PACKAGE_COMMAND, ""));
        }

        /// The UI package as an install left it: a checkout at a recorded
        /// revision, so no install fetches anything.
        fn installed_package(&self) {
            let lock =
                json!({ "packages": { PACKAGE: { "src": PACKAGE_SRC, "rev": PACKAGE_REV } } });
            self.write(&self.0.path().join(LOCKFILE), &lock.to_string());
            let checkout = self
                .site()
                .join("pack/core")
                .join(PACKAGE)
                .join(PACKAGE_REV);
            self.ui_package(&checkout, UI_MANIFEST);
        }

        fn approve_ui(&self) {
            let store =
                json!({ "entries": { PACKAGE: { "src": PACKAGE_SRC, "permissions": [] } } });
            self.write(
                &pack::approvals_path_in(&self.site(), Component::Ui),
                &store.to_string(),
            );
        }

        fn start(&self, files: UiFiles) -> UiStart {
            let dirs = UiDirs {
                config: vec![self.config_dir()],
                packs: Ok(PackRoots {
                    site: self.site(),
                    lockfile: Some(self.0.path().join(LOCKFILE)),
                }),
            };
            UiHost::start(Interaction::None, true, files, dirs).unwrap()
        }
    }

    /// The UI runtime with the bundled plugins and no file of the user's.
    fn with_bundled() -> UiStart {
        UserFiles::new().start(UiFiles::Global)
    }

    fn command(name: &str, body: &str) -> String {
        format!(
            "maki.api.register_command({{ name = '{name}', description = '', \
             handler = function() {body} end }})"
        )
    }

    fn commands(host: &UiHost) -> BTreeSet<String> {
        host.command_reader()
            .load()
            .commands
            .iter()
            .map(|command| command.name.to_string())
            .collect()
    }

    #[test]
    fn bundled_ui_plugins_register_their_commands_and_keymaps() {
        let UiStart { host, .. } = with_bundled();
        let expected: BTreeSet<String> = BUNDLED_COMMANDS.map(str::to_owned).into();
        assert_eq!(commands(&host), expected);
        let keymaps = host.keymap_reader().load();
        for (plugin, key) in BUNDLED_KEYS {
            let key = Key::parse(key).unwrap();
            assert!(
                keymaps
                    .entries
                    .iter()
                    .any(|entry| entry.key == key && &*entry.plugin == plugin),
                "{plugin} did not bind {key:?}"
            );
        }
    }

    /// `ui.lua` sets what the client reads and declares a package the UI
    /// host installs into its own site, whose UI entry runs once the UI
    /// component is approved and not before.
    #[test_case(true ; "approved")]
    #[test_case(false ; "unapproved")]
    fn a_package_ui_lua_declares_loads_once_its_ui_is_approved(approved: bool) {
        let files = UserFiles::new();
        files.ui_lua(&declaring_ui_lua());
        files.installed_package();
        if approved {
            files.approve_ui();
        }
        let started = files.start(UiFiles::Global);
        assert!(!started.ui_config.scrollbar);
        assert_eq!(started.input_history_size, HISTORY_SIZE);
        let agent_only = format!("{AGENT_SECTION} {AGENT_ONLY_IN_UI_LUA}");
        assert!(
            started.warnings.contains(&agent_only),
            "{:?}",
            started.warnings
        );
        assert_eq!(commands(&started.host).contains(PACKAGE_COMMAND), approved);
        assert_eq!(
            started
                .warnings
                .iter()
                .any(|warning| warning == UI_APPROVAL_REQUIRED),
            !approved,
            "{:?}",
            started.warnings
        );
    }

    /// A hand-installed package nobody declared runs its UI entry when its
    /// manifest speaks for the UI component.
    #[test_case(UI_MANIFEST, true ; "with_a_ui_table")]
    #[test_case(AGENT_ONLY_MANIFEST, false ; "without_a_ui_table")]
    fn a_discovered_package_loads_its_ui_entry_when_eligible(manifest: &str, expected: bool) {
        let files = UserFiles::new();
        files.ui_package(&files.site().join(HAND_INSTALLED), manifest);
        let started = files.start(UiFiles::Global);
        assert_eq!(commands(&started.host).contains(PACKAGE_COMMAND), expected);
        let undeclared = format!("{PACKAGE}: {UNDECLARED_UI_ENTRY_WARNING}");
        assert_eq!(started.warnings.contains(&undeclared), !expected);
    }

    #[test_case(true, "shown = true", None ; "configured")]
    #[test_case(false, "shown = true", None ; "disabled")]
    #[test_case(true, "shown = 'bad'", Some(OPTION_TYPE_ERR) ; "wrong_type")]
    #[test_case(true, "typo = true", Some(UNKNOWN_OPTION_ERR) ; "unknown_key")]
    fn ui_packages_use_the_ui_config(enabled: bool, fields: &str, error: Option<&str>) {
        let files = UserFiles::new();
        let root = files.site().join(HAND_INSTALLED);
        files.ui_package(&root, "[permissions]\n[ui]\n");
        files.write(&root.join("plugin/init.lua"), "");
        files.write(&root.join(UI_ENTRY), &format!(
            "local opts = maki.api.register_options({{ shown = {{ default = true, desc = 'Shown' }} }})\nassert(opts.shown)\n{}",
            command(PACKAGE_COMMAND, ""),
        ));
        files.ui_lua(&format!(
            "maki.setup({{ plugins = {{ {PACKAGE} = {{ enabled = {enabled}, {fields} }} }} }})"
        ));
        let started = files.start(UiFiles::Global);
        assert_eq!(
            commands(&started.host).contains(PACKAGE_COMMAND),
            enabled && error.is_none()
        );
        if let Some(error) = error {
            assert!(
                started
                    .warnings
                    .iter()
                    .any(|warning| warning.contains(error)),
                "{:?}",
                started.warnings
            );
        }
    }

    /// `--no-plugins` runs no `ui.lua`, loads no package and no bundled
    /// plugin.
    #[test]
    fn no_plugins_loads_nothing() {
        let files = UserFiles::new();
        files.ui_lua(&declaring_ui_lua());
        files.installed_package();
        files.approve_ui();
        files.ui_package(&files.site().join(HAND_INSTALLED), UI_MANIFEST);
        let started = files.start(UiFiles::Disabled);
        assert!(commands(&started.host).is_empty());
        assert!(started.host.keymap_reader().load().entries.is_empty());
        assert!(started.ui_config.scrollbar);
        assert!(started.warnings.is_empty(), "{:?}", started.warnings);
    }

    /// A project `ui.lua` runs only in a folder the client trusts.
    #[test_case(true ; "trusted")]
    #[test_case(false ; "untrusted")]
    fn a_project_ui_lua_runs_only_when_trusted(trusted: bool) {
        let project = TempDir::new().unwrap();
        fs::create_dir(project.path().join(".git")).unwrap();
        let ui_lua = project.path().join(PROJECT_UI_LUA);
        fs::create_dir_all(ui_lua.parent().unwrap()).unwrap();
        fs::write(&ui_lua, NO_SCROLLBAR_UI_LUA).unwrap();
        let project_config = if trusted {
            ProjectConfig::for_project(project.path())
        } else {
            ProjectConfig::discover(project.path())
        };
        let started = UserFiles::new().start(UiFiles::resolve(&project_config, false));
        assert_eq!(started.ui_config.scrollbar, !trusted);
    }

    #[test_case("maki.api.register_tool({})", "api", "register_tool" ; "api_register_tool")]
    fn an_agent_name_fails_and_another_plugin_still_loads(
        source: &str,
        namespace: &str,
        name: &str,
    ) {
        let UiStart { host, .. } = with_bundled();
        let err = host.load_source(AGENT_CALLER, source).unwrap_err();
        assert!(
            err.to_string()
                .contains(&not_in_ui_runtime(namespace, name)),
            "{err}"
        );
        host.load_source(OTHER, &command(OTHER_COMMAND, ""))
            .unwrap();
        assert!(commands(&host).contains(OTHER_COMMAND));
    }

    /// The local answer is missing, and the UI's goes to `UiOps`, so the
    /// event loop answers it.
    #[test]
    fn facade_calls_answer_without_a_local_backend() {
        let host = UiHost::bare();
        let source = command(
            PROBE_COMMAND,
            "local _, info_err = maki.model.info('a/b')\n\
             maki.session.list()\n\
             maki.ui.flash(info_err)",
        );
        host.load_source(PROBE, &source).unwrap();
        let actions = host.ui_action_rx();
        host.ui_handle()
            .run_command(Arc::from(PROBE), Arc::from(PROBE_COMMAND), String::new(), 0);
        let mut listed = false;
        let flashed = loop {
            match actions.recv_timeout(WAIT).unwrap() {
                UiAction::Session {
                    req: SessionRequest::List,
                    reply_tx,
                } => {
                    listed = true;
                    reply_tx.send(Ok(json!([]))).unwrap();
                }
                UiAction::Flash(text) => break text,
                _ => continue,
            }
        };
        assert!(listed, "session.list never reached the UI");
        assert_eq!(flashed, NO_LOCAL_ERR);
    }

    /// Every tab is remote here, named by its agent's session id, which the
    /// event loop resolves: the id goes as given, where the combined
    /// runtime refuses one that is not a local id.
    #[test]
    fn a_session_option_goes_to_the_ui_as_given() {
        let host = UiHost::bare();
        let source = command(
            PROBE_COMMAND,
            "local id = maki.session.current()\n\
             maki.ui.set_status_hint({ { 'q', 'quit' } }, { session = id })\n\
             maki.ui.open_win(maki.ui.buf({ scratch = true }), { session = id })",
        );
        host.load_source(PROBE, &source).unwrap();
        let actions = host.ui_action_rx();
        host.ui_handle()
            .run_command(Arc::from(PROBE), Arc::from(PROBE_COMMAND), String::new(), 0);
        let named = Some(TabRef(Arc::from(AGENT_SESSION)));
        let opened = loop {
            match actions.recv_timeout(WAIT).unwrap() {
                UiAction::Session {
                    req: SessionRequest::Current,
                    reply_tx,
                } => reply_tx.send(Ok(json!(AGENT_SESSION))).unwrap(),
                UiAction::OpenWin { session, .. } => break session,
                _ => continue,
            }
        };
        assert_eq!(opened, named);
        let hints = host.hint_reader().load_full();
        assert_eq!(hints.entries[0].session, named);
    }

    /// Export and messages carry the id unparsed: a remote tab's is its
    /// agent's, and the event loop answers for it.
    #[test]
    fn export_and_messages_ask_the_ui_with_an_agents_id() {
        let host = UiHost::bare();
        let source = command(
            PROBE_COMMAND,
            &format!(
                "local _, export_err = maki.session.export({{ session = '{AGENT_SESSION}' }})\n\
                 local _, messages_err = maki.session.messages({{ session = '{AGENT_SESSION}' }})\n\
                 maki.ui.flash(export_err .. '|' .. messages_err)"
            ),
        );
        host.load_source(PROBE, &source).unwrap();
        let actions = host.ui_action_rx();
        host.ui_handle()
            .run_command(Arc::from(PROBE), Arc::from(PROBE_COMMAND), String::new(), 0);
        let flashed = loop {
            match actions.recv_timeout(WAIT).unwrap() {
                UiAction::ExportSession { id, reply_tx } => {
                    assert_eq!(id, AGENT_SESSION);
                    reply_tx.send(Err(REFUSED.to_owned())).unwrap();
                }
                UiAction::Flash(text) => break text,
                _ => continue,
            }
        };
        assert_eq!(flashed, format!("{REFUSED}|{REFUSED}"));
    }
    fn drawn_bundled(call: RenderCall, clicks: Vec<usize>) -> String {
        let UiStart { host, .. } = with_bundled();
        drawn_by(&host, call, clicks)
    }

    fn drawn_by(host: &UiHost, call: RenderCall, clicks: Vec<usize>) -> String {
        let ctx = RenderCtx {
            cwd: PathBuf::from("/agent/workspace"),
            home: None,
            tool_output_lines: ToolOutputLines {
                bash: 2,
                code_execution: 2,
                ..ToolOutputLines::default()
            },
            width: 80,
            reason: RenderReason::Load,
        };
        match host.ui_handle().redraw(call, ctx, clicks).recv().unwrap() {
            RenderReply::Drawn {
                body: Some(body), ..
            } => body.text(),
            other => panic!("renderer did not draw: {other:?}"),
        }
    }

    fn bundled_call(tool: &str, status: ChildStatus) -> RenderCall {
        RenderCall {
            id: RENDER_CALL.into(),
            data: Arc::new(ToolData::new(
                Arc::from(tool),
                DATA_VERSION,
                json!({ "command": RENDER_COMMAND, "code": RENDER_COMMAND }),
                tool.into(),
            )),
            output: Some(Arc::new(ToolOutput::Plain(RENDER_TEXT.into()))),
            status,
            streamed: Vec::new(),
        }
    }

    #[test_case("bash", ChildStatus::Running ; "running_bash")]
    #[test_case("bash", ChildStatus::Done ; "finished_bash")]
    #[test_case("code_execution", ChildStatus::Running ; "running_python")]
    #[test_case("code_execution", ChildStatus::Done ; "finished_python")]
    #[test_case("bash", ChildStatus::Error ; "failed_bash")]
    #[test_case("code_execution", ChildStatus::Error ; "failed_python")]
    fn execution_renderers_keep_the_command_and_output(tool: &str, status: ChildStatus) {
        let mut call = bundled_call(tool, status);
        Arc::make_mut(&mut call.data).state = json!({ "text": RENDER_TEXT });
        let text = drawn_bundled(call, Vec::new());
        assert!(
            text.contains(RENDER_COMMAND) && text.contains(RENDER_TEXT),
            "{text}"
        );
    }

    #[test_case(None, None, RENDER_COMMAND, "" ; "no_description")]
    #[test_case(Some(RENDER_COMMAND), None, RENDER_COMMAND, "" ; "command_is_not_a_description")]
    #[test_case(Some("print hi"), Some("/agent/workspace"), RENDER_COMMAND, "print hi" ; "current_workdir_is_hidden")]
    #[test_case(Some("print hi"), None, "cd src && echo hi", "print hi in src" ; "description_and_directory")]
    #[test_case(Some("   "), None, "cd src && echo hi", "in src" ; "blank_description_with_directory")]
    fn bash_renderer_keeps_the_descriptive_title_and_directory(
        description: Option<&str>,
        workdir: Option<&str>,
        command: &str,
        expected: &str,
    ) {
        let UiStart { host, .. } = with_bundled();
        let mut call = bundled_call("bash", ChildStatus::Done);
        Arc::make_mut(&mut call.data).input =
            json!({ "command": command, "description": description, "workdir": workdir });
        let ctx = RenderCtx {
            cwd: PathBuf::from("/agent/workspace"),
            home: None,
            tool_output_lines: ToolOutputLines::default(),
            width: 80,
            reason: RenderReason::Load,
        };
        let reply = host
            .ui_handle()
            .redraw(call, ctx, Vec::new())
            .recv()
            .unwrap();
        let RenderReply::Drawn { header, .. } = reply else {
            panic!("renderer did not draw: {reply:?}")
        };
        assert_eq!(
            header.map(|header| header.text()).unwrap_or_default(),
            expected
        );
    }

    #[test_case(Vec::new(), false ; "collapsed")]
    #[test_case(vec![0], true ; "expanded")]
    fn execution_output_can_expand(clicks: Vec<usize>, expanded: bool) {
        let mut call = bundled_call("bash", ChildStatus::Done);
        call.output = Some(Arc::new(ToolOutput::Plain(
            format!("{FIRST_LINE}\nsecond\nthird\nfourth\n{LAST_LINE}").into(),
        )));
        let text = drawn_bundled(call, clicks);
        assert_eq!(text.contains(FIRST_LINE), expanded, "{text}");
        assert!(text.contains(LAST_LINE), "{text}");
    }

    #[test]
    fn batch_pairs_dispatched_children_with_their_diffs_after_a_rejection() {
        let mut call = bundled_call("batch", ChildStatus::Done);
        let data = Arc::make_mut(&mut call.data);
        data.state = json!({ "children": [
            { "tool": "batch", "status": "error", "output": NESTED_BATCH_ERROR },
            { "tool": "edit", "status": "success", "index": 1 },
        ] });
        data.children.push(ToolChild {
            index: 0,
            status: ChildStatus::Done,
            data: ToolData::new(
                Arc::from("edit"),
                DATA_VERSION,
                json!({}),
                RENDER_TEXT.into(),
            ),
            output: Some(Arc::new(ToolOutput::Diff {
                path: "file.txt".into(),
                before: DIFF_BEFORE.into(),
                after: DIFF_AFTER.into(),
                summary: RENDER_TEXT.into(),
            })),
            usage: None,
        });
        let text = drawn_bundled(call, Vec::new());
        assert!(text.contains(NESTED_BATCH_ERROR), "{text}");
        assert!(text.contains("- old") && text.contains("+ new"), "{text}");
    }

    #[test]
    fn question_renderer_keeps_the_question_and_answer() {
        let mut call = bundled_call("question", ChildStatus::Done);
        Arc::make_mut(&mut call.data).input =
            json!({ "questions": [{ "question": RENDER_COMMAND }] });
        let text = drawn_bundled(call, Vec::new());
        assert!(
            text.contains(RENDER_COMMAND) && text.contains(RENDER_TEXT),
            "{text}"
        );
    }

    #[test]
    fn todo_renderer_keeps_the_list_when_the_model_output_is_empty() {
        let mut call = bundled_call("todo_write", ChildStatus::Done);
        Arc::make_mut(&mut call.data).input =
            json!({ "todos": [{ "content": RENDER_TEXT, "status": "completed" }] });
        call.output = Some(Arc::new(ToolOutput::Plain("".into())));
        let text = drawn_bundled(call, Vec::new());
        assert!(text.contains(RENDER_TEXT) && text.contains("[✓]"), "{text}");
    }
    #[test_case(false ; "collapsed")]
    #[test_case(true ; "expanded")]
    fn execution_script_can_expand_beyond_its_initial_cap(expanded: bool) {
        let mut call = bundled_call("code_execution", ChildStatus::Done);
        Arc::make_mut(&mut call.data).input =
            json!({ "code": format!("{}{LAST_SCRIPT_LINE}", "pass\n".repeat(MAX_SCRIPT_LINES)) });
        let text = drawn_bundled(call, if expanded { vec![0] } else { Vec::new() });
        assert_eq!(text.contains(LAST_SCRIPT_LINE), expanded);
    }

    #[test]
    fn batch_waits_for_a_child_renderer_before_copying_its_body() {
        let UiStart { host, .. } = with_bundled();
        host.load_source(DEFERRED_TOOL, DEFERRED_RENDERER).unwrap();
        let mut call = bundled_call("batch", ChildStatus::Done);
        let data = Arc::make_mut(&mut call.data);
        data.state =
            json!({ "children": [{ "tool": DEFERRED_TOOL, "status": "success", "index": 1 }] });
        data.children.push(ToolChild {
            index: 0,
            status: ChildStatus::Done,
            data: ToolData::new(
                Arc::from(DEFERRED_TOOL),
                DATA_VERSION,
                json!({}),
                DEFERRED_TOOL.into(),
            ),
            output: Some(Arc::new(ToolOutput::Plain("".into()))),
            usage: None,
        });
        let text = drawn_by(&host, call, Vec::new());
        assert!(text.contains(RENDER_TEXT), "{text}");
    }
    #[test]
    fn batch_keeps_partial_output_when_a_cancelled_child_has_no_final_reply() {
        let mut call = bundled_call("batch", ChildStatus::Done);
        let data = Arc::make_mut(&mut call.data);
        data.state = json!({ "children": [{ "tool": "bash", "status": "error", "index": 1, "output": CANCELLED_CHILD }] });
        let mut child = ToolData::new(
            Arc::from("bash"),
            DATA_VERSION,
            json!({ "command": RENDER_COMMAND }),
            RENDER_COMMAND.into(),
        );
        child.state = json!({ "text": RENDER_TEXT });
        data.children.push(ToolChild {
            index: 0,
            status: ChildStatus::Cancelled,
            data: child,
            output: None,
            usage: None,
        });
        let text = drawn_bundled(call, Vec::new());
        assert!(
            text.contains(RENDER_TEXT) && text.contains(CANCELLED_CHILD),
            "{text}"
        );
    }
}
