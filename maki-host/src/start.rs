use std::env;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::Context;

use maki_agent::tools::ToolRegistry;
use maki_config::project::{self, TrustMode};
use maki_config::{Config, load_env_files, load_permissions};
use maki_lua::{DiscoveredPackage, InitFiles, Interaction, PluginHost};
use maki_storage::{StateDir, logging};

use crate::HostFlags;
use crate::setup;

pub fn sanitize_warnings(warnings: &[String]) -> Vec<String> {
    warnings
        .iter()
        .map(String::as_str)
        .map(maki_lua::sanitize_message)
        .collect()
}

pub fn stop_plugins(host: &mut PluginHost) {
    host.begin_shutdown();
    ToolRegistry::global().clear_lua();
}

pub fn config_or_fallback(
    loaded: Result<Config>,
    fallback: Option<Config>,
    warnings: &mut Vec<String>,
) -> Result<Config> {
    match (loaded, fallback) {
        (Ok(config), _) => Ok(config),
        (Err(error), Some(last_good)) => {
            warnings.push(format!(
                "config reload failed, using previous config: {error:#}"
            ));
            Ok(last_good)
        }
        (Err(error), None) => Err(error),
    }
}

pub fn report_warnings(warnings: Vec<String>) {
    for warning in sanitize_warnings(&warnings) {
        eprintln!("warning: {warning}");
    }
}

/// What a builtin that fails to load means for the run in progress.
#[derive(Clone, Copy)]
pub enum BuiltinFailure {
    /// Startup has nothing to fall back on.
    Fatal,
    /// `/reload` keeps the open UI alive, so the failure is only reported.
    Warn,
}

/// Discovered package names plus the ones `init.lua` or `ui.lua` declared
/// with `maki.pack.add`. Resolved by `build_config` instead of handed to it, because
/// the declared set is only complete once the init files have run, and
/// validation would otherwise reject a `plugins.<name>` for a package the user
/// just declared.
pub type KnownNames<'a> = dyn Fn(&PluginHost) -> Result<Vec<String>> + 'a;

