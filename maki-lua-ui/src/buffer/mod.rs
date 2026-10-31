//! What the agent sends the UI to draw: buffers, highlight, markdown and
//! theme styles on `maki.ui`, with aliases of the `maki.text` formatting
//! helpers.

use maki_lua_core::runtime::TaskExts;
use maki_lua_core::{api, docs, runtime};
use mlua::{Lua, Result as LuaResult, Table};

mod blit;
pub mod buf;
pub mod render;
mod task;

use task::BufsView;

pub(crate) fn extend(lua: &Lua, maki: &Table) -> LuaResult<()> {
    let table = lua.create_table()?;
    render::add_bridge_fns(&table, lua)?;
    render::add_text_aliases(&table, &maki.get("text")?)?;
    maki.set("ui", table)
}

pub(crate) fn begin_task(exts: &mut TaskExts) {
    exts.get_or_insert_with(BufsView::default);
}
