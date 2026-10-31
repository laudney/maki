//! The agent domain and its session facade.

use std::collections::HashMap;
use std::sync::Arc;

use maki_agent::permissions::PluginRuleStore;
use maki_agent::tools::ToolRegistry;
use mlua::{Function, Lua, RegistryKey, Value as LuaValue};
use serde_json::Value;

use crate::api::util::convert::json_to_lua;
use crate::docs::{DocEntry, ModuleDoc, compose_docs};
use crate::domain::Domain;
use crate::runtime::RequestLanes;

pub(crate) mod agent;
pub(crate) mod facade;

pub(crate) use agent::{AgentDomain, AgentGrant, AgentHandles};

/// Build the agent binding and its request handles.
pub(crate) fn compose(
    lanes: &RequestLanes,
    registry: Arc<ToolRegistry>,
    plugin_rules: Arc<PluginRuleStore>,
) -> (Box<dyn Domain>, AgentHandles) {
    let agent = AgentHandles::default();
    let agent_domain = Box::new(AgentDomain {
        pending: Arc::default(),
        registry,
        plugin_rules,
        tx: lanes.tx.clone(),
        prio_tx: lanes.prio_tx.clone(),
        layered: Arc::clone(&agent.layered),
        background: agent.background.clone(),
    });
    (agent_domain, agent)
}

/// Combines agent and UI reference entries for `maki-docgen`.
pub fn api_docs(ui: &'static [DocEntry]) -> Vec<&'static ModuleDoc> {
    compose_docs([agent::DOCS, maki_lua_core::facade::DOCS, ui])
}

/// One domain's callbacks of each tool, by plugin and tool name, kept in Lua
/// app data. A plugin is in it from the commit of its load until its next
/// load or its clear.
pub(crate) struct ToolRecords<K>(pub HashMap<Arc<str>, HashMap<Arc<str>, K>>);

impl<K> Default for ToolRecords<K> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

/// Resolves a plugin callback and converts its json input, warning on
/// failure. `None` when the tool has no such callback registered.
pub(crate) fn plugin_fn<K: 'static>(
    lua: &Lua,
    plugin: &str,
    tool: &str,
    callback: &'static str,
    key: impl FnOnce(&K) -> Option<&RegistryKey>,
    input: &Value,
) -> Option<(Function, LuaValue)> {
    let func = {
        let records = lua.app_data_ref::<ToolRecords<K>>()?;
        let key = key(records.0.get(plugin)?.get(tool)?)?;
        match lua.registry_value::<Function>(key) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(plugin, tool, callback, error = %e, "callback registry lookup failed");
                return None;
            }
        }
    };
    match json_to_lua(lua, input) {
        Ok(v) => Some((func, v)),
        Err(e) => {
            tracing::warn!(plugin, tool, callback, error = %e, "callback input conversion failed");
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::compose;
    use crate::domain::Domain;
    use crate::runtime::RequestLanes;
    use maki_agent::permissions::PluginRuleStore;
    use maki_agent::tools::ToolRegistry;
    use std::sync::Arc;

    pub(crate) fn domains() -> Box<dyn Domain> {
        compose(
            &RequestLanes::new(),
            Arc::new(ToolRegistry::new()),
            Arc::new(PluginRuleStore::default()),
        )
        .0
    }
}
