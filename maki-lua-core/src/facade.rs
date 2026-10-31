//! Shared plugin namespaces over one backend: the agent's local resources
//! or the client's UI requests. A runtime never contains both.

use std::sync::Arc;

use futures::future::BoxFuture;
use maki_config::providers::ModelTier;
use maki_storage::StateDir;
use maki_storage::id::MakiId;
use maki_types::message::{HistorySnapshot, Message};
use maki_types::session::{SessionHeader, StoredSession};
use maki_types::transcript::{SessionExport, Transcript};
use mlua::{Lua, Result as LuaResult, Table, Value};
use serde_json::Value as JsonValue;

use crate::api::util::convert::json_to_lua;
use crate::api::util::pair::{Pair, try_pair};
use crate::api::{model, plan, session, task};
use crate::docs::DocEntry;
use crate::domain::LoadScope;
use crate::runtime::in_plugin_load;

pub const NO_UI_ERR: &str = "no interactive UI attached";
pub const UI_DROPPED_ERR: &str = "ui event loop dropped the request";
pub const PLUGIN_LOADING_ERR: &str = "not available while a plugin loads: call it from a callback";
/// What a call answers that only the agent's own sessions could, in a
/// runtime with no agent.
pub const NO_LOCAL_ERR: &str = "no local agent attached";

pub const DOCS: &[DocEntry] = &[
    DocEntry::new(50, &plan::DOCS),
    DocEntry::new(240, &model::DOCS),
    DocEntry::new(290, &session::DOCS),
    DocEntry::new(320, &task::DOCS),
];

pub type UiReply = Result<JsonValue, String>;

pub enum SessionRequest {
    List,
    Live,
    Current,
    /// UI-mode fallback for `maki.session.read` when no snapshot slot is
    /// installed — the UI resolves the id and returns a `session_snapshot_json`
    /// payload, or [`crate::session_snapshot::SessionSnapshot::stored`] for an
    /// id no tab has open. Headless drivers install a `SessionSnapshotSlot`
    /// instead so `read` doesn't need a UI.
    Read {
        id: Option<String>,
    },
    /// {transcript} is parsed but not validated yet. The UI lays it over the
    /// focused tab's blank session, the one place that decides a new
    /// session's defaults, and answers what is wrong with it. Without
    /// {prompt} or {focus} it is only stored, otherwise it opens in a tab.
    New {
        transcript: Box<Transcript>,
        prompt: Option<String>,
        focus: bool,
    },
    Prompt {
        id: Option<String>,
        text: String,
    },
    Focus {
        id: String,
    },
    Delete {
        id: String,
    },
    /// Plan or build, for the live session {id} names: a plan form row fires
    /// for the session its plan belongs to, which is not always the focused
    /// one.
    SetMode {
        id: Option<String>,
        mode: String,
    },
    SetTitle {
        id: String,
        title: String,
    },
}

pub enum TaskRequest {
    List,
    Focus { id: String },
    Remove { id: String },
    Stop { id: String, session: String },
}

pub enum ModelRequest {
    Get,
    Available,
    Set {
        spec: Option<String>,
        thinking: Option<String>,
        fast: Option<bool>,
    },
}

/// The plan surface `maki.plan` drives. Plan state is per session, so every
/// request names one, and `None` means the focused session.
pub struct PlanRequest {
    pub session: Option<String>,
}

/// A request a UI answers with JSON.
pub enum FacadeRequest {
    Session(SessionRequest),
    Model(ModelRequest),
    Plan(PlanRequest),
    Task(TaskRequest),
}

/// What the agent answers from its own process: the headless snapshot slot,
/// live sessions, stored sessions, the mailbox and the model registry.
pub trait LocalOps: Send + Sync {
    /// The slot a headless driver installs for the one session it runs,
    /// `None` when there is none.
    fn headless_snapshot(&self, lua: &Lua, id: Option<&str>) -> Option<Result<JsonValue, String>>;
    fn live_header(&self, id: MakiId) -> Option<Arc<SessionHeader>>;
    fn live_history(&self, id: MakiId) -> Option<Arc<Vec<Message>>>;
    fn live_snapshot(&self, id: MakiId) -> Option<Arc<HistorySnapshot>>;
    /// Blocking, without claiming the session.
    fn read_stored(&self, id: MakiId, dir: &StateDir) -> Result<StoredSession, String>;
    /// Blocking: lays {transcript} over {base} and stores the result.
    fn store_new(
        &self,
        transcript: Transcript,
        base: StoredSession,
        dir: &StateDir,
    ) -> Result<MakiId, String>;
    fn notify(
        &self,
        id: MakiId,
        text: String,
        display: Option<&str>,
        wake: bool,
    ) -> Result<(), String>;
    fn model(&self, lua: &Lua, spec: &str) -> LuaResult<Result<Table, String>>;
    fn resolve_tier(&self, provider: Option<&str>, tier: ModelTier) -> Option<String>;
    /// Whether the user picked {spec} for {tier}.
    fn tier_assigned(&self, spec: &str, tier: ModelTier) -> bool;
}

