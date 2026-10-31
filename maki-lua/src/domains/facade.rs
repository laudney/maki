//! The facade's local backend: the agent's live and stored sessions, its
//! mailbox and the model registry, and the headless snapshot slot.

use std::sync::Arc;

use futures::future::LocalBoxFuture;
use maki_agent::SessionMailbox;
use maki_agent::agent::{live_header, live_history, live_snapshot, transcript_into_session};
use maki_agent::session::store_new;
use maki_lua_core::facade::{FacadeOps, LocalOps};
use maki_providers::{Model, ModelTier, model_registry};
use maki_storage::StateDir;
use maki_storage::id::MakiId;
use maki_types::message::{HistorySnapshot, Message};
use maki_types::session::{SessionHeader, StoredSession};
use maki_types::transcript::Transcript;
use mlua::{Lua, Result as LuaResult, Table};
use serde_json::Value as JsonValue;

use crate::api::model::model_info_table;
use crate::domain::{DomainRequest, Vm};

/// Answers `maki.session.read` for a driver that has no UI to ask. Takes the
/// optional session id from Lua and returns a serialized
/// [`crate::SessionSnapshot`].
pub type SessionSnapshotFn =
    Box<dyn Fn(Option<&str>) -> Result<serde_json::Value, String> + Send + Sync + 'static>;

pub struct SessionSnapshotSlot(pub SessionSnapshotFn);

struct AgentLocal;

impl LocalOps for AgentLocal {
    fn headless_snapshot(&self, lua: &Lua, id: Option<&str>) -> Option<Result<JsonValue, String>> {
        lua.app_data_ref::<SessionSnapshotSlot>()
            .map(|slot| (slot.0)(id))
    }

    fn live_header(&self, id: MakiId) -> Option<Arc<SessionHeader>> {
        live_header(id)
    }

    fn live_history(&self, id: MakiId) -> Option<Arc<Vec<Message>>> {
        live_history(id)
    }

    fn live_snapshot(&self, id: MakiId) -> Option<Arc<HistorySnapshot>> {
        live_snapshot(id)
    }

    fn read_stored(&self, id: MakiId, dir: &StateDir) -> Result<StoredSession, String> {
        StoredSession::read_only(id, dir).map_err(|e| e.to_string())
    }

    fn store_new(
        &self,
        transcript: Transcript,
        base: StoredSession,
        dir: &StateDir,
    ) -> Result<MakiId, String> {
        let session = transcript_into_session(transcript, base).map_err(|e| e.to_string())?;
        store_new(session, dir).map_err(|e| e.to_string())
    }

    fn notify(
        &self,
        id: MakiId,
        text: String,
        display: Option<&str>,
        wake: bool,
    ) -> Result<(), String> {
        SessionMailbox::notify(id, text, display, wake).map_err(|e| e.to_string())
    }

    fn model(&self, lua: &Lua, spec: &str) -> LuaResult<Result<Table, String>> {
        match Model::from_spec(spec) {
            Ok(model) => model_info_table(lua, &model).map(Ok),
            Err(error) => Ok(Err(error.to_string())),
        }
    }

    fn resolve_tier(&self, provider: Option<&str>, tier: ModelTier) -> Option<String> {
        model_registry::resolve_tier(provider, tier)
    }

    fn tier_assigned(&self, spec: &str, tier: ModelTier) -> bool {
        model_registry::override_tiers(spec).contains(&tier)
    }
}

pub(crate) fn facade_ops() -> FacadeOps {
    FacadeOps::Agent(Arc::new(AgentLocal))
}

/// Install a `SessionSnapshotSlot` on the Lua thread. Headless drivers send
/// one so `maki.session.read` works without a UI to ask.
pub(crate) struct InstallSessionSnapshot {
    pub provider: SessionSnapshotFn,
}

impl DomainRequest for InstallSessionSnapshot {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self { provider } = *self;
        Box::pin(async move {
            vm.rt.lua.set_app_data(SessionSnapshotSlot(provider));
        })
    }
}
