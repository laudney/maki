mod migrate;
mod session;
mod subcmd;
mod tui;

use color_eyre::Result;
use color_eyre::eyre::Context;

use maki_host::start::{cli_stack, trust_mode};
use maki_host::{auth, project_trust};

use maki_storage::StateDir;

use crate::cli::{AuthAction, Cli, Command, McpAction, MigrateAction, SessionAction, TrustAction};
use crate::update;

pub fn dispatch(cli: Cli) -> Result<()> {
    let trust_mode = trust_mode(&cli.host);
    let background_timeout = cli.background_timeout;
    match cli.command {
        Some(Command::Auth { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            // Providers registered by a Lua plugin only reach the registry once
            // plugin load has published them, and `login`/`logout` drive a hook
            // that runs on the host's Lua thread, so `_host` stays bound for the
            // whole arm.
            let _host = cli_stack(cli.host.no_plugins, cli.host.no_jit, trust_mode)?;
            match action {
                AuthAction::Login { provider } => auth::auth_login(provider.as_deref(), &storage)?,
                AuthAction::Logout { provider } => auth::auth_logout(&provider, &storage)?,
                AuthAction::Status => auth::auth_status(&storage)?,
            }
        }
        Some(Command::Index { path }) => {
            subcmd::index(&path, cli.host.no_plugins, cli.host.no_jit, trust_mode)?;
        }
        Some(Command::Models { refresh }) => {
            subcmd::models(cli.host.no_plugins, cli.host.no_jit, refresh, trust_mode)?
        }
        Some(Command::Session { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            match action {
                SessionAction::List { global, json } => session::list(global, json, &storage)?,
                SessionAction::Delete { session_id, force } => {
                    session::delete(&session_id, force, &storage)?
                }
                SessionAction::Export { session_id } => session::export(&session_id, &storage)?,
                SessionAction::Import { file } => session::import(file.as_deref(), &storage)?,
            }
        }
        Some(Command::Mcp { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            match action {
                McpAction::Auth { server } => subcmd::mcp_auth(&server, &storage, trust_mode)?,
                McpAction::Logout { server } => subcmd::mcp_logout(&server, &storage)?,
            }
        }
        Some(Command::Update { yes, no_color }) => {
            update::update(yes, no_color).map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
        }
        Some(Command::Rollback) => {
            update::rollback().map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
        }
        Some(Command::Acp(mut args)) => {
            args.background_timeout = args.background_timeout.or(background_timeout);
            maki_host::acp(args, &cli.host)?;
        }
        Some(Command::Migrate { action }) => match action {
            MigrateAction::Xdg => migrate::xdg()?,
            MigrateAction::Providers => {
                // A script counts as ported once a plugin registers its slug.
                let _host = cli_stack(cli.host.no_plugins, cli.host.no_jit, trust_mode)?;
                migrate::providers()?
            }
        },
        Some(Command::Trust { action }) => {
            let storage = StateDir::resolve().context("resolve state directory")?;
            match action {
                TrustAction::Add { path, yes } => {
                    project_trust::add(&storage, path.as_deref(), yes)?
                }
                TrustAction::Remove { path } => project_trust::remove(&storage, path.as_deref())?,
                TrustAction::List => {
                    for decision in project_trust::list(&storage)? {
                        println!("{decision}");
                    }
                }
            }
        }
        Some(Command::Prompt {
            variant,
            plan,
            tools,
            names,
        }) => {
            subcmd::prompt(
                &variant,
                plan,
                tools,
                names,
                cli.host.no_plugins,
                cli.host.no_jit,
                trust_mode,
            )?;
        }
        None => {
            tui::run(cli)?;
        }
    }
    Ok(())
}
