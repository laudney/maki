//! What a runtime that never installed the provider hooks lets a plugin do.
//!
//! The hooks are process-wide and only a started plugin host installs them.
//! Nothing in this file starts one, so every test here sees the defaults
//! under `cargo nextest` and plain `cargo test` alike.

use std::process::Command;

use maki_lua::test_support::{owner_allows, owner_vouches, sanitize_child_env};
use maki_providers::test_support::register_bundled;
use serde_json::json;
use url::Url;

const PROVIDER_SLUG: &str = "lan-box";
const PROVIDER_HOST: &str = "127.0.0.1";
const PROVIDER_PORT: u16 = 11434;
const PROVIDER_AUTHORITY: &str = "127.0.0.1:11434";
const PROVIDER_URL: &str = "http://127.0.0.1:11434/v1";
const PROVIDER_KEY_VAR: &str = "ANTHROPIC_API_KEY";
const KEY: &str = "sk-test";

/// The slug resolves to an origin maki chose, so with the hooks installed
/// the owner would reach it.
#[test]
fn an_owned_provider_origin_is_neither_vouched_for_nor_served() {
    register_bundled(
        json!({
            "slug": PROVIDER_SLUG,
            "display_name": PROVIDER_SLUG,
            "codec": "openai",
            "base_url": PROVIDER_URL,
            "models": [{ "prefixes": [PROVIDER_SLUG], "tier": "strong" }],
        }),
        PROVIDER_AUTHORITY,
    );

    assert!(!owner_vouches(
        PROVIDER_SLUG,
        &Url::parse(PROVIDER_URL).unwrap()
    ));
    assert!(!owner_allows(PROVIDER_SLUG, PROVIDER_HOST, PROVIDER_PORT));
}

#[test]
fn a_child_drops_provider_keys_without_provider_hooks() {
    let mut command = Command::new(PROVIDER_KEY_VAR);
    command.env(PROVIDER_KEY_VAR, KEY);
    sanitize_child_env(&mut command);
    let envs: Vec<_> = command.get_envs().collect();
    assert!(envs.contains(&(PROVIDER_KEY_VAR.as_ref(), None)));
}
