//! The Maki extensions to ACP share one `maki` object in `initialize`
//! metadata. The client asks for all of them, the agent answers with the
//! ones it turned on, and each is agreed on its own at its exact version: a
//! later version defines its own compatibility rule.

use agent_client_protocol_schema::v1::Meta;
use serde_json::Value;

use crate::btw::{BTW_KEY, BTW_VERSION};
use crate::elicitation::{QUESTION_KEY, QUESTION_VERSION};
use crate::login::{LOGIN_KEY, LOGIN_VERSION};
use crate::plan::{PLAN_KEY, PLAN_VERSION};
use crate::plugins;
use crate::session::{KEY as SESSION_KEY, VERSION as SESSION_VERSION};
use crate::settings::{SETTINGS_KEY, SETTINGS_VERSION};
use crate::tool_data::{TOOL_DATA_KEY, TOOL_DATA_VERSION};
use crate::usage::{LEDGER_KEY, LEDGER_VERSION, USAGE_KEY, USAGE_VERSION};

pub const META_KEY: &str = "maki";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Agreed {
    pub sessions: bool,
    pub usage: bool,
    pub settings: bool,
    pub btw: bool,
    /// The usage ledger extension, see [`crate::usage`]. It rides on the
    /// usage extension's metadata, so it is never agreed without it.
    pub usage_ledger: bool,
    pub login: bool,
    pub question: bool,
    pub plan: bool,
    pub tool_data: bool,
    pub plugins: bool,
}

const ALL: Agreed = Agreed {
    sessions: true,
    usage: true,
    settings: true,
    btw: true,
    usage_ledger: true,
    login: true,
    question: true,
    plan: true,
    tool_data: true,
    plugins: true,
};

impl Agreed {
    fn keys(&self) -> impl Iterator<Item = (&'static str, u64)> {
        [
            self.sessions.then_some((SESSION_KEY, SESSION_VERSION)),
            self.usage.then_some((USAGE_KEY, USAGE_VERSION)),
            self.settings.then_some((SETTINGS_KEY, SETTINGS_VERSION)),
            self.btw.then_some((BTW_KEY, BTW_VERSION)),
            self.usage_ledger.then_some((LEDGER_KEY, LEDGER_VERSION)),
            self.login.then_some((LOGIN_KEY, LOGIN_VERSION)),
            self.question.then_some((QUESTION_KEY, QUESTION_VERSION)),
            self.plan.then_some((PLAN_KEY, PLAN_VERSION)),
            self.plugins.then_some((plugins::KEY, plugins::VERSION)),
            self.tool_data.then_some((TOOL_DATA_KEY, TOOL_DATA_VERSION)),
        ]
        .into_iter()
        .flatten()
    }
}

pub fn request_meta() -> Meta {
    meta(&ALL)
}

/// What the agent answers, `None` when it turned nothing on.
pub fn response_meta(agreed: &Agreed) -> Option<Meta> {
    (*agreed != Agreed::default()).then(|| meta(agreed))
}

pub fn agreed(meta: Option<&Meta>) -> Agreed {
    let maki = meta.and_then(|meta| meta.get(META_KEY));
    let at = |key: &str, version: u64| {
        maki.and_then(|maki| maki.get(key)).and_then(Value::as_u64) == Some(version)
    };
    let usage = at(USAGE_KEY, USAGE_VERSION);
    let tool_data = at(TOOL_DATA_KEY, TOOL_DATA_VERSION);
    Agreed {
        sessions: at(SESSION_KEY, SESSION_VERSION),
        usage,
        settings: at(SETTINGS_KEY, SETTINGS_VERSION),
        btw: at(BTW_KEY, BTW_VERSION),
        usage_ledger: usage && at(LEDGER_KEY, LEDGER_VERSION),
        login: at(LOGIN_KEY, LOGIN_VERSION),
        question: at(QUESTION_KEY, QUESTION_VERSION),
        plan: at(PLAN_KEY, PLAN_VERSION),
        plugins: at(plugins::KEY, plugins::VERSION),
        tool_data,
    }
}

fn meta(agreed: &Agreed) -> Meta {
    let maki = agreed
        .keys()
        .map(|(key, version)| (key.to_owned(), Value::from(version)))
        .collect();
    Meta::from_iter([(META_KEY.to_owned(), Value::Object(maki))])
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    fn offered(maki: Value) -> Meta {
        Meta::from_iter([(META_KEY.to_owned(), maki)])
    }

    #[test_case(json!({ USAGE_KEY: USAGE_VERSION }), Agreed { usage: true, ..Agreed::default() } ; "usage_alone")]
    #[test_case(json!({ SETTINGS_KEY: SETTINGS_VERSION }), Agreed { settings: true, ..Agreed::default() } ; "settings_alone")]
    #[test_case(json!({ BTW_KEY: BTW_VERSION }), Agreed { btw: true, ..Agreed::default() } ; "btw_alone")]
    #[test_case(json!({ USAGE_KEY: USAGE_VERSION, LEDGER_KEY: LEDGER_VERSION }), Agreed { usage: true, usage_ledger: true, ..Agreed::default() } ; "ledger_with_usage")]
    #[test_case(json!({ LEDGER_KEY: LEDGER_VERSION }), Agreed::default() ; "ledger_without_its_carrier")]
    #[test_case(json!({ USAGE_KEY: USAGE_VERSION + 1, LEDGER_KEY: LEDGER_VERSION }), Agreed::default() ; "ledger_beside_another_usage_version")]
    #[test_case(json!({ USAGE_KEY: 1, LEDGER_KEY: 1 }), Agreed::default() ; "obsolete_usage_uses_standard_fields")]
    #[test_case(json!({ LOGIN_KEY: LOGIN_VERSION }), Agreed { login: true, ..Agreed::default() } ; "login_alone")]
    #[test_case(json!({ QUESTION_KEY: QUESTION_VERSION }), Agreed { question: true, ..Agreed::default() } ; "question_alone")]
    #[test_case(json!({ PLAN_KEY: PLAN_VERSION }), Agreed { plan: true, ..Agreed::default() } ; "plan_alone")]
    #[test_case(json!({ TOOL_DATA_KEY: TOOL_DATA_VERSION }), Agreed { tool_data: true, ..Agreed::default() } ; "tool_data_alone")]
    #[test_case(json!({ TOOL_DATA_KEY: TOOL_DATA_VERSION + 1 }), Agreed::default() ; "unsupported_tool_data_version")]
    #[test_case(json!({ SESSION_KEY: SESSION_VERSION }), Agreed { sessions: true, ..Agreed::default() } ; "session_operations")]
    #[test_case(json!({}), Agreed::default() ; "none")]
    fn each_extension_is_agreed_alone(maki: Value, expected: Agreed) {
        assert_eq!(agreed(Some(&offered(maki))), expected);
    }

    #[test]
    fn the_answer_names_only_what_was_agreed() {
        let usage_only = Agreed {
            usage: true,
            ..Agreed::default()
        };
        assert_eq!(agreed(response_meta(&usage_only).as_ref()), usage_only);
        assert!(response_meta(&Agreed::default()).is_none());
    }
}
