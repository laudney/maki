//! `maki.model`. The event loop owns the model slot and the per-session
//! request options, so most calls round-trip to it; `info` resolves
//! locally and works without a UI.

use maki_config::providers::ModelTier;
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Error as LuaError, Lua, Result as LuaResult, Table, Value};

use crate::api::util::convert::TableExt;
use crate::api::util::pair::{Pair, err_pair, try_pair};
use crate::facade::{FacadeOps, FacadeRequest, ModelRequest};
use crate::plugin_permissions::PluginPermissions;

pub const SET_ARG_ERR: &str = "expected a model spec string or an options table";

async fn roundtrip(lua: Lua, ops: FacadeOps, req: ModelRequest) -> LuaResult<Pair<Value>> {
    ops.ask_json(&lua, FacadeRequest::Model(req)).await
}

/// Resolve a model spec to everything maki knows about it: identity, tier,
/// context window, and the price table the session would be billed by --
/// including rates resolved from provider config or the bundled catalog
/// (e.g. subsidised custom providers), which the provider's own /v1/models
/// endpoint may never report. Purely local -- no UI round-trip, no network
/// -- so it also works from slash commands and headless embeddings.
///
/// @param spec string `"provider/id"`, as listed by `available()`.
/// @return (table|nil, string|nil) `{spec, id, provider, provider_display,
///   tier, subsidised_by?, context_window, max_output_tokens?, free?,
///   pricing?}`, or nil and an error.
///
///   `free` has three states: `true` when the model is known to cost nothing,
///   `false` when it is metered, and nil when no source ever quoted a rate.
///   Check `~= nil` before trusting it.
///
///   `subsidised_by` names the subscription prepaying this provider (billed
///   cost is $0, the rates are the list-price reference). It sits at the top
///   level because it holds whether or not rates resolved.
///
///   `pricing` is present only when rates are known:
///   `{input, output, cache_write, cache_read}` in USD per million tokens,
///   plus optional `fast = {input, output}`.
/// @example
/// local m, err = maki.model.info("anthropic/claude-opus-4-6")
/// if m and m.subsidised_by then print(m.subsidised_by, m.pricing.input) end
#[lua_fn]
fn info(lua: &Lua, #[ctx] ops: FacadeOps, spec: String) -> LuaResult<Pair<Table>> {
    match try_pair!(ops.local()).model(lua, &spec)? {
        Ok(table) => Ok((Some(table), None)),
        Err(e) => Ok(err_pair(e)),
    }
}

/// The model maki uses for a tier, the same one a subagent asking for that
/// tier gets: user's pick from the `/model` picker for that tier (whatever
/// provider it names), else the curated default within `provider`, else any
/// provider. `assigned` tells a pick apart from a default. Pass `provider` to
/// prefer its models, the way subagents prefer the session's provider.
///
/// @param name string `"weak"`, `"medium"`, `"strong"`, or `"compaction"`.
/// @param provider string|nil Provider slug to resolve within first.
/// @return (table|nil, string|nil) The model in the shape `info` returns, plus
///   `assigned` (boolean), true when you picked it for this tier. nil and nil
///   when no model fits the tier, nil and an error when the spec no longer
///   resolves.
/// @example
/// local m = maki.model.tier("weak", maki.model.get().provider)
/// if m then maki.model.set(m.spec) end
#[lua_fn]
fn tier(
    lua: &Lua,
    #[ctx] ops: FacadeOps,
    name: String,
    provider: Option<String>,
) -> LuaResult<Pair<Table>> {
    let tier: ModelTier = match name.parse() {
        Ok(tier) => tier,
        Err(e) => return Ok(err_pair(e)),
    };
    let local = try_pair!(ops.local());
    let spec = match local.resolve_tier(provider.as_deref(), tier) {
        Some(spec) => spec,
        None => return Ok((None, None)),
    };
    let tbl = match local.model(lua, &spec)? {
        Ok(table) => table,
        Err(e) => return Ok(err_pair(format!("tier {tier} points at {spec}: {e}"))),
    };
    tbl.set("assigned", local.tier_assigned(&spec, tier))?;
    Ok((Some(tbl), None))
}

