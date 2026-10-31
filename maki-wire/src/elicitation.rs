//! Maps the `question` tool onto ACP form elicitation (`elicitation/create`).
//! The tool keeps its Lua-advertised schema; only execution is rerouted here,
//! so Zed and friends render native forms instead of a spinner nobody can answer.
//!
//! The question extension carries what the standard form drops: the tool's
//! own questions in `_meta.maki.question.questions` of the request, and the
//! picks and typed answers in `_meta.maki.question.answers` of the response,
//! so a Maki client draws the local form and the model reads what was typed.

use std::collections::BTreeMap;

use agent_client_protocol_schema::v1::{
    ClientCapabilities, CreateElicitationRequest, CreateElicitationResponse,
    ElicitationAcceptAction, ElicitationAction, ElicitationContentValue, ElicitationFormMode,
    ElicitationPropertySchema, ElicitationSchema, ElicitationSessionScope, EnumOption, Meta,
    MultiSelectPropertySchema, SessionId, StringPropertySchema, ToolCallId,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::extensions::META_KEY;

pub const QUESTION_KEY: &str = "question";
pub const QUESTION_VERSION: u64 = 1;
const QUESTIONS_FIELD: &str = "questions";
const ANSWERS_FIELD: &str = "answers";

/// Mirrors the Lua question tool's dismiss output so the model sees the same
/// text regardless of frontend.
pub const DISMISSED: &str = "(question dismissed by user)";
const NO_ANSWER: &str = "(no answer)";

#[derive(Deserialize)]
struct Question {
    question: String,
    #[serde(default)]
    header: String,
    #[serde(default)]
    options: Vec<QuestionOption>,
    #[serde(default, rename = "multiSelect", alias = "multiple")]
    multi_select: bool,
}

#[derive(Deserialize)]
struct QuestionOption {
    label: String,
    #[serde(default)]
    description: String,
}

pub fn supports_form(caps: &ClientCapabilities) -> bool {
    caps.elicitation.as_ref().is_some_and(|e| e.form.is_some())
}

fn parse_questions(input: &Value) -> Result<Vec<Question>, String> {
    let questions = input.get("questions").cloned().unwrap_or(Value::Null);
    serde_json::from_value(questions).map_err(|e| format!("invalid questions: {e}"))
}

fn enum_options(options: &[QuestionOption]) -> Vec<EnumOption> {
    options
        .iter()
        .map(|opt| {
            let title = if opt.description.is_empty() {
                opt.label.clone()
            } else {
                format!("{} - {}", opt.label, opt.description)
            };
            EnumOption::new(opt.label.clone(), title)
        })
        .collect()
}

fn property(q: &Question) -> ElicitationPropertySchema {
    let title = q.question.clone();
    if q.options.is_empty() {
        ElicitationPropertySchema::String(StringPropertySchema::new().title(title))
    } else if q.multi_select {
        ElicitationPropertySchema::Array(
            MultiSelectPropertySchema::titled(enum_options(&q.options)).title(title),
        )
    } else {
        ElicitationPropertySchema::String(
            StringPropertySchema::new()
                .title(title)
                .one_of(enum_options(&q.options)),
        )
    }
}

/// Property keys are positional (`q1`, `q2`, ...) so answers map back to
/// questions even when headers repeat or are missing.
fn key(index: usize) -> String {
    format!("q{}", index + 1)
}

/// `extension` adds the tool's own questions for a client that agreed on
/// the question extension; the standard form stays for any other reader.
pub fn form_request(
    session_id: &str,
    tool_call_id: Option<String>,
    input: &Value,
    extension: bool,
) -> Result<CreateElicitationRequest, String> {
    let questions = parse_questions(input)?;
    if questions.is_empty() {
        return Err("at least one question is required".to_owned());
    }

    let mut schema = ElicitationSchema::new();
    schema.properties = questions
        .iter()
        .enumerate()
        .map(|(i, q)| (key(i), property(q)))
        .collect();

    let scope = ElicitationSessionScope::new(SessionId::from(session_id.to_owned()))
        .tool_call_id(tool_call_id.map(ToolCallId::from));
    let message = match questions.as_slice() {
        [only] => only.question.clone(),
        many => format!("{} questions", many.len()),
    };
    let mut request =
        CreateElicitationRequest::new(ElicitationFormMode::new(scope, schema), message);
    if extension {
        request.meta = Some(question_meta(
            QUESTIONS_FIELD,
            input[QUESTIONS_FIELD].clone(),
        ));
    }
    Ok(request)
}

/// The tool's questions of a request under the question extension.
pub fn extension_questions(request: &CreateElicitationRequest) -> Option<Value> {
    question_field(request.meta.as_ref(), QUESTIONS_FIELD).cloned()
}

/// The answer a client under the question extension sends: one array per
/// question, empty for a skipped one.
pub fn submitted(answers: Vec<Vec<String>>) -> CreateElicitationResponse {
    CreateElicitationResponse::new(ElicitationAcceptAction::new())
        .meta(question_meta(ANSWERS_FIELD, json!(answers)))
}

pub fn dismissed() -> CreateElicitationResponse {
    CreateElicitationResponse::new(ElicitationAction::Cancel)
}

fn question_meta(field: &str, value: Value) -> Meta {
    Meta::from_iter([(
        META_KEY.to_owned(),
        json!({ QUESTION_KEY: { field: value } }),
    )])
}

fn question_field<'a>(meta: Option<&'a Meta>, field: &str) -> Option<&'a Value> {
    meta?.get(META_KEY)?.get(QUESTION_KEY)?.get(field)
}

