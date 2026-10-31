//! The agent builds model information directly from its model registry.

use maki_providers::Model;
use mlua::{Lua, Result as LuaResult, Table};

pub(crate) fn model_info_table(lua: &Lua, model: &Model) -> LuaResult<Table> {
    let tbl = lua.create_table()?;
    tbl.set("spec", model.spec())?;
    tbl.set("id", model.id.clone())?;
    tbl.set("provider", model.provider.to_string())?;
    tbl.set("provider_display", model.provider_display_name())?;
    tbl.set("tier", model.tier.to_string())?;
    tbl.set("context_window", model.context_window)?;
    if let Some(max) = model.max_output_tokens {
        tbl.set("max_output_tokens", max)?;
    }
    // `None` writes nil, which is the third state: a plugin reads false as
    // "metered" and nil as "we never learned a price".
    tbl.set("free", model.free())?;
    // Top level, not under `pricing`: the subsidy is a property of the route
    // to the provider, and `pricing` is absent on exactly the subsidised
    // models whose rates never resolved, which is the case it exists for.
    tbl.set("subsidised_by", model.subsidised_by.as_deref())?;
    if !model.pricing.is_zero() {
        let pricing = lua.create_table()?;
        pricing.set("input", model.pricing.input)?;
        pricing.set("output", model.pricing.output)?;
        pricing.set("cache_write", model.pricing.cache_write)?;
        pricing.set("cache_read", model.pricing.cache_read)?;
        if let Some(fast) = &model.pricing.fast {
            let f = lua.create_table()?;
            f.set("input", fast.input)?;
            f.set("output", fast.output)?;
            pricing.set("fast", f)?;
        }
        tbl.set("pricing", pricing)?;
    }
    Ok(tbl)
}

#[cfg(test)]
mod tests {
    use super::model_info_table;
    use crate::api::test_support::{eval as lua_eval, lua as lua_with_model, serving_ui};
    use crate::api::util::convert::lua_to_json;
    use maki_lua_core::api::model::SET_ARG_ERR;
    use maki_lua_core::facade::{ModelRequest, NO_UI_ERR, UI_DROPPED_ERR, UiReply};
    use maki_lua_ui::UiAction;
    use maki_providers::{Model, ModelTier, model_registry};
    use maki_storage::StateDir;
    use mlua::{Lua, Value};
    use serde_json::{Value as Json, json};
    use std::sync::Arc;
    use tempfile::TempDir;
    use test_case::test_case;

    const SPEC: &str = "anthropic/claude-opus-4-6";
    const THINKING: &str = "high";
    const SUBSIDY: &str = "Max";
    const UI_FAILURE: &str = "Model is not allowed by policy: anthropic/claude-opus-4-6";

    /// Assignments live in a process-global registry, so a test that needs one
    /// writes it and hands back the pair used to clear it afterwards. `unset`
    /// only removes a matching spec, hence the spec is returned with the dir.
    fn assign_tier(spec: &str, tier: ModelTier) -> (Lua, ModelTier, String) {
        let dir = TempDir::new().unwrap();
        let storage = StateDir::from_path(dir.path().to_path_buf());
        model_registry::set_and_persist(spec.into(), tier, &storage);
        (lua_with_model(None), tier, spec.into())
    }

    fn clear_tier(tier: ModelTier, spec: String) {
        let dir = TempDir::new().unwrap();
        let storage = StateDir::from_path(dir.path().to_path_buf());
        model_registry::unset_and_persist(&spec, tier, &storage);
    }

    /// The receiver is dropped up front, so the request cannot even leave.
    fn closed_ui() -> Lua {
        let (tx, rx) = flume::unbounded::<UiAction>();
        drop(rx);
        lua_with_model(Some(tx))
    }

    fn stub_ui(answer: fn(ModelRequest) -> Option<UiReply>) -> Lua {
        serving_ui(move |request| {
            let UiAction::Model { req, reply_tx } = request else {
                panic!("expected model request")
            };
            if let Some(reply) = answer(req) {
                reply_tx.send(reply).unwrap();
            }
        })
    }