/// Reads the focused session's model, thinking level, and fast mode.
/// `thinking` comes back in the spelling `set` accepts, so a table from here
/// can go straight back in.
///
/// `thinking_options` lists named thinking choices, cheapest first, as
/// `{name, tokens?, sends}` per row. Request paths that use named `thinking_fields`
/// expose those levels without token counts. Toggle-only fields expose
/// `adaptive` and, when allowed, `off`. Otherwise the generic budget ladder applies.
/// `tokens` is absent on `off` and `adaptive` too. `sends` is the row the
/// request goes out as: the row's own name, or the level a model that does
/// not take this one snaps it to. The list is empty exactly when
/// `supports_thinking` is false.
///
/// `thinking` keeps the session's intent and delegation ceiling.
/// `thinking_shown` names the effective row to highlight, including snapped
/// effort levels and budgets sent as native levels or toggles. With declared
/// fields, a budget stays numeric only where the adapter forwards the count.
/// On the generic ladder, a model whose API has no adaptive mode leaves
/// `adaptive` out and shows its fixed level.
/// Declared effort fields keep `adaptive` separate because their fragments can
/// differ from the provider fallback.
///
/// @return (table|nil, string|nil) `{spec, id, provider, thinking,
///   thinking_shown, thinking_options, fast, supports_thinking,
///   supports_fast}`, or nil and an error.
/// @example
/// local m = maki.model.get()
/// if m.spec ~= "anthropic/claude-opus-4-6" then ... end
/// for _, option in ipairs(m.thinking_options) do
///   print(option.name, option.tokens, option.sends)
/// end
#[lua_fn]
async fn get(lua: Lua, #[ctx] ops: FacadeOps) -> LuaResult<Pair<Value>> {
    roundtrip(lua, ops, ModelRequest::Get).await
}

/// Lists the model specs you can switch to: what the providers you are logged
/// into offer, minus what your model policy blocks. The list fills in the
/// background at startup, so right after launch it can still be empty.
///
/// @return (table|nil, string|nil) Array of `"provider/id"` specs, or nil and an error.
/// @example
/// local specs = maki.model.available()
#[lua_fn]
async fn available(lua: Lua, #[ctx] ops: FacadeOps) -> LuaResult<Pair<Value>> {
    roundtrip(lua, ops, ModelRequest::Available).await
}

/// Switches the focused session's model, thinking level, or fast mode. Fields
/// you leave out stay as they are, so this doubles as a thinking-only switch.
/// Answers with the new state, in the same shape `get` returns.
///
/// @param opts string|table A model spec, or a table with any of:
///   `spec` (string) `"provider/id"`, as listed by `available()`;
///   `thinking` (string|number) `"off"`, `"adaptive"`, an effort level
///   (`"minimal"` to `"max"`), a token budget, or `""` to toggle it on and off;
///   `fast` (boolean) Anthropic fast mode.
/// @return (table|nil, string|nil) The new state, or nil and an error.
/// @example
/// maki.model.set("anthropic/claude-opus-4-6")
/// maki.model.set({ spec = "zai/glm-5", thinking = "high" })
/// maki.keymap.set({ "n", "i" }, "<M-t>", function() maki.model.set({ thinking = "" }) end)
#[lua_fn(guard = SessionSettings)]
async fn set(lua: Lua, #[ctx] ops: FacadeOps, opts: Value) -> LuaResult<Pair<Value>> {
    let req = match opts {
        Value::String(spec) => ModelRequest::Set {
            spec: Some(spec.to_str()?.to_owned()),
            thinking: None,
            fast: None,
        },
        Value::Table(opts) => ModelRequest::Set {
            spec: opts.opt("spec")?,
            thinking: opts.opt("thinking")?,
            fast: opts.opt("fast")?,
        },
        other => {
            return Err(LuaError::runtime(format!(
                "{SET_ARG_ERR}, got {}",
                other.type_name()
            )));
        }
    };
    roundtrip(lua, ops, req).await
}

lua_table! {
    /// The model behind the focused session. Good for a keybind that flips
    /// between your two go-to models, or lifts thinking for one hard question.
    /// Without an interactive UI, `get`, `available` and `set` return
    /// `nil, "no interactive UI attached"`.
    ///
    /// Under `maki connect` the connected agent answers `get`, `available`
    /// and `set`. `set` changes thinking and fast mode and returns the state
    /// once the agent confirms it. A model spec returns an error there, so
    /// switch models with `/model`. `info` and `tier` return
    /// `nil, "no local agent attached"`.
    "maki.model" => pub fn create_model_table(
        ops: FacadeOps,
        perms: &PluginPermissions,
    ),
    DOCS [get(ops), available(ops), set(perms, ops), tier(ops), info(ops)]
}
