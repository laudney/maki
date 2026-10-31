//! `maki.plan`: the plan-mode surface. Plugins read the current plan state,
//! layer `ui.plan_form.actions` to shape the form's menu, and layer
//! `ui.plan_form` to take the form over.

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Table, Value};

use crate::api::util::convert::TableExt;
use crate::api::util::pair::Pair;
use crate::facade::{FacadeOps, FacadeRequest, PlanRequest};
use crate::plugin_permissions::PluginPermissions;

/// Read the current plan state. Returns `{ mode, path, content, ready }`:
/// - `mode` is `"plan"` or `"build"`.
/// - `path` is the absolute plan path once the session has one, else `nil`.
/// - `ready` is `true` once the agent has written the plan file.
/// - `content` is the file contents, `nil` when the plan is not ready or the
///   read failed.
///
/// @param opts table? `session` (string?) Session id, defaults to focused.
/// @return (table|nil, string|nil) Plan snapshot table, or nil and an error.
/// @example
/// local plan, err = maki.plan.read({ session = id })
/// if plan and plan.ready then
///   print(plan.path, plan.content)
/// end
#[lua_fn(guard = SessionRead)]
async fn read(lua: Lua, #[ctx] ops: FacadeOps, opts: Option<Table>) -> LuaResult<Pair<Value>> {
    let session = match opts {
        Some(opts) => opts.opt("session")?,
        None => None,
    };
    ops.ask_json(&lua, FacadeRequest::Plan(PlanRequest { session }))
        .await
}

lua_table! {
    /// Plan-mode surface for plugins.
    ///
    /// Read the plan, and shape the plan form by layering the two slots maki
    /// fires around it. Plan state is per session, so every call takes an
    /// optional `session` and defaults to the focused tab. Under
    /// `maki connect`, `read` reads the connected agent's plan.
    ///
    /// `ui.plan_form.actions` is the menu. The default answers with the
    /// built-in rows, each `{ id, label, desc, action }`, and a layer
    /// appends, reorders or drops them before returning the list. Every row
    /// needs an `id` no other row uses, since that is how a later layer finds
    /// it. Anything past the 32nd row is dropped.
    ///
    /// A row carrying a `handler` has that function called on the Lua thread
    /// with `{ session, path, parallel }` when the user picks it. The row's
    /// `action` runs after the handler returns, unless the handler returned
    /// `false` or failed, so copying a built-in row and adding a handler
    /// keeps the built-in outcome. Drop the `action` to replace it.
    ///
    /// `ui.plan_form` is the form itself. A layer that answers `false` keeps
    /// it closed and renders the plan however it likes.
    ///
    /// Layering either slot costs every permission, the price of steering a
    /// call whose reach nobody declared: a row decides what pressing Enter
    /// does, up to a build-mode turn with every tool behind it.
    ///
    /// Unloading your plugin hands the form back and reaps its row handlers.
    ///
    /// ```lua
    /// -- A row of your own, next to the built-in ones:
    /// maki.api.set_slot("ui.plan_form.actions", function(prev, ev)
    ///   local rows = prev(ev)
    ///   table.insert(rows, {
    ///     id = "commit_and_implement",
    ///     label = "Commit and implement",
    ///     desc = "Commit the plan file first, then implement it",
    ///     handler = function(opts)
    ///       maki.fn.system({ "git", "commit", "-am", "plan" })
    ///       maki.session.set_mode("build", { session = opts.session })
    ///       maki.session.prompt("Implement " .. opts.path, { session = opts.session })
    ///     end,
    ///   })
    ///   return rows
    /// end)
    ///
    /// -- Render the plan yourself for as long as this plugin is loaded:
    /// maki.api.set_slot("ui.plan_form", function(prev, ev)
    ///   local plan = maki.plan.read({ session = ev.session })
    ///   return false
    /// end)
    /// ```
    "maki.plan" => pub fn create_plan_table(
        ops: FacadeOps,
        perms: &PluginPermissions,
    ),
    DOCS [read(perms, ops)]
}
