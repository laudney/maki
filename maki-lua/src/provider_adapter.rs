//! The agent's answers to [`crate::provider_hooks`], read from the provider
//! registry `maki.provider` fills.

use std::process::Command;

use maki_providers::{Timeouts, plugin, strip_provider_keys, user_agent};
use url::Url;

use crate::provider_hooks::{
    ChildEnv, ProviderOrigins, ProviderRoute, install_child_env, install_provider_origins,
};

struct ProvidersRegistry;

impl ProviderOrigins for ProvidersRegistry {
    fn vouched(&self, slug: &str) -> Option<Url> {
        plugin::vouched_origin(slug)
    }

    fn authority(&self, slug: &str) -> Option<String> {
        plugin::effective_authority(slug)
    }

    fn route(&self) -> ProviderRoute {
        let timeouts = Timeouts::default();
        ProviderRoute {
            user_agent: user_agent(),
            connect: timeouts.connect,
            low_speed: timeouts.low_speed,
        }
    }
}

struct StripProviderKeys;

impl ChildEnv for StripProviderKeys {
    fn sanitize(&self, command: &mut Command) {
        strip_provider_keys(command);
    }
}

pub(crate) fn install() {
    install_provider_origins(ProvidersRegistry);
    install_child_env(StripProviderKeys);
}

#[cfg(test)]
mod tests {
    use maki_providers::test_support::register_bundled;
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::plugin_permissions::{Missing, Permission, PluginPermissions, PluginReach};

    const PROVIDER_KEY_VAR: &str = "ANTHROPIC_API_KEY";
    const KEY: &str = "sk-test";
    const LOCAL_PROVIDER_SLUG: &str = "lan-box";
    const LOCAL_PROVIDER_HOST: &str = "127.0.0.1:11434";
    const LOCAL_PROVIDER_URL: &str = "http://127.0.0.1:11434/v1";

    /// Everything, with `run` only when {runs}: a caller that runs processes
    /// skips the egress comparison.
    fn reaching(hosts: Option<&[&str]>, runs: bool) -> PluginReach {
        let mut p = PluginPermissions::trusted();
        p.set(Permission::Run, runs);
        p.set_net_hosts(hosts.map(|hosts| hosts.iter().map(|h| (*h).to_owned()).collect()));
        PluginReach::new(p)
    }

    /// A bundled provider's address skips the private-address guard, so even
    /// a plugin allowed every public host falls short of it. The owner checking
    /// itself also proves the provider list is not locked twice.
    #[test_case(|_| reaching(None, false), Some(Missing::ProviderOrigin) ; "another_plugin_falls_short")]
    #[test_case(|_| reaching(None, true), None ; "a_caller_that_runs_processes_skips_origins")]
    #[test_case(PluginReach::clone, None ; "the_owner_covers_itself")]
    fn first_missing_compares_provider_origins(
        caller: fn(&PluginReach) -> PluginReach,
        expected: Option<Missing>,
    ) {
        register_bundled(
            json!({
                "slug": LOCAL_PROVIDER_SLUG,
                "display_name": LOCAL_PROVIDER_SLUG,
                "codec": "openai",
                "base_url": LOCAL_PROVIDER_URL,
                "models": [{ "prefixes": [LOCAL_PROVIDER_SLUG], "tier": "strong" }],
            }),
            LOCAL_PROVIDER_HOST,
        );
        install();
        let owner = reaching(Some(&[LOCAL_PROVIDER_HOST]), false);
        owner.egress().owns(LOCAL_PROVIDER_SLUG.to_owned());

        assert_eq!(caller(&owner).first_missing(&owner), expected);
    }

    #[test]
    fn a_child_loses_the_provider_keys() {
        let mut command = Command::new(PROVIDER_KEY_VAR);
        command.env(PROVIDER_KEY_VAR, KEY);
        StripProviderKeys.sanitize(&mut command);
        let key = command
            .get_envs()
            .find(|(var, _)| *var == PROVIDER_KEY_VAR)
            .and_then(|(_, value)| value);
        assert_eq!(key, None);
    }
}