    /// Echoes the request back, so a test can assert on what the UI would have
    /// acted on. Fields left out stay `nil` on the way back.
    fn echo(req: ModelRequest) -> Option<UiReply> {
        Some(Ok(match req {
            ModelRequest::Get => json!({ "spec": SPEC, "thinking": THINKING, "fast": true }),
            ModelRequest::Available => json!([SPEC]),
            ModelRequest::Set {
                spec,
                thinking,
                fast,
            } => json!({ "spec": spec, "thinking": thinking, "fast": fast }),
        }))
    }

    /// The value comes back as JSON so assertions outlive the Lua state.
    fn eval(lua: &Lua, script: &str) -> (Json, Option<String>) {
        let (val, err): (Value, Option<String>) = lua_eval(lua, script);
        (lua_to_json(lua, &val).unwrap(), err)
    }

    /// `set` forwards only the fields it was given, so whatever you leave out
    /// the UI leaves alone. `false` and `""` are values though, not omissions:
    /// `""` is the thinking toggle. The last case is the documented loop, `get`
    /// straight back into `set`, read-only extras and all.
    #[test_case("return model.get()", json!({ "spec": SPEC, "thinking": THINKING, "fast": true }) ; "get")]
    #[test_case("return model.available()", json!([SPEC]) ; "available")]
    #[test_case("return model.set('anthropic/claude-opus-4-6')", json!({ "spec": SPEC }) ; "set_bare_spec_string")]
    #[test_case("return model.set({ thinking = 8192, fast = true })", json!({ "thinking": "8192", "fast": true }) ; "set_table_without_spec")]
    #[test_case("return model.set({ thinking = '', fast = false })", json!({ "thinking": "", "fast": false }) ; "set_empty_thinking_and_false_fast")]
    #[test_case("local m = model.get() return model.set(m)", json!({ "spec": SPEC, "thinking": THINKING, "fast": true }) ; "set_fed_by_get")]
    fn requests_cross_the_channel_and_answer_with_the_new_state(script: &str, expected: Json) {
        assert_eq!(eval(&stub_ui(echo), script), (expected, None));
    }

    /// Every way of not getting an answer lands in the error slot, instead of
    /// throwing or parking forever.
    #[test_case(lua_with_model(None), NO_UI_ERR ; "no_ui_attached")]
    #[test_case(closed_ui(), NO_UI_ERR ; "event_loop_closed")]
    #[test_case(stub_ui(|_| None), UI_DROPPED_ERR ; "reply_channel_dropped")]
    #[test_case(stub_ui(|_| Some(Err(UI_FAILURE.to_owned()))), UI_FAILURE ; "ui_refused")]
    fn unanswered_request_returns_an_error_pair(lua: Lua, expected: &str) {
        assert_eq!(
            eval(&lua, "return model.get()"),
            (Json::Null, Some(expected.to_owned()))
        );
    }

    /// `info` resolves locally: no UI required, and a builtin model answers
    /// with its identity and price table.
    #[test]
    fn info_resolves_a_builtin_model_without_a_ui() {
        let lua = lua_with_model(None);
        let (val, err) = eval(&lua, "return model.info('anthropic/claude-sonnet-4-5')");
        assert_eq!(err, None);
        assert_eq!(val["spec"], json!("anthropic/claude-sonnet-4-5"));
        assert_eq!(val["provider"], json!("anthropic"));
        assert!(val["context_window"].as_u64().unwrap() > 0);
        assert!(val["pricing"]["input"].as_f64().unwrap() > 0.0);
        assert!(val["pricing"]["output"].as_f64().unwrap() > 0.0);
        assert_eq!(val["subsidised_by"], Json::Null);
        assert_eq!(val["free"], json!(false));
    }

    /// The three states have to read apart: a known `$0`, a metered model, and
    /// one nothing ever quoted a price for.
    #[test_case("zai/glm-4.7-flash",       json!(true)  ; "builtin_zero_priced_is_free")]
    #[test_case("anthropic/claude-sonnet-4-5", json!(false) ; "metered_is_not_free")]
    #[test_case("ollama/my-custom-model", Json::Null   ; "no_price_table_is_unknown")]
    fn info_free_separates_zero_from_unknown(spec: &str, expected: Json) {
        let lua = lua_with_model(None);
        let (val, err) = eval(&lua, &format!("return model.info('{spec}')"));
        assert_eq!(err, None);
        assert_eq!(val["free"], expected);
    }

