//! What the core asks of the providers it never names: which origins a
//! provider's own plugin may reach unguarded, on what terms, and which
//! variables a child process must not inherit.
//!
//! The registry behind these answers is process-wide, so each hook is a
//! process-wide registration too. The first install wins and later ones are
//! ignored, which is safe because every host installs the same adapter. A
//! runtime with nothing installed vouches for and serves no provider origin
//! and removes provider keys from child environments. Only `maki.provider`,
//! an agent API, gives a plugin a slug to ask about, so a runtime without
//! the agent domain never asks.

use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use url::Url;

static ORIGINS: OnceLock<Box<dyn ProviderOrigins>> = OnceLock::new();
static CHILD_ENV: OnceLock<Box<dyn ChildEnv>> = OnceLock::new();

pub trait ProviderOrigins: Send + Sync {
    /// The origin of provider {slug} when the user or maki chose it, never
    /// one a third-party plugin authored.
    fn vouched(&self, slug: &str) -> Option<Url>;
    /// The `host:port` provider {slug} sends its requests to, with the port
    /// always written out.
    fn authority(&self, slug: &str) -> Option<String>;
    fn route(&self) -> ProviderRoute;
}

/// The terms a provider's own requests run under, so a plugin reaching its
/// provider's origin gets the same ones.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProviderRoute {
    pub user_agent: &'static str,
    pub connect: Duration,
    pub low_speed: Duration,
}

pub trait ChildEnv: Send + Sync {
    fn sanitize(&self, command: &mut Command);
}

pub fn install_provider_origins(origins: impl ProviderOrigins + 'static) {
    let _ = ORIGINS.set(Box::new(origins));
}

pub fn install_child_env(env: impl ChildEnv + 'static) {
    let _ = CHILD_ENV.set(Box::new(env));
}

pub(crate) fn vouched_origin(slug: &str) -> Option<Url> {
    ORIGINS.get()?.vouched(slug)
}

pub(crate) fn provider_authority(slug: &str) -> Option<String> {
    ORIGINS.get()?.authority(slug)
}

/// `None` with nothing installed, when no origin is vouched for either.
pub(crate) fn provider_route() -> Option<ProviderRoute> {
    ORIGINS.get().map(|origins| origins.route())
}

pub fn sanitize_child_env(command: &mut Command) -> &mut Command {
    if let Some(env) = CHILD_ENV.get() {
        env.sanitize(command);
    } else {
        maki_config::child_env::strip_provider_keys(command, Vec::new());
    }
    command
}
