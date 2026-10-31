//! Shared tool events in `_meta.maki.toolData`. Standard ACP content
//! remains the readable fallback when this extension is not agreed.

use agent_client_protocol_schema::v1::{Meta, SessionUpdate, ToolCallUpdate};
use maki_types::event::AgentEvent;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::extensions::META_KEY;
use crate::sanitize;

pub const TOOL_DATA_VERSION: u64 = 2;
pub const TOOL_DATA_KEY: &str = "toolData";
const NOTIFICATION_META_PATH: [&str; 3] = ["params", "update", "_meta"];

pub fn attach(update: Option<SessionUpdate>, id: &str, event: &AgentEvent) -> SessionUpdate {
    let mut update = match update {
        Some(SessionUpdate::ToolCallUpdate(update)) => update,
        Some(other) => return other,
        None => ToolCallUpdate::new(id.to_owned(), Default::default()),
    };
    let mut meta = update.meta.take().unwrap_or_default();
    if let Value::Object(maki) = meta
        .entry(META_KEY.to_owned())
        .or_insert_with(|| Value::Object(Map::new()))
    {
        maki.insert(
            TOOL_DATA_KEY.to_owned(),
            serde_json::to_value(event).unwrap_or_default(),
        );
    }
    SessionUpdate::ToolCallUpdate(update.meta(meta))
}

pub fn read(meta: Option<&Meta>, id: &str) -> Option<Result<AgentEvent, String>> {
    let value = meta?.get(META_KEY)?.get(TOOL_DATA_KEY)?;
    Some(
        AgentEvent::deserialize(value)
            .map_err(|error| format!("unreadable tool data: {error}"))
            .and_then(|event| {
                let call = match &event {
                    AgentEvent::ToolStart(event) => &event.id,
                    AgentEvent::ToolProgress(event) => &event.id,
                    AgentEvent::ToolDone(event) => &event.id,
                    AgentEvent::ToolMovable { id, .. } | AgentEvent::ToolParts { id, .. } => id,
                    _ => return Err("not a tool event".into()),
                };
                if call != id {
                    return Err("tool event names another call".into());
                }
                Ok(event)
            }),
    )
}

/// Sanitizes a received session update notification as
/// [`sanitize::sanitize_value`] does, all but its tool data, so a remote
/// renderer gets the input and state a local one does. Its text is
/// sanitized where it is drawn instead.
pub fn sanitize_keeping_raw(message: &mut Value) {
    let raw = maki_meta(message).and_then(|maki| maki.remove(TOOL_DATA_KEY));
    sanitize::sanitize_value(message);
    if let (Some(raw), Some(maki)) = (raw, maki_meta(message)) {
        maki.insert(TOOL_DATA_KEY.to_owned(), raw);
    }
}

fn maki_meta(message: &mut Value) -> Option<&mut Map<String, Value>> {
    NOTIFICATION_META_PATH
        .iter()
        .chain([&META_KEY])
        .try_fold(message, |value, key| value.get_mut(*key))?
        .as_object_mut()
}
#[cfg(test)]
mod tests {
    use super::*;
    use maki_types::event::ToolDoneEvent;
    use maki_types::tool::{ToolData, ToolOutput};
    use serde_json::json;
    use std::sync::Arc;
    use test_case::test_case;

    const CALL: &str = "call-1";
    const RAW: &str = "line one\r\nline\u{7} two\u{1b}[31m";
    const LARGE_BYTES: usize = 2 * 1024 * 1024;

    #[test]
    fn renderer_state_and_output_reach_the_client_whole_and_raw() {
        let text = RAW.repeat(LARGE_BYTES / RAW.len());
        let mut data = ToolData::new("bash".into(), 1, json!({"command": RAW}), RAW.into());
        data.state = json!({ "log": text });
        let event = AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            output: Arc::new(ToolOutput::Plain(text.clone().into())),
            data: Some(Arc::new(data)),
            ..ToolDoneEvent::error(CALL.into(), RAW)
        }));
        let update = attach(None, CALL, &event);
        let mut notification = json!({"params":{"update":update}});
        sanitize_keeping_raw(&mut notification);
        let update: ToolCallUpdate =
            serde_json::from_value(notification["params"]["update"].take()).unwrap();
        let AgentEvent::ToolDone(received) = read(update.meta.as_ref(), CALL).unwrap().unwrap()
        else {
            unreachable!()
        };
        assert_eq!(received.output.as_text(), text);
        let received = received.data.unwrap();
        assert_eq!(received.input["command"], RAW);
        assert_eq!(received.state["log"], text);
    }

    #[test_case(json!({"dropped":"oversize"}) ; "obsolete_payload")]
    #[test_case(json!({"type":"nudge"}) ; "not_a_tool_event")]
    #[test_case(json!({"type":"tool_progress", "id":"other", "append":""}) ; "wrong_call")]
    fn malformed_tool_data_is_refused(value: Value) {
        let meta = Meta::from_iter([(META_KEY.to_owned(), json!({TOOL_DATA_KEY:value}))]);
        assert!(read(Some(&meta), CALL).unwrap().is_err());
    }

    #[test]
    fn ordinary_fields_are_sanitized_without_touching_tool_data() {
        let mut message = json!({"params":{"update":{"title": RAW,
            "_meta":{META_KEY:{TOOL_DATA_KEY:{"input":RAW}}}}}});
        sanitize_keeping_raw(&mut message);
        assert_eq!(
            message["params"]["update"]["title"],
            sanitize::sanitize_display(RAW).as_ref()
        );
        assert_eq!(
            message["params"]["update"]["_meta"][META_KEY][TOOL_DATA_KEY]["input"],
            RAW
        );
        assert!(read(None, CALL).is_none());
    }
}
