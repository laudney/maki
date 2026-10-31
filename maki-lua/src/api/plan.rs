//! Tests of `maki.plan` over a UI channel. The namespace is the core's.

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use crate::api::test_support::{eval, lua, replying_ui};
    use maki_lua_core::facade::NO_UI_ERR;
    use maki_lua_ui::UiAction;
    use mlua::Value;
    use serde_json::json;

    #[test]
    fn plan_read_without_ui_returns_error_pair() {
        let lua = lua(None);
        let (val, err): (Value, Option<String>) = eval(&lua, "return plan.read()");
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    /// Plan state is per session, so the call has to be able to name one.
    #[test_case(r#"plan.read({ session = "s1" })"#, Some("s1") ; "explicit_session")]
    #[test_case("plan.read()", None ; "focused_session")]
    fn the_session_option_travels_to_the_ui(call: &str, expected: Option<&str>) {
        let (lua, requests) = replying_ui(Ok(json!(true)));
        eval::<Value>(&lua, &format!("return {call}"));
        let UiAction::Plan { req, .. } = requests.try_recv().unwrap() else {
            panic!("expected plan request")
        };
        assert_eq!(req.session.as_deref(), expected);
    }
}