fn answer_text(value: Option<&ElicitationContentValue>) -> Option<String> {
    match value? {
        ElicitationContentValue::String(s) if !s.is_empty() => Some(s.clone()),
        ElicitationContentValue::StringArray(items) if !items.is_empty() => Some(items.join(", ")),
        ElicitationContentValue::Integer(n) => Some(n.to_string()),
        ElicitationContentValue::Number(n) => Some(n.to_string()),
        ElicitationContentValue::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Turns the client's `elicitation/create` result into the same markdown the
/// Lua tool feeds the model: one `header: labels` line per question. The
/// extension's answers win over the standard content, since only they can
/// hold a typed answer.
pub fn format_response(input: &Value, raw_result: &str) -> String {
    let Ok(response) = serde_json::from_str::<Value>(raw_result) else {
        return DISMISSED.to_owned();
    };
    if response["action"] != "accept" {
        return DISMISSED.to_owned();
    }
    let questions = parse_questions(input).unwrap_or_default();
    let meta = response
        .get("_meta")
        .and_then(|meta| serde_json::from_value::<Meta>(meta.clone()).ok());
    if let Some(answers) = question_field(meta.as_ref(), ANSWERS_FIELD)
        .and_then(|answers| serde_json::from_value::<Vec<Vec<String>>>(answers.clone()).ok())
    {
        return format_answers(&questions, |i| {
            answers
                .get(i)
                .filter(|picks| !picks.is_empty())
                .map(|picks| picks.join(", "))
        });
    }
    // Values parse per key: nothing in the schema is `required`, so clients
    // may null out skipped fields, and one unreadable value should cost one
    // answer, not the whole form.
    let content: BTreeMap<String, ElicitationContentValue> = response["content"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| Some((k.clone(), serde_json::from_value(v.clone()).ok()?)))
                .collect()
        })
        .unwrap_or_default();
    format_answers(&questions, |i| answer_text(content.get(&key(i))))
}

