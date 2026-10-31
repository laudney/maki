//! The steps a host takes to load plugins into a running core: config files,
//! bundled entries, package directories and custom loaders, and the
//! `packadd` rounds. Whatever a composition decides on its own, such as the
//! grant a load carries, comes from its [`ComponentPolicy`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(any(test, feature = "test-support"))]
use std::sync::LazyLock;
use tokio::runtime::EnterGuard;
#[cfg(any(test, feature = "test-support"))]
use tokio::runtime::{Builder, Handle, Runtime, RuntimeFlavor};

use maki_config::{PluginsConfig, RawConfig};

use crate::api::options::PluginOpts;
use crate::api::pack::{Declared, LoadMode};
use crate::api::util::setup::Setup;
use crate::bundled::{self, Bundled, BundledPlugin};
use crate::error::PluginError;
use crate::handle::CoreHandle;
use crate::pack::{
    ComponentApprovals, DiscoveredPackage, Origin, eligible, granted, ineligible_warning,
};
use crate::plugin_permissions::{Component, PluginPermissions, check_plugin_compatibility};
use crate::runtime::{ConfigScope, DomainGrant, LoadChunk, LoadContext, LoadStep, Request};

#[cfg(any(test, feature = "test-support"))]
pub static HOST_RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime for plugin hosts")
});

#[cfg(any(test, feature = "test-support"))]
pub fn enter_test_runtime() -> Option<EnterGuard<'static>> {
    let on_workers = Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == RuntimeFlavor::MultiThread);
    (!on_workers).then(|| HOST_RUNTIME.enter())
}

#[cfg(not(any(test, feature = "test-support")))]
pub fn enter_test_runtime() -> Option<EnterGuard<'static>> {
    None
}

pub const NO_ENTRY_ERR: &str = "bundled plugin has neither init.lua nor ui.lua";
/// What a host that loads the UI says of a package whose `ui/` entry its
/// manifest does not speak for, after the package's name.
pub const UNDECLARED_UI_ENTRY_WARNING: &str =
    "its ui/ entry has no [ui] table in plugin.toml, so it was not loaded";
/// A package's agent entry directory.
const AGENT_ENTRY_DIR: &str = "plugin";
/// A package's UI entry directory.
const UI_ENTRY_DIR: &str = "ui";
const LUA_EXT: &str = "lua";
pub const SKIPPED_PLUGIN_WARNING: &str = "skipping plugin lua";
pub const TRUST_SCOPE_WARNING: &str =
    "trust is only read from the global init.lua; ignoring the trust table in";
pub const INSTRUCTIONS_SCOPE_WARNING: &str =
    "instructions is only read from the global init.lua; ignoring the instructions table in";
/// What a section `ui.lua` set that the UI does not read says, after it.
pub const AGENT_ONLY_IN_UI_LUA: &str = "in ui.lua does nothing: only the agent reads it";

/// What a load is, for the grant its composition gives it.
pub enum LoadKind {
    /// A plugin that ships inside the binary.
    Bundled,
    /// The user's own config file.
    Config,
    /// A third-party package, with the provider slugs it was granted.
    Package(Arc<[String]>),
}

/// What a composition decides on its own while the host steps load.
pub trait ComponentPolicy {
    /// The domain grant one load of {kind} carries.
    fn grant(&self, kind: LoadKind) -> DomainGrant;

    /// Runs before {package} loads, whether the load then succeeds or not.
    fn before_package_load(&self, package: &str, granted_providers: &[String]);

    /// A warning about what {plugin} registered, after it loaded.
    fn load_warning(&self, plugin: &str) -> Option<String>;
}

pub fn has_entry(plugin: &BundledPlugin) -> bool {
    Component::ALL
        .iter()
        .any(|&component| plugin.dir.get_file(bundled::entry(component)).is_some())
}

/// {component}'s entry directory in a package.
const fn package_entry_dir(component: Component) -> &'static str {
    match component {
        Component::Agent => AGENT_ENTRY_DIR,
        Component::Ui => UI_ENTRY_DIR,
    }
}

