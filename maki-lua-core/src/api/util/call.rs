//! A tool call as Lua reads it, the one shape for a renderer's `data`, for
//! what `maki.agent.call_tool` hands back and for each record of a call's
//! children.

use maki_types::tool::{ChildStatus, ToolChild, ToolData, ToolOutput};
use mlua::{IntoLua, Lua, LuaSerdeExt, Result as LuaResult, Table};

use super::convert::{json_to_lua, serde_to_lua};

const TOOL_KEY: &str = "tool";
const VERSION_KEY: &str = "version";
const INPUT_KEY: &str = "input";
const OUTPUT_KEY: &str = "output";
const STATE_KEY: &str = "state";
const STATUS_KEY: &str = "status";
pub const SUMMARY_KEY: &str = "summary";
const ANNOTATIONS_KEY: &str = "annotations";
pub const CHILDREN_KEY: &str = "children";
pub const REWRITTEN_KEY: &str = "rewritten";
const USAGE_KEY: &str = "usage";
pub const INDEX_KEY: &str = "index";

/// {data} with its {output}, None while it runs, and its {status}.
pub fn call_to_lua(
    lua: &Lua,
    data: &ToolData,
    output: Option<&ToolOutput>,
    status: ChildStatus,
) -> LuaResult<Table> {
    let table = lua.create_table()?;
    table.set(TOOL_KEY, &*data.tool)?;
    table.set(VERSION_KEY, data.version)?;
    table.set(INPUT_KEY, json_to_lua(lua, &data.input)?)?;
    table.set(
        OUTPUT_KEY,
        output.map(|output| serde_to_lua(lua, output)).transpose()?,
    )?;
    table.set(STATE_KEY, json_to_lua(lua, &data.state)?)?;
    table.set(STATUS_KEY, serde_to_lua(lua, &status)?)?;
    table.set(SUMMARY_KEY, data.summary.as_str())?;
    let annotations = list(lua, data.annotations.iter().map(String::as_str))?;
    table.set(ANNOTATIONS_KEY, annotations)?;
    let children = data
        .children
        .iter()
        .map(|child| child_to_lua(lua, child))
        .collect::<LuaResult<Vec<Table>>>()?;
    table.set(CHILDREN_KEY, list(lua, children)?)?;
    table.set(REWRITTEN_KEY, data.rewritten)?;
    Ok(table)
}

/// A child's record: its call, its `usage` and its 1-based invocation
/// `index`.
pub fn child_to_lua(lua: &Lua, child: &ToolChild) -> LuaResult<Table> {
    let table = call_to_lua(lua, &child.data, child.output.as_deref(), child.status)?;
    table.set(USAGE_KEY, child.usage.as_deref())?;
    table.set(INDEX_KEY, lua_index(child.index))?;
    Ok(table)
}

/// The path of a nested call as 1-based invocation indices, the way each
/// record's `index` reads.
pub fn path_to_lua(lua: &Lua, path: &[u32]) -> LuaResult<Table> {
    list(lua, path.iter().copied().map(lua_index))
}

fn lua_index(index: u32) -> i64 {
    i64::from(index) + 1
}

/// A sequence that encodes as a JSON array even when empty, as
/// [`json_to_lua`] builds them.
fn list<T: IntoLua>(lua: &Lua, items: impl IntoIterator<Item = T>) -> LuaResult<Table> {
    let table = lua.create_sequence_from(items)?;
    table.set_metatable(Some(lua.array_metatable()))?;
    Ok(table)
}
