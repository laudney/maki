//! `maki.task`. The host keeps the subagent transcripts, so a plugin only needs
//! to list the tasks and show one.

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Table, Value};

use crate::api::util::pair::Pair;
use crate::facade::{FacadeOps, FacadeRequest, TaskRequest};
use crate::plugin_permissions::PluginPermissions;
use crate::runtime::is_user_action;

pub const USER_ACTION_ERR: &str = "stopping a task requires a user command or keybind";

async fn roundtrip(lua: Lua, ops: FacadeOps, req: TaskRequest) -> LuaResult<Pair<Value>> {
    ops.ask_json(&lua, FacadeRequest::Task(req)).await
}

/// Lists the focused session's chats in chat order. Entry 1 is always the main
/// chat, with id `"main"` and no `status`: its work is the session's own, and
/// `maki.session.live()` already reports that. The rest are subagents: one a
/// tool spawned has its tool call's id, one a plugin opened with
/// [maki.agent.session](#maki-agent-session) has its session id and a
/// `plugin` field.
///
/// @return (table|nil, string|nil) Array of `{id, session, name, focused, status?, plugin?,
///   parent?, part?, started_at?, ended_at?, cost?}` where `status` is `"working"`,
///   `"idle"` (a plugin task between prompts), `"done"`, `"error"`, or
///   `"cancelled"` (stopped by the user or the model), and `session` is the
///   chat session that `stop()` asks for. `parent` is the id of the call the
///   task runs under, like a `batch` that started it. That call is listed
///   only once it moved. `part` is true for a piece listed with
///   `ctx:set_parts()`. Stopping a part stops its whole call, while a
///   subagent the call started stops alone. `started_at` and `ended_at` are
///   Unix seconds: when the task started, or moved to the background, and
///   when it finished. A task loaded from disk has neither. `cost` is what a
///   subagent billed so far, in dollars. The main chat has none of the
///   three. Or nil and an error.
/// @example
/// for _, t in ipairs(maki.task.list() or {}) do
///   print(t.name, t.status or "main")
/// end
#[lua_fn]
async fn list(lua: Lua, #[ctx] ops: FacadeOps) -> LuaResult<Pair<Value>> {
    roundtrip(lua, ops, TaskRequest::List).await
}

/// Shows a task's transcript, the way the chat cycling keys do. An id from
/// another session returns an error instead of landing on the wrong task.
///
/// @param id string Task id, as returned by `list()`. `"main"` is the main chat.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// local _, err = maki.task.focus("main")
#[lua_fn]
async fn focus(lua: Lua, #[ctx] ops: FacadeOps, id: String) -> LuaResult<Pair<Value>> {
    roundtrip(lua, ops, TaskRequest::Focus { id }).await
}

/// Deletes a finished task and its saved transcript. Refuses the main chat and
/// running tasks. Deleting the focused task moves focus to the chat before it.
///
/// @param id string Task id, as returned by `list()`.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// local _, err = maki.task.remove("toolu_01")
#[lua_fn(guard = SessionWrite)]
async fn remove(lua: Lua, #[ctx] ops: FacadeOps, id: String) -> LuaResult<Pair<Value>> {
    roundtrip(lua, ops, TaskRequest::Remove { id }).await
}

/// Stops a task in the focused session, the same way Esc in its tab does. A
/// plugin task can be stopped while idle, even before its first prompt. The
/// main chat, a finished task, or an unknown id returns an error.
///
/// Only the user may stop a task, so this works in a command the user typed
/// or a key they pressed. Tools, hooks, autocmds, `maki.async` tasks, and
/// commands run through `maki.api.run_command` get an error instead, or any
/// plugin could stop work that is not its own.
///
/// From a tool handler, `stop(ctx, id)` is the model's stop. It stops a
/// background task (a command or subagent) that the calling agent moved,
/// which is allowed because the tool call belongs to a turn the user started.
/// It waits up to 5 seconds for the final reply and returns it, and the agent
/// gets no notice of the end. A late reply returns no `output`, and the
/// result reaches the agent later as a quiet notice. A task another agent
/// moved, a finished task or an unknown id returns an error that names the
/// agent's running task ids.
///
/// @param id string|LuaCtx Task id, as returned by `list()`. Or the handler's
///   ctx, for the model's stop.
/// @param opts table|string `session` (string), the chat session of the
///   `list()` row. If the user switched sessions since, this returns an error
///   rather than stopping a task with the same id in the other session. With a
///   ctx: the background task id.
/// @return (boolean|table|nil, string|nil) true on success, or nil and an
///   error. With a ctx, success is `{name, output?}`. `name` is the task's
///   name in `/tasks`, and `output` is the final reply, missing when it was
///   late.
/// @example
/// local _, err = maki.task.stop(task.id, { session = task.session })
/// -- in a tool handler
/// local stopped, err = maki.task.stop(ctx, input.id)
#[lua_fn]
async fn stop(lua: Lua, #[ctx] ops: FacadeOps, id: String, opts: Table) -> LuaResult<Pair<Value>> {
    if !is_user_action(&lua) {
        return Ok((None, Some(USER_ACTION_ERR.to_owned())));
    }
    let session = opts.get::<String>("session")?;
    roundtrip(lua, ops, TaskRequest::Stop { id, session }).await
}

lua_table! {
    /// The subagents of the focused session and their transcripts. Tasks are
    /// spawned by the `task` tool and addressed by an id that survives a reload.
    /// Without an interactive UI every function returns
    /// `nil, "no interactive UI attached"`. The agent-only `stop(ctx, id)`
    /// form is also available without a UI.
    "maki.task" => pub fn create_task_table(
        ops: FacadeOps,
        perms: &PluginPermissions,
    ),
    DOCS [list(ops), focus(ops), remove(perms, ops), stop(ops)]
}
