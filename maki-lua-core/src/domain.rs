//! One VM's bindings and lifecycle hooks, kept outside the shared core.

use std::any::Any;
use std::rc::Rc;
use std::sync::Arc;

use futures::future::LocalBoxFuture;
use maki_storage::id::MakiId;
use mlua::{Lua, Result as LuaResult, Table};
use tokio::task::LocalSet;

use crate::docs::DocEntry;
use crate::error::PluginError;
use crate::plugin_permissions::{Component, PluginPermissions, PluginReach};
use crate::runtime::{InflightGate, LuaRuntime, TaskExts};

/// What a [`DomainRequest`] is served with on the VM thread.
pub struct Vm<'a> {
    pub rt: &'a LuaRuntime,
    pub ex: &'a Rc<LocalSet>,
    pub gate: &'a Rc<InflightGate>,
}

/// A request a domain sends its part of the VM. The request loop awaits
/// `serve` in place, so a request that must not hold the loop spawns its
/// work on `vm.ex` and returns at once.
pub trait DomainRequest: Send + Any {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()>;
}

pub struct LoadScope<'a> {
    pub plugin: &'a Arc<str>,
    /// Already owned by {plugin}.
    pub reach: &'a PluginReach,
    /// The per-load state a domain needs and the core never reads, such as
    /// the agent's provider grant. Each domain downcasts to its own type.
    pub grant: &'a dyn Any,
}

/// What a bundled plugin opened outside its own load runs with, by name: its
/// manifest's permissions and the VM's grant for a bundled load,
/// which the core passes on unread.
pub type BundledGrant = fn(&str) -> Result<(PluginPermissions, Box<dyn Any + Send>), PluginError>;

/// `Sync` so task scopes can reach their VM's lifecycle hooks.
pub trait Domain: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn component(&self) -> Component {
        Component::Agent
    }
    /// Once, on the VM thread, before any load: the domain's stores go into
    /// Lua app data, and its thread-local callbacks are set.
    fn install(&mut self, _lua: &Lua) -> LuaResult<()> {
        Ok(())
    }
    fn start(&self, _lua: &Lua) {}
    fn extend(&self, lua: &Lua, maki: &Table, load: &LoadScope<'_>) -> LuaResult<()>;
    /// Before a load's chunks run: drop what the previous load of the name
    /// published, and revive what `clear_plugin` tombstoned.
    fn reload(&self, _lua: &Lua, _plugin: &str) {}
    /// After the chunks ran: make what the load registered live. A failure
    /// rolls the whole load back.
    fn commit_load(&self, _lua: &Lua, _load: &LoadScope<'_>) -> Result<(), PluginError> {
        Ok(())
    }
    /// A failed load, after `clear_plugin`: drop what it left pending. The
    /// provider grant is kept.
    fn rollback_load(&self, _lua: &Lua, _plugin: &str) {}
    fn clear_plugin(&self, _lua: &Lua, _plugin: &str) {}
    /// The explicit unload only, after `clear_plugin`.
    fn unload(&self, _lua: &Lua, _plugin: &str) {}
    /// After the `SessionEnd` handlers ran and the session's jobs were reaped.
    fn end_session(&self, _lua: &Lua, _session: MakiId) {}
    /// A task scope starts on a cell: the extensions every scope carries.
    fn begin_task(&self, _lua: &Lua, _exts: &mut TaskExts) {}
    /// A task scope ended, after its jobs were reaped.
    fn end_task(&self, _lua: &Lua, _task: u64) {}
    fn docs(&self) -> &'static [DocEntry] {
        &[]
    }
}
