//! The data tables of the calls a renderer drew last, kept so a click
//! changes the same renderer state without replaying the call's clicks, and
//! the turns that let one draw or click of a call run at a time.

use std::collections::HashMap;
use std::sync::Arc;

use mlua::{Lua, RegistryKey, Table};
use tokio::sync::{Mutex, OwnedMutexGuard};

/// Purely a cache, as the bridge's warm tools are: a click that misses it
/// draws the call again and replays its earlier clicks, so eviction costs
/// time, never the result.
pub(crate) const WARM_CALL_CAP: usize = 32;

/// Which renderer drew a call, so a warm entry serves only that renderer.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DrawnBy {
    pub plugin: Arc<str>,
    pub tool: Arc<str>,
    pub version: u32,
}

struct WarmCall {
    id: String,
    by: DrawnBy,
    data: RegistryKey,
}

/// Oldest first.
#[derive(Default)]
pub(crate) struct WarmCalls(Vec<WarmCall>);

fn release(lua: &Lua, gone: impl IntoIterator<Item = WarmCall>) {
    for call in gone {
        if let Err(e) = lua.remove_registry_value(call.data) {
            tracing::warn!(id = call.id, plugin = %call.by.plugin, error = %e, "failed to drop warm call key");
        }
    }
}

/// Keeps {data} of call {id} as the newest entry, in place of any older one
/// of the call.
pub(crate) fn warm(lua: &Lua, id: &str, by: DrawnBy, data: &Table) {
    let Some(mut calls) = lua.app_data_mut::<WarmCalls>() else {
        return;
    };
    let mut gone: Vec<WarmCall> = calls.0.extract_if(.., |call| call.id == id).collect();
    match lua.create_registry_value(data.clone()) {
        Ok(data) => {
            calls.0.push(WarmCall {
                id: id.to_owned(),
                by,
                data,
            });
            if calls.0.len() > WARM_CALL_CAP {
                gone.push(calls.0.remove(0));
            }
        }
        Err(e) => {
            tracing::warn!(id, plugin = %by.plugin, error = %e, "cannot keep a drawn call warm")
        }
    }
    drop(calls);
    release(lua, gone);
}

/// The data table of call {id}, when {by} drew it last.
pub(crate) fn warm_data(lua: &Lua, id: &str, by: &DrawnBy) -> Option<Table> {
    let calls = lua.app_data_ref::<WarmCalls>()?;
    let call = calls
        .0
        .iter()
        .find(|call| call.id == id && call.by == *by)?;
    lua.registry_value(&call.data).ok()
}

pub(crate) fn evict(lua: &Lua, id: &str) {
    evict_where(lua, |call| call.id == id);
}

/// A reload of {plugin} leaves its renderer's state stale.
pub(crate) fn clear_plugin_warm(lua: &Lua, plugin: &str) {
    evict_where(lua, |call| &*call.by.plugin == plugin);
}

fn evict_where(lua: &Lua, doomed: impl Fn(&WarmCall) -> bool) {
    let gone: Vec<WarmCall> = match lua.app_data_mut::<WarmCalls>() {
        Some(mut calls) => calls.0.extract_if(.., |call| doomed(call)).collect(),
        None => return,
    };
    release(lua, gone);
}

/// One lock per call with a draw or a click running or waiting.
#[derive(Default)]
pub(crate) struct CallTurns(HashMap<String, Arc<Mutex<()>>>);

/// The right of one draw or click of a call to run, so a click reads the
/// data the one before it left, held until it drops.
pub(crate) struct CallTurn {
    lua: Lua,
    id: String,
    guard: Option<OwnedMutexGuard<()>>,
}

/// Waits for every draw and click of call {id} that came before.
pub(crate) async fn take_turn(lua: &Lua, id: &str) -> CallTurn {
    let lock = lua
        .app_data_mut::<CallTurns>()
        .map(|mut turns| Arc::clone(turns.0.entry(id.to_owned()).or_default()));
    let guard = match lock {
        Some(lock) => Some(lock.lock_owned().await),
        None => None,
    };
    CallTurn {
        lua: lua.clone(),
        id: id.to_owned(),
        guard,
    }
}

impl Drop for CallTurn {
    /// The lock goes with the last turn of the call: a waiter holds a share.
    fn drop(&mut self) {
        drop(self.guard.take());
        if let Some(mut turns) = self.lua.app_data_mut::<CallTurns>()
            && turns
                .0
                .get(&self.id)
                .is_some_and(|lock| Arc::strong_count(lock) == 1)
        {
            turns.0.remove(&self.id);
        }
    }
}
