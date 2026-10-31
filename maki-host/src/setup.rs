use std::collections::HashSet;
use std::fmt::Display;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail, eyre};

use maki_config::{ModelPolicy, ProviderConfig, SessionDefaults};
use maki_providers::model::{Model, ModelError, ModelTier};
use maki_providers::provider::provider_for_slug;
use maki_providers::spec::ProviderRegistry;
use maki_providers::{AgentError, Timeouts, custom, plugin};
use maki_storage::StateDir;
use maki_storage::model::{read_model, read_thinking};

use crate::provider_scripts;

const PROVIDER_PRIORITY: &[&str] = &[
    "anthropic",
    "openai",
    "xai",
    "copilot",
    "zai",
    "synthetic",
    "deepseek",
    "regolo",
];
const STARTUP_TIERS: [ModelTier; 2] = [ModelTier::Strong, ModelTier::Medium];

/// The model to start on, plus a warning when the saved one had to be skipped.
/// Logging is not up this early, so the caller has to show the warning.
pub fn resolve_model(
    explicit: Option<&str>,
    provider_config: &ProviderConfig,
    storage: &StateDir,
) -> Result<(Model, Option<String>)> {
    let policy = &provider_config.model_policy;
    if let Some(spec) = explicit {
        if !policy.allows(spec) {
            return Err(eyre!(
                "model {spec:?} is not allowed by provider model policy"
            ));
        }
        let model = from_spec_or_warm_catalog(spec).context("invalid --model spec")?;
        return Ok((model, None));
    }
    let Some(spec) = read_model(storage) else {
        return Ok((fallback_model(provider_config)?, None));
    };
    let skipped = match saved_model(&spec, policy) {
        Ok(model) => return Ok((model, None)),
        Err(reason) => format!("saved model {spec} unavailable ({reason:#})"),
    };
    match fallback_model(provider_config) {
        Ok(model) => {
            let warning = format!("{skipped}, using {}", model.spec());
            Ok((model, Some(warning)))
        }
        Err(error) => Err(error.wrap_err(skipped)),
    }
}

fn saved_model(spec: &str, policy: &ModelPolicy) -> Result<Model> {
    if !policy.allows(spec) {
        bail!("not allowed by provider model policy");
    }
    let model = from_spec_or_warm_catalog(spec)?;
    provider_ready(&model.provider)?;
    Ok(model)
}

fn fallback_model(provider_config: &ProviderConfig) -> Result<Model> {
    let policy = &provider_config.model_policy;
    if let Some(spec) = provider_config.default_model.as_deref() {
        if !policy.allows(spec) {
            return Err(eyre!(
                "default model {spec:?} is not allowed by provider model policy"
            ));
        }
        return from_spec_or_warm_catalog(spec).context("invalid default_model in config");
    }
    auto_detect_model(policy).ok_or_else(|| {
        let policy_note = if policy.is_restrictive() {
            "\nnote: an allowed_models/excluded_models policy is active and may exclude every candidate"
        } else {
            ""
        };
        eyre!(
            "no provider available - set an API key (e.g. ANTHROPIC_API_KEY), run `maki auth login`, or use -m to specify a model{policy_note}\n\nSee https://maki.sh/docs/providers/ for setup instructions"
        )
    })
}

/// A plugin provider that gets credentials from a hook resolves them on its
/// first request, so being registered is enough here: broken credentials fail
/// that request instead of quietly switching providers.
fn provider_ready(slug: &str) -> Result<(), AgentError> {
    provider_for_slug(slug, Timeouts::default()).map(drop)
}

/// `always_thinking` pins the level. Without it, a fresh session starts at
/// whatever `/thinking` last set, the way the model comes back too; a model
/// that cannot run that level clamps it when the session loads.
pub fn remember_thinking(defaults: &mut SessionDefaults, storage: &StateDir) {
    if defaults.thinking.is_none() {
        defaults.thinking = read_thinking(storage);
    }
}

/// An unknown slug may just mean the models.dev catalog has not been loaded
/// yet, so retry once with a warm catalog. `Model::from_spec` itself must stay
/// non-blocking: the UI draws with it.
fn from_spec_or_warm_catalog(spec: &str) -> Result<Model> {
    let result = match Model::from_spec(spec) {
        Err(ModelError::UnsupportedProvider(_)) => {
            maki_providers::warm_catalog();
            Model::from_spec(spec)
        }
        result => result,
    };
    if let Err(ModelError::UnsupportedProvider(slug)) = &result
        && let Some(hint) = provider_scripts::unknown_provider_hint(slug)
    {
        bail!(hint);
    }
    Ok(result?)
}

fn auto_detect_model(policy: &ModelPolicy) -> Option<Model> {
    for tier in STARTUP_TIERS {
        for &slug in PROVIDER_PRIORITY {
            if provider_ready(slug).is_ok()
                && let Ok(model) = Model::from_tier(slug, tier)
                && policy.allows(&model.spec())
            {
                return Some(model);
            }
        }
    }
    let mut checked = HashSet::new();
    user_provider_models()
        .filter(|model| policy.allows(&model.spec()))
        .find(|model| {
            checked.insert(Arc::clone(&model.provider)) && provider_ready(&model.provider).is_ok()
        })
}

/// Lua plugin providers, then `providers.toml` entries. They come after the
/// prioritised slugs so a key in the environment still wins. Plugins come
/// sorted by slug, so startup picks the same provider every run.
fn user_provider_models() -> impl Iterator<Item = Model> {
    let plugins = plugin::specs()
        .into_iter()
        .flat_map(|spec| STARTUP_TIERS.map(|tier| Model::from_tier_dynamic(spec.slug, tier)));
    let custom = custom::startup_specs(&STARTUP_TIERS)
        .into_iter()
        .map(|spec| Model::from_spec(&spec));
    plugins.chain(custom).filter_map(Result::ok)
}

/// Built-in slugs keep their compiled protocol, model catalog and auth wiring,
/// so a `providers.toml` entry setting those fields is only partly honored
/// (#597). A bundled plugin's slug counts once its declaration registered,
/// since it keeps the slug over a `protocol` the same way. Call this after
/// `init_logging` and the bundled load, or the warning has nowhere to go and
/// no declaration to check.
pub fn warn_ignored_provider_fields() {
    for (slug, def) in &maki_config::providers::ProvidersConfig::load().providers {
        if !ProviderRegistry::is_shipped(slug) || ProviderRegistry::get(slug).is_none() {
            continue;
        }
        let ignored = maki_config::providers::ignored_builtin_fields(slug, def);
        if ignored.is_empty() {
            continue;
        }
        tracing::warn!(
            slug,
            fields = %ignored.join(", "),
            "providers.toml entry for built-in provider ignores these fields \
             (base_url/plan/api_key still apply), use a custom slug to set \
             protocol or models"
        );
    }
}

/// Telemetry is opt-in and must never stop maki from starting, so a bad
/// setting is a warning in the log, not an error to the user. Call this after
/// `init_logging` or the warning has nowhere to go.
pub fn init_telemetry(config: &maki_config::TelemetryConfig) {
    if let Err(error) = maki_otel::init(config) {
        tracing::warn!(%error, "telemetry disabled");
    }
}

/// Headless runs without a session id still count, they just stay
/// unattributed.
pub fn report_session_start(start_type: &'static str, session_id: Option<impl Display>) {
    let id = session_id.map(|id| id.to_string());
    maki_otel::emit::session_started(start_type, id.as_deref());
}