    /// `tier` answers the assigned model in the same shape `info` does, so a
    /// plugin can hand the spec straight to `set`.
    #[test]
    fn tier_returns_the_assigned_model() {
        let (lua, tier, spec) = assign_tier(SPEC, ModelTier::Strong);
        let (val, err) = eval(&lua, "return model.tier('strong')");
        clear_tier(tier, spec);
        assert_eq!(err, None);
        assert_eq!(val["spec"], json!(SPEC));
        assert_eq!(val["tier"], json!("strong"));
        assert_eq!(val["assigned"], json!(true));
    }

    /// An untouched slot still answers the curated default a subagent would
    /// get, flagged so a plugin can tell it from a real pick.
    #[test]
    fn tier_without_an_assignment_falls_back_to_the_default() {
        let (val, err) = eval(
            &lua_with_model(None),
            "return model.tier('weak', 'anthropic')",
        );
        assert_eq!(err, None);
        assert_eq!(val["provider"], json!("anthropic"));
        assert_eq!(val["tier"], json!("weak"));
        assert_eq!(val["assigned"], json!(false));
    }

    #[test]
    fn tier_with_nothing_to_resolve_answers_nil() {
        let (val, err) = eval(&lua_with_model(None), "return model.tier('compaction')");
        assert_eq!((val, err), (Json::Null, None));
    }

    /// A name maki does not know reports through the error slot, the same way
    /// `set` reports a spec it cannot parse.
    #[test]
    fn tier_with_an_unknown_name_returns_an_error_pair() {
        let (val, err) = eval(&lua_with_model(None), "return model.tier('turbo')");
        assert_eq!(val, Json::Null);
        assert!(err.is_some());
    }

    /// An assignment left over from a provider that is gone is an error, not a
    /// silent `nil`: the user did pick something, it just no longer resolves.
    #[test]
    fn tier_with_a_stale_assignment_returns_an_error_pair() {
        let (lua, tier, spec) = assign_tier("no-such-provider/nope", ModelTier::Weak);
        let (val, err) = eval(&lua, "return model.tier('weak')");
        clear_tier(tier, spec);
        assert_eq!(val, Json::Null);
        assert!(err.is_some());
    }

    /// An unresolvable spec answers `(nil, err)` instead of throwing.
    #[test]
    fn info_unknown_provider_returns_an_error_pair() {
        let lua = lua_with_model(None);
        let (val, err) = eval(&lua, "return model.info('no-such-provider/nope')");
        assert_eq!(val, Json::Null);
        assert!(err.is_some());
    }

    /// A subsidy is visible even when its provider never quoted rates.
    #[test_case("anthropic/claude-sonnet-4-5", true ; "known_rates")]
    #[test_case("ollama/my-custom-model", false ; "unknown_rates")]
    fn info_table_reports_the_subsidy_independently_of_pricing(spec: &str, priced: bool) {
        let lua = Lua::new();
        let mut model = Model::from_spec(spec).unwrap();
        model.subsidised_by = Some(Arc::from(SUBSIDY));
        let tbl = model_info_table(&lua, &model).unwrap();
        let json = lua_to_json(&lua, &Value::Table(tbl)).unwrap();
        assert_eq!(json["pricing"].is_null(), !priced);
        assert_eq!(json["subsidised_by"], SUBSIDY);
    }

    /// A non-spec argument is a programmer error, so it throws instead of
    /// answering with a pair.
    #[test_case("return model.set(42)" ; "number")]
    #[test_case("return model.set()" ; "no_argument")]
    #[test_case("return model.set(nil)" ; "explicit_nil")]
    fn set_throws_on_a_non_spec_argument(script: &str) {
        let lua = lua_with_model(None);
        let err = crate::test_support::runtime()
            .block_on(lua.load(script).eval_async::<Value>())
            .unwrap_err();
        assert!(err.to_string().contains(SET_ARG_ERR));
    }
}