/// The plugin startup every entry point shares. The host starts in
/// `composition`, packages are discovered before `build_config` runs, so
/// `plugins.<name>` can configure an installed package, declared ones are
/// installed after it, and everything is loaded after the builtins, so a
/// package claiming a builtin tool name is the side that fails.
///
/// `interaction` decides whether an install that needs the user's confirmation
/// may ask for it or has to fail; only the interactive UI can answer.
///
/// Warnings are returned sanitized, leaving the sink to the caller; the extra
/// `Vec` handed to `build_config` is for warnings raised while building it.
pub fn load_plugins(
    no_jit: bool,
    no_plugins: bool,
    on_builtin_failure: BuiltinFailure,
    interaction: Interaction,
    build_config: impl FnOnce(&PluginHost, &KnownNames<'_>, &mut Vec<String>) -> Result<Config>,
) -> Result<(PluginHost, Config, Vec<String>)> {
    let mut host = PluginHost::start(Arc::clone(ToolRegistry::global_arc()), !no_jit)
        .context("initialize lua plugin host")?;

    // Opens the provider registration window and drops what the previous load
    // registered: on a `/reload` this host is a new one, and an entry the old
    // one left behind answers on a channel nobody serves.
    maki_providers::plugin::begin_load();
    let legacy_warnings = maki_providers::plugin::keep_legacy_selections();

    let discovery = maki_lua::discover_installed(no_plugins);
    // Includes the names discovery refused, so a package it could not read does
    // not become a config error pointing at the user's `plugins.<name>` table.
    let discovered_names = discovery.known_names();
    let mut warnings: Vec<String> = discovery
        .problems
        .into_iter()
        .map(|problem| format!("skipping package: {problem}"))
        .collect();
    warnings.extend(legacy_warnings);

    let config = build_config(
        &host,
        &|host: &PluginHost| {
            let mut names = discovered_names.clone();
            names.extend(declared_packages(host)?.into_iter().map(|d| d.spec.name));
            names.sort();
            names.dedup();
            Ok(names)
        },
        &mut warnings,
    )?;

    // Before any plugin can call `maki.net`, so the first request already sees
    // the hosts the user exempted from the private-address block.
    maki_lua::set_allowed_private_hosts(&config.net.allowed_private_hosts);
    maki_lua::set_shell_preference(config.agent.shell.clone());

    if let Err(e) = host.load_builtins(&config.plugins) {
        let e = color_eyre::eyre::Report::from(e).wrap_err("load builtin plugins");
        match on_builtin_failure {
            BuiltinFailure::Fatal => return Err(e),
            BuiltinFailure::Warn => warnings.push(format!("{e:#}")),
        }
    }

    // Installing here rather than inside `maki.pack.add` keeps a clone off the
    // Lua thread, and is the phase Neovim's own `load` default defers to.
    let declared = declared_packages(&host)?;
    let installed = maki_lua::install_declared(&declared, interaction);
    warnings.extend(installed.failures);
    let available: Vec<DiscoveredPackage> = discovery
        .packages
        .into_iter()
        .chain(installed.packages)
        .collect();
    for warning in host.load_declared_packages(&available, &declared, &config.plugins) {
        // A UI the install pass held back is gated again where it loads, and
        // reported there in the same words.
        if !warnings.contains(&warning) {
            warnings.push(warning);
        }
    }

    // Last, so it covers every load above. Taking empties it, so a later
    // `/reload` only reports what that load found.

    // Publishes this load's providers in one step. Until here every reader
    // still sees the generation that was serving, so a `/reload` never opens a
    // window in which a registered provider answers "unknown".
    maki_providers::plugin::commit_load();

    Ok((host, config, sanitize_warnings(&warnings)))
}

fn declared_packages(host: &PluginHost) -> Result<Vec<maki_lua::Declared>> {
    host.declared_packages().context("read declared packages")
}

/// Everything a non-session subcommand needs before it can do work: the model
/// registry, project trust, `.env`, the plugin host, the effective config, and
/// the TUI's logging and telemetry, so a plugin's `maki.log.*` has somewhere to
/// go. One function, so the next
/// subcommand cannot forget a step the way all three of these did.
///
/// Plugins still load before the subscriber exists, because the config that
/// configures it is Lua-produced. That is unchanged from `cmd::tui::run`.
pub fn cli_stack(
    no_plugins: bool,
    no_jit: bool,
    trust_mode: TrustMode,
) -> Result<(PluginHost, Config)> {
    // First, as in `cmd::tui::run`, so anything that resolves a model sees the
    // models the TUI would.
    let storage = StateDir::resolve().context("resolve data directory")?;
    maki_providers::model_registry::load_from_storage(&storage);
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    // The `trust.paths` policy deliberately stops at the session entry points
    // (`cmd::tui`, `maki-acp`): a one-shot utility would record a grant the
    // user never saw, for a session it never runs.
    let trust =
        project::resolve_noninteractive(&cwd, trust_mode, maki_config::project::Runtime::Agent);
    load_env_files(&trust.project_config);

    let (host, config, warnings) = load_plugins(
        no_jit,
        no_plugins,
        BuiltinFailure::Fatal,
        Interaction::None,
        |host, names, warnings| {
            // `notices`, not `warning`: these commands never ask, so the
            // skipped path and how to undo it are the only sign the project
            // config did nothing.
            warnings.extend(trust.notices());
            let raw = host
                .load_init_files(
                    InitFiles::resolve(&trust.project_config, no_plugins),
                    warnings,
                )
                .context("load init.lua files")?;
            let mut config = raw
                .unwrap_or_default()
                .into_config(&names(host)?)
                .context("invalid config")?;
            config.permissions = load_permissions(&trust.project_config);
            Ok(config)
        },
    )?;
    logging::init_logging(
        config.storage.max_log_bytes,
        config.storage.max_log_files,
        trust.project_config.config_root(),
    );
    setup::init_telemetry(&config.telemetry);
    logging::install_panic_log_hook();
    report_warnings(warnings);
    Ok((host, config))
}

/// `--trust` is a grant for this process, so every entry point under it
/// reads the same shared project config the TUI would.
pub fn trust_mode(flags: &HostFlags) -> TrustMode {
    if flags.trust {
        TrustMode::Session
    } else {
        TrustMode::Consult
    }
}