/// `QuestionHelpers.format_answers` of the Lua tool, line for line.
fn format_answers(questions: &[Question], answer: impl Fn(usize) -> Option<String>) -> String {
    questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let label = if q.header.is_empty() {
                format!("Q{}", i + 1)
            } else {
                q.header.clone()
            };
            let answer = answer(i).unwrap_or_else(|| NO_ANSWER.to_owned());
            format!("{label}: {answer}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::ElicitationScope;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const TYPED: &str = "my own answer";

    fn questions_input() -> Value {
        serde_json::json!({
            "questions": [
                {
                    "question": "Pick a framework",
                    "header": "Framework",
                    "options": [
                        { "label": "axum", "description": "tokio based" },
                        { "label": "actix" }
                    ]
                },
                {
                    "question": "Which features?",
                    "header": "Features",
                    "multiSelect": true,
                    "options": [{ "label": "auth" }, { "label": "uploads" }]
                },
                { "question": "Anything else?" }
            ]
        })
    }

    #[test_case(false ; "standard")]
    #[test_case(true ; "with_tool_questions")]
    fn form_request_maps_questions_to_schema(extension: bool) {
        let req = form_request(
            "sess_1",
            Some("tool_1".to_owned()),
            &questions_input(),
            extension,
        )
        .unwrap();
        assert_eq!(req.message, "3 questions");
        assert_eq!(
            extension_questions(&req),
            extension.then(|| questions_input()["questions"].clone())
        );

        let ElicitationScope::Session(scope) = req.scope() else {
            panic!("expected session scope");
        };
        assert_eq!(scope.session_id.0.as_ref(), "sess_1");
        assert_eq!(
            scope.tool_call_id.as_ref().map(|t| t.0.as_ref()),
            Some("tool_1")
        );

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["mode"], "form");
        let props = &json["requestedSchema"]["properties"];
        assert_eq!(props["q1"]["type"], "string");
        assert_eq!(props["q1"]["oneOf"][0]["const"], "axum");
        assert_eq!(props["q2"]["type"], "array");
        assert_eq!(props["q3"]["type"], "string");
        assert!(props["q3"].get("oneOf").is_none());
    }

    #[test]
    fn single_question_is_the_message() {
        let input = serde_json::json!({ "questions": [{ "question": "Proceed?" }] });
        let req = form_request("sess_1", None, &input, false).unwrap();
        assert_eq!(req.message, "Proceed?");
    }

    #[test_case(serde_json::json!({}) ; "missing_questions")]
    #[test_case(serde_json::json!({ "questions": [] }) ; "empty_questions")]
    fn form_request_rejects_bad_input(input: Value) {
        assert!(form_request("sess_1", None, &input, false).is_err());
    }

    #[test_case(json!({"action":"accept", "content":{"q1":"axum", "q2":["auth","uploads"]}}), "Framework: axum\nFeatures: auth, uploads\nQ3: (no answer)".into() ; "standard_answers")]
    #[test_case(json!({"action":"accept", "content":{"q1":"axum", "q2":null}}), "Framework: axum\nFeatures: (no answer)\nQ3: (no answer)".into() ; "one_null_answer")]
    #[test_case(serde_json::to_value(submitted(vec![vec![TYPED.into()], Vec::new(), vec!["later".into()]])).unwrap(), format!("Framework: {TYPED}\nFeatures: (no answer)\nQ3: later") ; "typed_and_skipped_answers")]
    #[test_case(json!({"action":"accept", "content":{"q1":"actix"}, "_meta":{META_KEY:{QUESTION_KEY:{ANSWERS_FIELD:[["axum",TYPED]]}}}}), format!("Framework: axum, {TYPED}\nFeatures: (no answer)\nQ3: (no answer)") ; "extension_overrides_content")]
    fn responses_keep_submitted_answers_and_allow_partial_forms(raw: Value, expected: String) {
        assert_eq!(
            format_response(&questions_input(), &raw.to_string()),
            expected
        );
    }

    #[test_case(r#"{"action":"decline"}"# ; "decline")]
    #[test_case(r#"{"action":"cancel"}"# ; "cancel")]
    #[test_case("not json" ; "unparsable")]
    #[test_case("null" ; "jsonrpc_error_forwarded_as_null")]
    fn non_accept_is_dismissed(raw: &str) {
        assert_eq!(format_response(&questions_input(), raw), DISMISSED);
    }

    #[test]
    fn supports_form_requires_form_capability() {
        assert!(!supports_form(&ClientCapabilities::default()));
        let caps: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "elicitation": { "form": {} }
        }))
        .unwrap();
        assert!(supports_form(&caps));
        let url_only: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "elicitation": { "url": {} }
        }))
        .unwrap();
        assert!(!supports_form(&url_only));
    }
}
