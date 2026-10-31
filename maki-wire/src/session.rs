use agent_client_protocol_schema::v1::SessionId;
use maki_storage::id::MakiId;
use maki_storage::sessions::{QueueReceipt, StoredWriters};
use maki_types::event::{Envelope, SessionEndReason};
use maki_types::inbox::InboxSnapshot;
use maki_types::input::{AgentInput, SessionInput};
use maki_types::transcript::Transcript;
use serde::{Deserialize, Serialize};

pub const KEY: &str = "sessionOperations";
pub const VERSION: u64 = 1;
pub const METHOD: &str = "_maki/session";
pub const RESTORE_METHOD: &str = "_maki/session/restore";
pub const TASK_UPDATE_METHOD: &str = "_maki/task/update";
pub const TASK_PERMISSION_KEY: &str = "makiTask";
pub const PLUGIN_TASK_KEY: &str = "makiPluginTask";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskUpdate {
    pub session_id: SessionId,
    pub envelope: Envelope,
    pub inbox: Option<InboxSnapshot>,
}

pub const QUEUE_UPDATE_METHOD: &str = "_maki/queue/update";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueUpdate {
    pub session_id: SessionId,
    pub receipt: QueueReceipt,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "operation",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SessionOperation {
    RetryAuth {
        session_id: SessionId,
        id: Option<String>,
    },
    MoveTool {
        session_id: SessionId,
        id: String,
    },
    Enqueue {
        session_id: SessionId,
        #[serde(default)]
        id: Option<MakiId>,
        input: Box<SessionInput>,
    },
    CancelQueued {
        session_id: SessionId,
        id: MakiId,
    },
    Mcp {
        session_id: SessionId,
        #[serde(default)]
        toggle: Option<(String, bool)>,
    },
    ChangeDirectory {
        session_id: SessionId,
        path: String,
    },
    FireEvent {
        session_id: SessionId,
        event: String,
        data: serde_json::Value,
    },
    StopPluginTask {
        session_id: SessionId,
        id: String,
    },
    Snapshot {
        session_id: SessionId,
    },
    Read {
        session_id: SessionId,
    },
    Import {
        parent: SessionId,
        transcript: Box<Transcript>,
    },
    Fork {
        session_id: SessionId,
        #[serde(default)]
        turn: Option<(usize, String, Option<StoredWriters>)>,
    },
    SetTitle {
        session_id: SessionId,
        title: String,
    },
    Delete {
        session_id: SessionId,
    },
    Close {
        session_id: SessionId,
        #[serde(default)]
        reason: Option<SessionEndReason>,
    },
    Rewind {
        session_id: SessionId,
        turn_index: usize,
        prompt: String,
    },
    TaskInput {
        session_id: SessionId,
        id: String,
        input: Box<AgentInput>,
    },
    TaskPop {
        session_id: SessionId,
        id: String,
    },
    RemoveTask {
        session_id: SessionId,
        id: String,
    },
    CancelSubagent {
        session_id: SessionId,
        id: String,
    },
}

pub use maki_types::MoveOutcome;
