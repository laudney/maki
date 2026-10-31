//! The plan extension: the agent owns a session's plan file, so the client
//! learns of a plan write from that call's completion, reads the plan with
//! `_maki/plan/read`, and saves an edit with `_maki/plan/write`. A path is
//! only ever a name on the client, never a file it opens.

use std::hash::{DefaultHasher, Hash, Hasher};

use agent_client_protocol_schema::v1::{Meta, SessionId, SessionUpdate};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::extensions::META_KEY;

pub const PLAN_KEY: &str = "plan";
pub const PLAN_VERSION: u64 = 1;
pub const READ_METHOD: &str = "_maki/plan/read";
pub const WRITE_METHOD: &str = "_maki/plan/write";
/// Past this a plan write's completion names the file without its text.
pub const PLAN_TEXT_LIMIT: usize = 256 * 1024;
pub const NO_PLAN: &str = "the session has no plan file";
pub const BUSY: &str = "the agent is running a prompt";
pub const CONFLICT: &str = "the plan changed on the agent";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanRead {
    pub session_id: SessionId,
}

/// `text` and `revision` are `None` while the file does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanText {
    pub path: String,
    pub text: Option<String>,
    pub revision: Option<String>,
}

/// `revision` is the one [`PlanText`] read, `None` for a file not written
/// yet.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanWrite {
    pub session_id: SessionId,
    pub text: String,
    pub revision: Option<String>,
}

/// On the completed update of a call that wrote the plan file. `text` only
/// after a full write that fits [`PLAN_TEXT_LIMIT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanWritten {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Compared only within one agent process: a read and the write that
/// follows it.
pub fn revision(text: &str) -> String {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Merged into what the update carries, so the tool data extension's metadata
/// stays beside it.
pub fn attach(update: SessionUpdate, written: &PlanWritten) -> SessionUpdate {
    let SessionUpdate::ToolCallUpdate(mut update) = update else {
        return update;
    };
    let mut meta = update.meta.take().unwrap_or_default();
    let maki = meta
        .entry(META_KEY.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(maki) = maki
        && let Ok(written) = serde_json::to_value(written)
    {
        maki.insert(PLAN_KEY.to_owned(), written);
    }
    SessionUpdate::ToolCallUpdate(update.meta(meta))
}

pub fn written(meta: Option<&Meta>) -> Option<PlanWritten> {
    let value = meta?.get(META_KEY)?.get(PLAN_KEY)?;
    serde_json::from_value(value.clone()).ok()
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::{ToolCallUpdate, ToolCallUpdateFields};
    use serde_json::json;

    use super::*;
    use crate::tool_data;
    use maki_types::event::{AgentEvent, ToolDoneEvent};

    const PATH: &str = "/agent/plans/plan.md";
    const TEXT: &str = "# Plan";
    const CALL: &str = "call-1";

    #[test]
    fn the_plan_write_sits_beside_tool_data() {
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            CALL,
            ToolCallUpdateFields::default(),
        ));
        let rendered = tool_data::attach(
            Some(update),
            CALL,
            &AgentEvent::ToolDone(Box::new(ToolDoneEvent::error(CALL.into(), TEXT))),
        );
        let written = PlanWritten {
            path: PATH.into(),
            text: Some(TEXT.into()),
        };
        let SessionUpdate::ToolCallUpdate(update) = attach(rendered, &written) else {
            panic!("expected a tool call update");
        };
        assert_eq!(super::written(update.meta.as_ref()), Some(written));
        assert!(tool_data::read(update.meta.as_ref(), CALL).is_some_and(|render| render.is_ok()));
    }

    #[test]
    fn a_plan_without_text_keeps_its_wire_shape() {
        let written = PlanWritten {
            path: PATH.into(),
            text: None,
        };
        assert_eq!(
            serde_json::to_value(&written).unwrap(),
            json!({ "path": PATH })
        );
    }
}
