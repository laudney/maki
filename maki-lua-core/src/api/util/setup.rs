use std::sync::{Arc, Mutex};

use maki_config::{RawConfig, UI_LUA_SECTIONS, UI_LUA_STORAGE_KEYS};
use mlua::{Function, Lua, LuaSerdeExt, Result as LuaResult, Table, Value};

use crate::api::{pack::packadd__doc, split::split__doc};
use crate::docs::{DocKind, FnDoc, ModuleDoc, ParamDoc};

pub(crate) type ConfigStore = Arc<Mutex<Setup>>;

const DOUBLE_SETUP_MSG: &str = "maki.setup() already called in this init.lua";
const STORAGE_SECTION: &str = "storage";

/// What one config file's `maki.setup` call set.
#[derive(Debug, Default)]
pub struct Setup {
    pub raw: Option<RawConfig>,
    /// The sections a `ui.lua` set that only the agent reads, which it
    /// dropped.
    pub agent_only: Vec<String>,
}

pub(crate) const DOCS: ModuleDoc = ModuleDoc {
    name: "maki",
    kind: DocKind::Table,
    desc: "The global entry point. Every API lives under this table.",
    fns: &[
        FnDoc {
            name: "setup",
            args: "{config}",
            desc: "Apply your personal configuration. This is only available inside \
`init.lua` (not in plugins) and can be called at most once. The table \
accepts the same keys as the Configuration reference.",
            params: &[ParamDoc {
                name: "{config}",
                ty: "table",
                desc: "Configuration table.",
            }],
            returns: "",
            guard: None,
            value: false,
            example: "maki.setup({\n\
  model = \"opus\",\n\
  keymaps = false,\n\
})",
        },
        split__doc,
        packadd__doc,
    ],
};

/// `maki.setup` for one config file. In a `ui.lua` ({ui_lua}) it keeps only
/// the sections the UI reads, after the whole table was checked as any
/// config is, and records the rest.
pub(crate) fn create_setup_fn(lua: &Lua, store: ConfigStore, ui_lua: bool) -> LuaResult<Function> {
    lua.create_function(move |lua, table: Value| {
        let agent_only = match &table {
            Value::Table(table) if ui_lua => agent_only_sections(table)?,
            _ => Vec::new(),
        };
        let raw: RawConfig = lua
            .from_value(table)
            .map_err(|e| mlua::Error::runtime(e.to_string()))?;
        let mut guard = store.lock().unwrap();
        if guard.raw.is_some() {
            return Err(mlua::Error::runtime(DOUBLE_SETUP_MSG));
        }
        guard.raw = Some(if ui_lua { raw.into_ui_sections() } else { raw });
        guard.agent_only = agent_only;
        Ok(())
    })
}

/// The sections of {table}, a `maki.setup` table, that only the agent
/// reads, sorted, with a `storage` key named by its path.
fn agent_only_sections(table: &Table) -> LuaResult<Vec<String>> {
    let mut sections = Vec::new();
    for pair in table.pairs::<Value, Value>() {
        let (Value::String(key), value) = pair? else {
            continue;
        };
        let key = key.to_string_lossy();
        match value {
            Value::Table(storage) if key == STORAGE_SECTION => {
                for pair in storage.pairs::<Value, Value>() {
                    let (Value::String(name), _) = pair? else {
                        continue;
                    };
                    let name = name.to_string_lossy();
                    if !UI_LUA_STORAGE_KEYS.contains(&name.as_str()) {
                        sections.push(format!("{STORAGE_SECTION}.{name}"));
                    }
                }
            }
            _ if UI_LUA_SECTIONS.contains(&key.as_str()) => {}
            _ => sections.push(key),
        }
    }
    sections.sort();
    Ok(sections)
}