/// The round trips a UI answers. Not reaching it is [`NO_UI_ERR`], and a UI
/// that took the request and never answered is [`UI_DROPPED_ERR`].
pub trait UiOps: Send + Sync {
    fn request(&self, req: FacadeRequest) -> BoxFuture<'_, Result<UiReply, &'static str>>;
    /// Typed rather than JSON, with the messages shared, so neither side
    /// copies them. {id} goes as the plugin named it: a remote tab's is its
    /// agent's, which need not read as a local one.
    fn export(
        &self,
        id: String,
    ) -> BoxFuture<'_, Result<Result<SessionExport, String>, &'static str>>;
}

#[derive(Clone)]
pub enum FacadeOps {
    Agent(Arc<dyn LocalOps>),
    Ui(Option<Arc<dyn UiOps>>),
}

impl FacadeOps {
    pub fn local(&self) -> Result<&Arc<dyn LocalOps>, &'static str> {
        match self {
            Self::Agent(local) => Ok(local),
            Self::Ui(_) => Err(NO_LOCAL_ERR),
        }
    }

    pub fn headless_snapshot(
        &self,
        lua: &Lua,
        id: Option<&str>,
    ) -> Option<Result<JsonValue, String>> {
        self.local().ok()?.headless_snapshot(lua, id)
    }

    pub fn live_header(&self, id: MakiId) -> Option<Arc<SessionHeader>> {
        self.local().ok()?.live_header(id)
    }

    pub fn live_history(&self, id: MakiId) -> Option<Arc<Vec<Message>>> {
        self.local().ok()?.live_history(id)
    }

    pub fn live_snapshot(&self, id: MakiId) -> Option<Arc<HistorySnapshot>> {
        self.local().ok()?.live_snapshot(id)
    }

    /// Only works from a callback: the UI answers between frames, so a call
    /// from a plugin's top level fails.
    pub async fn ask(&self, req: FacadeRequest) -> Result<UiReply, &'static str> {
        self.ui()?.request(req).await
    }

    pub async fn export(&self, id: &str) -> Result<Result<SessionExport, String>, &'static str> {
        self.ui()?.export(id.to_owned()).await
    }

    /// [`Self::ask`], shaped into the `(value, err)` pair Lua expects. Never
    /// reaching the UI and the UI refusing both land in the error slot.
    pub async fn ask_json(&self, lua: &Lua, req: FacadeRequest) -> LuaResult<Pair<Value>> {
        let reply = try_pair!(self.ask(req).await);
        let value = try_pair!(reply);
        Ok((Some(json_to_lua(lua, &value)?), None))
    }

    fn ui(&self) -> Result<&Arc<dyn UiOps>, &'static str> {
        if in_plugin_load() {
            return Err(PLUGIN_LOADING_ERR);
        }
        match self {
            Self::Ui(Some(ui)) => Ok(ui),
            _ => Err(NO_UI_ERR),
        }
    }
}

pub fn extend(lua: &Lua, maki: &Table, load: &LoadScope<'_>, ops: &FacadeOps) -> LuaResult<()> {
    let permissions = load.reach.permissions();
    maki.set(
        "plan",
        plan::create_plan_table(lua, ops.clone(), permissions)?,
    )?;
    maki.set(
        "session",
        session::create_session_table(lua, ops.clone(), permissions, permissions.clone())?,
    )?;
    maki.set(
        "model",
        model::create_model_table(lua, ops.clone(), permissions)?,
    )?;
    maki.set(
        "task",
        task::create_task_table(lua, ops.clone(), permissions)?,
    )?;
    Ok(())
}
