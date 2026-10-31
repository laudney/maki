use std::process::Command;

use crate::providers::catalog;
use crate::spec::ProviderRegistry;

pub fn strip_provider_keys(command: &mut Command) -> &mut Command {
    let vars = ProviderRegistry::all()
        .into_iter()
        .map(|spec| spec.api_key_env.to_owned())
        .chain(catalog::key_vars_if_available());
    maki_config::child_env::strip_provider_keys(command, vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_without_the_provider_runtime_strips_every_native_key() {
        for provider in ProviderRegistry::all_compiled() {
            if provider.api_key_env.is_empty() {
                continue;
            }
            let mut command = Command::new("unused");
            maki_config::child_env::strip_provider_keys(&mut command, Vec::new());
            assert!(
                command
                    .get_envs()
                    .any(|(name, value)| name == provider.api_key_env && value.is_none()),
                "{} could pass its key to a client shell",
                provider.slug
            );
        }
    }
}