/// A package's entrypoints for {component}: every `*.lua` in its entry
/// directory, sorted by filename so load order is deterministic across
/// machines.
///
/// A repository can commit a symlink, so each entry is resolved and checked to
/// be inside the package before it is read.
fn package_entrypoints(root: &Path, component: Component) -> Result<Vec<PathBuf>, PluginError> {
    let entrypoint_dir = root.join(package_entry_dir(component));
    let entries = match fs::read_dir(&entrypoint_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(PluginError::Io {
                path: entrypoint_dir,
                source,
            });
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| PluginError::Io {
            path: entrypoint_dir.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(LUA_EXT) {
            continue;
        }
        let resolved = path.canonicalize().map_err(|e| PluginError::Io {
            path: path.clone(),
            source: e,
        })?;
        if !resolved.starts_with(root) {
            return Err(PluginError::PackageEscape { path });
        }
        if resolved.is_file() {
            let Some(file_name) = path.file_name().map(std::ffi::OsString::from) else {
                continue;
            };
            files.push((file_name, resolved));
        }
    }
    // By file name, not by the resolved path: a package may symlink an entry
    // elsewhere inside itself, and load order must still be the order a user
    // sees in the entry directory.
    files.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(files.into_iter().map(|(_, path)| path).collect())
}

/// Whether the package at {root} lists an entry file for {component},
/// judged from the names and file types alone so nothing of an entry that
/// may not run is read. A directory named like an entry is not one, as
/// [`package_entrypoints`] skips it; a directory that cannot be listed, or
/// a link that does not resolve, counts, so reading it reports why.
fn lists_entries(root: &Path, component: Component) -> bool {
    match fs::read_dir(root.join(package_entry_dir(component))) {
        Ok(entries) => entries.flatten().any(|entry| {
            let path = entry.path();
            path.extension().is_some_and(|ext| ext == LUA_EXT) && !path.is_dir()
        }),
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

/// {component}'s entry in the package at {root}, as steps tagged with it.
fn package_entry_steps(root: &Path, component: Component) -> Result<Vec<LoadStep>, PluginError> {
    package_entrypoints(root, component)?
        .into_iter()
        .map(|path| {
            let source = fs::read_to_string(&path).map_err(|e| PluginError::Io {
                path: path.clone(),
                source: e,
            })?;
            let chunk = LoadChunk::new(path.display().to_string(), source);
            Ok(chunk.into())
        })
        .collect()
}

/// What one package load runs and carries, besides its options.
struct PackageLoad {
    root: PathBuf,
    steps: Vec<LoadStep>,
    permissions: PluginPermissions,
    granted_providers: Arc<[String]>,
    revision_guard: Option<Arc<maki_pack::lock::Lock>>,
}

/// The host steps over one running core, the plugins its binary ships and
/// its composition's policy.
pub struct HostSteps<'a> {
    core: CoreHandle,
    bundled: &'static Bundled,
    policy: &'a dyn ComponentPolicy,
}

impl<'a> HostSteps<'a> {
    pub fn new(
        core: CoreHandle,
        bundled: &'static Bundled,
        policy: &'a dyn ComponentPolicy,
    ) -> Self {
        Self {
            core,
            bundled,
            policy,
        }
    }

    /// Sends the request {build} makes around a reply sender, and waits for
    /// the reply.
    fn ask<T>(&self, build: impl FnOnce(flume::Sender<T>) -> Request) -> Result<T, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        if !self.core.send(build(reply_tx)) {
            return Err(PluginError::HostDead);
        }
        reply_rx.recv().map_err(|_| PluginError::HostDead)
    }

    /// Runs the config file at {path} in {scope}, merging what it set into
    /// {merged}. A missing file runs nothing.
    pub fn run_config_file(
        &self,
        path: &Path,
        scope: ConfigScope,
        merged: &mut Option<RawConfig>,
        warnings: &mut Vec<String>,
    ) -> Result<(), PluginError> {
        if !path.is_file() {
            return Ok(());
        }
        let source = fs::read_to_string(path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let plugin_dir = path.parent().map(Path::to_path_buf);
        if let Err(e) = check_plugin_compatibility(scope.label(), plugin_dir.as_deref()) {
            warnings.push(format!("{SKIPPED_PLUGIN_WARNING}: {e}"));
            return Ok(());
        }
        let owner = scope.label().to_owned();
        let global = matches!(scope, ConfigScope::Global);
        let Setup { raw, agent_only } = self.run_config_lua(source, scope, plugin_dir)?;
        warnings.extend(
            agent_only
                .into_iter()
                .map(|section| format!("{section} {AGENT_ONLY_IN_UI_LUA}")),
        );
        if let Some(mut raw) = raw {
            // A folder cannot vouch for itself: ACP resolves many
            // client-chosen cwds against the config it read once at startup,
            // so one trusted project's `trust.paths` would reach folders
            // nobody ever trusted. The same holds for `instructions`, which
            // picks what a folder's checkout loads into the model. Stripped
            // rather than rejected, because there is no fallback config at
            // this point and a hard error would brick the folder the user
            // just trusted over an ignored setting.
            if !global {
                if std::mem::take(&mut raw.trust).is_set() {
                    warnings.push(format!("{TRUST_SCOPE_WARNING} {owner}"));
                }
                if std::mem::take(&mut raw.instructions).is_set() {
                    warnings.push(format!("{INSTRUCTIONS_SCOPE_WARNING} {owner}"));
                }
            }
            match merged {
                Some(existing) => existing.merge(raw),
                None => *merged = Some(raw),
            }
        }
        warnings.extend(self.policy.load_warning(&owner));
        Ok(())
    }

    /// Runs {source} as a config chunk in {scope}.
    pub fn run_config_lua(
        &self,
        source: String,
        scope: ConfigScope,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Setup, PluginError> {
        self.ask(|reply| Request::RunInitLua {
            source,
            scope,
            plugin_dir,
            grant: self.policy.grant(LoadKind::Config),
            reply,
        })?
    }

    /// Load the bundled entries enabled for this VM.
    pub fn load_bundled(
        &self,
        config: &PluginsConfig,
        component: Component,
    ) -> Result<(), PluginError> {
        for (plugin, opts) in &config.opts {
            // An enabled package takes its options when the package itself
            // loads, and an enabled builtin takes them in the loop below.
            if config.is_enabled(plugin) {
                continue;
            }
            // What is left is a name that exists but is not loading: a builtin
            // or a package the config disabled, or one discovery refused. It
            // cannot be a typo, because the config layer validated every
            // `plugins.<name>` key against the same names before this ran. A
            // package used to reach this as an error, which stopped maki from
            // starting over options it was already ignoring.
            let keys: Vec<&str> = opts.keys().map(String::as_str).collect();
            tracing::warn!(
                plugin = plugin.as_str(),
                keys = keys.join(", "),
                "nothing named {} is loading; its plugins.{} options are ignored",
                plugin,
                plugin
            );
        }
        if let Some(unknown) = config
            .names
            .iter()
            .find(|name| !self.bundled.is_bundled(name))
        {
            return Err(PluginError::UnknownPlugin {
                plugin: unknown.clone(),
            });
        }
        // `BUNDLED_PLUGINS` order, not `config.names` order, because a rule can
        // only name a registered tool, so whoever owns a tool loads before
        // whoever pre-approves it. `DEFAULT_BUILTINS` stays alphabetical for
        // the config surface.
        for bundled in self.bundled.plugins() {
            if !config.is_enabled(bundled.name) {
                continue;
            }
            if !has_entry(bundled) {
                return Err(PluginError::Lua {
                    plugin: bundled.name.to_owned(),
                    source: mlua::Error::runtime(NO_ENTRY_ERR),
                });
            }
            let Some(chunk) = bundled.entry_chunk(component) else {
                continue;
            };
            let steps = vec![chunk.into()];
            let permissions = bundled.permissions(component)?;
            let name: Arc<str> = Arc::from(bundled.name);
            let opts = Arc::new(config.opts.get(bundled.name).cloned().unwrap_or_default());
            self.send_load(
                Arc::clone(&name),
                steps,
                LoadContext {
                    opts,
                    ..LoadContext::plain(None, permissions, self.policy.grant(LoadKind::Bundled))
                },
            )?;
        }
        Ok(())
    }

    pub fn send_load(
        &self,
        name: Arc<str>,
        steps: Vec<LoadStep>,
        context: LoadContext,
    ) -> Result<(), PluginError> {
        self.ask(|reply| Request::LoadSource {
            name,
            steps,
            context,
            reply,
        })?
    }

    /// Packages declared by `maki.pack.add` in the config files that ran,
    /// each with the policy every declaring file set for its component.
    pub fn declared_packages(&self) -> Result<Vec<Declared>, PluginError> {
        self.ask(|reply| Request::CollectPackages { reply })
    }

    /// {dir} as the root of package {name}, refused before anything of it
    /// is read when it may not load at all.
    fn package_root(&self, name: &str, dir: &Path) -> Result<PathBuf, PluginError> {
        // Refused here and not only in discovery, because loading an owner
        // drops that owner's existing registrations first. A package named
        // after a bundled plugin would unload the builtin before its own
        // entrypoint ever ran, so every caller has to be gated, not just the
        // one that walks the site directory.
        if self.bundled.is_bundled(name) {
            return Err(PluginError::PackageNameConflict {
                name: name.to_owned(),
                path: dir.to_path_buf(),
            });
        }
        // Resolved once here, so the manifest, the entrypoints, and later
        // `require` calls all agree on one directory even if the path they came
        // from changes underneath us.
        let root = dir.canonicalize().map_err(|e| PluginError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        // Gated next to the bundled-name refusal and before any chunk is read,
        // and once for every component, so a package that outran this Maki
        // registers nothing at all.
        check_plugin_compatibility(name, Some(&root))?;
        Ok(root)
    }

    fn send_package_load(
        &self,
        name: &str,
        load: PackageLoad,
        opts: PluginOpts,
    ) -> Result<(), PluginError> {
        self.send_load(
            Arc::from(name),
            load.steps,
            LoadContext {
                plugin_dir: Some(load.root),
                permissions: load.permissions,
                opts,
                revision_guard: load.revision_guard,
                package: true,
                grant: self.policy.grant(LoadKind::Package(load.granted_providers)),
            },
        )
    }

    /// Loads the agent entry of one external package directory as a single
    /// owner.
    ///
    /// Every `plugin/*.lua` becomes a chunk, and the chunks share one
    /// environment, so what one file registers the next can use. The whole set
    /// commits or none of it does.
    pub fn load_package(
        &self,
        name: &str,
        dir: &Path,
        permissions: PluginPermissions,
        granted_providers: Arc<[String]>,
        opts: PluginOpts,
        revision_guard: Option<Arc<maki_pack::lock::Lock>>,
    ) -> Result<(), PluginError> {
        let root = self.package_root(name, dir)?;
        let steps = package_entry_steps(&root, Component::Agent)?;
        if steps.is_empty() {
            return Err(PluginError::PackageEmpty {
                name: name.to_owned(),
                path: root,
            });
        }
        let load = PackageLoad {
            root,
            steps,
            permissions,
            granted_providers,
            revision_guard,
        };
        self.send_package_load(name, load, opts)
    }

    /// Load this VM's entry with only its component's approval and grant.
    fn load_discovered(
        &self,
        pkg: &DiscoveredPackage,
        declaration: Option<&Declared>,
        component: Component,
        approvals: &ComponentApprovals,
        config: &PluginsConfig,
        warnings: &mut Vec<String>,
    ) -> Result<(), PluginError> {
        let runs = declaration.is_none_or(|declared| declared.policy(component).is_some())
            && config.is_enabled(&pkg.name);
        let (permissions, providers) = granted(pkg, approvals.of(component), component);
        let granted_providers = if runs && component == Component::Agent {
            providers.into()
        } else {
            Arc::from([])
        };
        self.policy
            .before_package_load(&pkg.name, &granted_providers);
        let root = self.package_root(&pkg.name, &pkg.dir)?;
        let has_entry = |part| {
            declaration
                .and_then(|declared| declared.policy(part))
                .is_some_and(|policy| matches!(policy.load, LoadMode::Custom(_)))
                || lists_entries(&root, part)
        };
        if !has_entry(component) {
            if Component::ALL.into_iter().any(has_entry) {
                return Ok(());
            }
            return Err(PluginError::PackageEmpty {
                name: pkg.name.clone(),
                path: root,
            });
        }
        if !runs {
            return Ok(());
        }
        if !pkg.requested.declares(component) {
            warnings.push(format!("{}: {UNDECLARED_UI_ENTRY_WARNING}", pkg.name));
            return Ok(());
        }
        if component == Component::Ui && !eligible(pkg, approvals.of(component), component) {
            warnings.push(ineligible_warning(pkg, approvals.of(component), component));
            return Ok(());
        }
        let steps = match declaration.filter(|declared| {
            declared
                .policy(component)
                .is_some_and(|policy| matches!(policy.load, LoadMode::Custom(_)))
        }) {
            Some(declared) => vec![LoadStep::Loader {
                declared: declared.clone(),
            }],
            None => package_entry_steps(&root, component)?,
        };
        let load = PackageLoad {
            root,
            steps,
            permissions,
            granted_providers,
            revision_guard: pkg.revision_guard.clone(),
        };
        self.send_package_load(
            &pkg.name,
            load,
            Arc::new(config.opts.get(&pkg.name).cloned().unwrap_or_default()),
        )
    }

    /// Refuses further `maki.packadd` calls, and returns anything the queue
    /// still holds.
    ///
    /// One call and not a read followed by a close, because a Lua task can
    /// record an activation between the two and closing would strand it.
    pub fn seal_pack_ops(&self) -> Result<Vec<String>, PluginError> {
        self.ask(|reply| Request::SealPackOps { reply })
    }

    /// Takes the package operations Lua recorded, leaving the queue empty.
    ///
    /// Called by the host after the initiating task has exited, which keeps a
    /// load off the thread that requested it.
    fn take_pending_pack_ops(&self) -> Result<Vec<String>, PluginError> {
        self.ask(|reply| Request::TakePackOps { reply })
    }

    /// Load eager packages and those activated by `packadd`. Collect new
    /// activations after each round, because one package can activate another.
    pub fn load_declared_packages(
        &self,
        packages: &[DiscoveredPackage],
        declared: &[Declared],
        config: &PluginsConfig,
        component: Component,
        approvals: &ComponentApprovals,
    ) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut loaded: Vec<&str> = Vec::new();
        let declaration_of = |pkg: &DiscoveredPackage| {
            declared.iter().find(|declaration| {
                declaration.spec.name == pkg.name
                    && matches!(&pkg.origin, Origin::Fetched { src } if src == &declaration.spec.src)
            })
        };
        // Whether a component of {pkg} may load here, and with {eager_only}
        // one that loads at startup.
        let loads = |pkg: &DiscoveredPackage, eager_only: bool| {
            let declaration = declaration_of(pkg);
            {
                config.is_enabled(&pkg.name)
                    && match declaration {
                        Some(declaration) => declaration
                            .policy(component)
                            .is_some_and(|policy| !eager_only || policy.load.is_eager()),
                        None => !eager_only || pkg.eager,
                    }
            }
        };
        let mut round: Vec<&DiscoveredPackage> =
            packages.iter().filter(|pkg| loads(pkg, true)).collect();

        // A `loop` and not `while !round.is_empty()`: with no `start` package
        // installed the first round is empty, and the names `init.lua` already
        // recorded still have to be collected.
        loop {
            for pkg in round {
                loaded.push(&pkg.name);
                let result = self.load_discovered(
                    pkg,
                    declaration_of(pkg),
                    component,
                    approvals,
                    config,
                    &mut warnings,
                );
                match result {
                    Ok(()) => warnings.extend(self.policy.load_warning(&pkg.name)),
                    Err(e) if e.is_version_floor() => {
                        tracing::warn!(
                            package = %pkg.name,
                            path = %pkg.dir.display(),
                            error = %e,
                            "{SKIPPED_PLUGIN_WARNING}"
                        );
                        warnings.push(format!("{SKIPPED_PLUGIN_WARNING}: {e}"));
                    }
                    Err(e) => {
                        tracing::error!(
                            package = %pkg.name,
                            path = %pkg.dir.display(),
                            error = %e,
                            "failed to load package"
                        );
                        warnings.push(format!("{}: failed to load: {e}", pkg.name));
                    }
                }
            }

            let ops = match self.take_pending_pack_ops() {
                Ok(ops) => ops,
                Err(e) => {
                    warnings.push(format!("could not read package activations: {e}"));
                    break;
                }
            };
            round = Vec::new();
            for name in ops {
                if loaded.contains(&name.as_str()) {
                    continue;
                }
                // Refused rather than loaded when the config disabled it, so
                // `packadd` cannot be a way around `plugins.<name>.enabled`.
                let found = packages
                    .iter()
                    .find(|pkg| pkg.name == name && loads(pkg, false));
                match found {
                    Some(pkg) => round.push(pkg),
                    None => warnings.push(format!(
                        "packadd {name:?}: no package with that name is installed"
                    )),
                }
            }
            if round.is_empty() {
                break;
            }
        }
        // Nothing drains the queue after this, so `packadd` is closed rather
        // than left accepting names no one will read. Closing returns whatever
        // arrived since the last round, which is reported rather than dropped:
        // that request was going to be honoured a moment earlier.
        match self.seal_pack_ops() {
            Ok(leftover) => {
                for name in leftover {
                    warnings.push(format!(
                        "packadd {name:?}: arrived after the packages had loaded"
                    ));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not close the package activation queue");
            }
        }
        warnings
    }
}
