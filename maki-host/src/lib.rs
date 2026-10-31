pub mod acp;
pub mod auth;
mod cli;
pub mod interrupt;
pub mod project_trust;
pub mod provider_scripts;
pub mod setup;
pub mod start;
pub mod update;

use color_eyre::Result;
use color_eyre::eyre::Context;

use maki_storage::StateDir;

pub use cli::{AcpArgs, HostFlags};
pub use maki_otel::{TELEMETRY_SHUTDOWN_TIMEOUT, exit};

/// Serves ACP with plugins in the agent composition, or with `--login` runs
/// the interactive login and exits.
pub fn acp(args: AcpArgs, flags: &HostFlags) -> Result<()> {
    let trust_mode = start::trust_mode(flags);
    if args.login {
        let storage = StateDir::resolve().context("resolve data directory")?;
        // As `auth login`: a plugin's providers reach the registry only
        // once plugin load published them.
        let _host = start::cli_stack(flags.no_plugins, flags.no_jit, trust_mode)?;
        return auth::auth_login(None, &storage);
    }
    acp::run(args, flags, trust_mode)
}
