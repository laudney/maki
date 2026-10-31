use maki_agent::SessionMailbox;
use mlua::{Lua, LuaSerdeExt, Result as LuaResult, Table, Value};

use crate::api::util::ctx::LuaCtx;
use crate::background::BackgroundTasks;
use crate::runtime::CANCEL_ABANDON_AFTER;

pub(crate) fn extend_agent(lua: &Lua, task: &Table) -> LuaResult<()> {
    let original: mlua::Function = task.get("stop")?;
    task.set(
        "stop",
        lua.create_async_function(move |lua, (id, opts): (Value, Value)| {
            let original = original.clone();
            async move {
                if let Value::UserData(ctx) = &id {
                    let ctx = ctx.borrow::<LuaCtx>()?;
                    let agent = ctx
                        .agent()
                        .ok_or_else(|| mlua::Error::runtime(ctx.cap_err("maki.task.stop")))?;
                    let asker = agent.mailbox.as_ref().map(SessionMailbox::session_id);
                    let Value::String(task_id) = opts else {
                        return Err(mlua::Error::runtime("stop(ctx, id): id must be a string"));
                    };
                    let task_id = task_id.to_string_lossy();
                    drop(ctx);
                    return match BackgroundTasks::of(&lua)
                        .stop_by_model(asker, &task_id, CANCEL_ABANDON_AFTER)
                        .await
                    {
                        Ok(stopped) => Ok((lua.to_value(&stopped)?, None)),
                        Err(error) => Ok((Value::Nil, Some(error))),
                    };
                }
                original.call_async((id, opts)).await
            }
        })?,
    )
}

#[cfg(test)]
mod tests {
    use crate::api::test_support::{eval, lua as lua_with_task, replying_ui};
    use maki_lua_core::api::task::USER_ACTION_ERR;
    use maki_lua_core::facade::{NO_UI_ERR, TaskRequest};
    use maki_lua_ui::UiAction;
    use mlua::{Lua, Value};
    use serde_json::json;
    use test_case::test_case;

    use crate::runtime::{TaskCell, TaskScope, run_command_scoped};
    use maki_agent::cancel::CancelToken;

    const TASK_ID: &str = "toolu_01";
    const DONE_ID: &str = "toolu_02";
    const ERROR_ID: &str = "toolu_03";
    const MAIN_ID: &str = "main";
    const MAIN_NAME: &str = "chat";
    const SUBAGENT_NAME: &str = "explore repo";
    const WORKING_STATUS: &str = "working";
    const DONE_STATUS: &str = "done";
    const ERROR_STATUS: &str = "error";
    const NIL_STATUS: &str = "nil";
    const CHAT_SESSION: &str = "chat-session";
    const UNKNOWN_TASK_ERR: &str = "unknown task: toolu_01";
    const LIST_CALL: &str = "return task.list()";
    const STOP_CALL: &str = "return task.stop('toolu_01', {session='chat-session'})";
    const LIST_SCRIPT: &str = "
        local tasks = task.list()
        local ids, statuses = {}, {}
        for i, t in ipairs(tasks) do
            ids[i] = t.id
            statuses[i] = tostring(t.status)
        end
        return table.concat(ids, ','), table.concat(statuses, ','), tasks[1].name
    ";

    #[test_case(LIST_CALL ; "list")]
    fn without_ui_returns_error_pair(code: &str) {
        let lua = lua_with_task(None);
        let (val, err): (Value, Option<String>) = eval(&lua, code);
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    /// A stale id should be visible but not fatal: the host's `Err` comes back
    /// as the `(nil, err)` pair, word for word, without raising into the plugin.
    #[test_case(LIST_CALL ; "list")]
    fn host_error_reply_surfaces_as_error_pair(code: &str) {
        let (lua, _requests) = replying_ui(Err(UNKNOWN_TASK_ERR.to_owned()));

        let (val, err): (Value, Option<String>) = eval(&lua, code);
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(UNKNOWN_TASK_ERR));
    }

    /// `plugins/task/picker_rows.lua` spots the main chat with `if not task.status`,
    /// so a missing key has to stay missing on the Lua side.
    #[test]
    fn list_passes_host_array_through_unchanged() {
        let (lua, requests) = replying_ui(Ok(json!([
            { "id": MAIN_ID, "name": MAIN_NAME, "focused": true },
            { "id": TASK_ID, "name": SUBAGENT_NAME, "status": WORKING_STATUS, "focused": false },
            { "id": DONE_ID, "name": SUBAGENT_NAME, "status": DONE_STATUS, "focused": false },
            { "id": ERROR_ID, "name": SUBAGENT_NAME, "status": ERROR_STATUS, "focused": false },
        ])));

        let (ids, statuses, main_name): (String, String, String) = eval(&lua, LIST_SCRIPT);
        assert_eq!(ids, format!("{MAIN_ID},{TASK_ID},{DONE_ID},{ERROR_ID}"));
        assert_eq!(
            statuses,
            format!("{NIL_STATUS},{WORKING_STATUS},{DONE_STATUS},{ERROR_STATUS}")
        );
        assert_eq!(main_name, MAIN_NAME);

        assert!(matches!(
            requests.try_recv().unwrap(),
            UiAction::Task {
                req: TaskRequest::List,
                ..
            }
        ));
    }

    #[test]
    fn stop_from_a_user_command_roundtrips_through_ui_channel() {
        let (lua, requests) = replying_ui(Ok(json!(true)));

        let (val, err): (bool, Option<String>) = crate::test_support::runtime()
            .block_on(run_command_scoped(
                &lua,
                0,
                lua.load(STOP_CALL).eval_async(),
            ))
            .unwrap();
        assert!(val);
        assert_eq!(err, None);

        assert!(matches!(
            requests.try_recv().unwrap(),
            UiAction::Task { req: TaskRequest::Stop { id, session }, .. } if id == TASK_ID && session == CHAT_SESSION
        ));
    }

    enum Caller {
        Hook,
        CommandAlias,
        ChildOfUserCommand,
    }

    #[test_case(Caller::Hook ; "hook_outside_any_command")]
    #[test_case(Caller::CommandAlias ; "command_run_by_run_command")]
    #[test_case(Caller::ChildOfUserCommand ; "task_nested_in_a_user_command")]
    fn stop_without_user_authority_never_reaches_the_host(caller: Caller) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_task(Some(tx));
        let stop = lua.load(STOP_CALL).eval_async();
        let (val, err): (Value, Option<String>) = crate::test_support::runtime()
            .block_on(async {
                match caller {
                    Caller::Hook => stop.await,
                    Caller::CommandAlias => run_command_scoped(&lua, 1, stop).await,
                    Caller::ChildOfUserCommand => {
                        run_command_scoped(&lua, 0, async {
                            let child =
                                TaskScope::new(&lua, TaskCell::new(CancelToken::none(), None));
                            child.scope_future(stop).await
                        })
                        .await
                    }
                }
            })
            .unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(USER_ACTION_ERR));
        assert!(rx.is_empty());
    }

    /// A refusal means the rows on screen are stale, so the picker flashes it
    /// and lists again either way.
    #[test_case(None ; "stopped")]
    #[test_case(Some(UNKNOWN_TASK_ERR) ; "refused")]
    fn tasks_picker_stops_the_selected_row_and_refreshes(stop_error: Option<&str>) {
        let lua = picker_lua(stop_error);
        lua.load("open_tasks()").exec().unwrap();

        let globals = lua.globals();
        assert_eq!(globals.get::<String>("stopped_id").unwrap(), TASK_ID);
        assert_eq!(
            globals.get::<String>("stopped_session").unwrap(),
            CHAT_SESSION
        );
        assert_eq!(globals.get::<u32>("list_calls").unwrap(), 2);
        assert_eq!(
            globals.get::<Option<String>>("flashed").unwrap().as_deref(),
            stop_error
        );
    }

    /// Runs `plugins/task/picker.lua` against a fake `maki` that lists the main
    /// chat and one task, then presses Down, Ctrl+S and Esc.
    fn picker_lua(stop_error: Option<&str>) -> Lua {
        let lua = Lua::new();
        let globals = lua.globals();
        globals.set("task_id", TASK_ID).unwrap();
        globals.set("chat_session", CHAT_SESSION).unwrap();
        globals.set("stop_error", stop_error).unwrap();
        lua.load(
            r#"
            local modules = {}
            function require(name) return assert(modules[name], name) end
            function install_module(name, source) modules[name] = assert(loadstring(source))() end
            local keys = {'<Down>', '<C-s>', '<Esc>'}
            local tasks = {
                {id = 'main', session = chat_session, name = 'chat', focused = true},
                {id = task_id, session = chat_session, name = 'work', status = 'working'},
            }
            list_calls = 0
            local win = {width = 80, height = 20}
            function win:set_config(_) end
            function win:set_cursor(_) end
            function win:close() end
            function win:recv(_)
                return {type = 'key', key = assert(table.remove(keys, 1), 'picker did not close')}
            end
            maki = {
                task = {
                    list = function() list_calls = list_calls + 1; return tasks end,
                    focus = function() return true end,
                    stop = function(id, opts)
                        stopped_id, stopped_session = id, opts.session
                        if stop_error then return nil, stop_error end
                        return true
                    end,
                },
                ui = {
                    buf = function() return {set_lines = function() end} end,
                    open_win = function() return win end,
                    display_width = function(s) return #s end,
                    flash = function(s) flashed = s end,
                },
                api = {
                    create_autocmd = function() end,
                    register_command = function(opts) open_tasks = opts.handler end,
                },
                keymap = {set = function() end},
            }
        "#,
        )
        .exec()
        .unwrap();
        let install: mlua::Function = globals.get("install_module").unwrap();
        for (name, source) in [
            (
                "maki.text_input",
                include_str!("../../../plugins/lib/maki/text_input.lua"),
            ),
            (
                "maki.color",
                include_str!("../../../plugins/lib/maki/color.lua"),
            ),
            (
                "maki.list_picker",
                include_str!("../../../plugins/lib/maki/list_picker.lua"),
            ),
            (
                "picker_rows",
                include_str!("../../../plugins/task/picker_rows.lua"),
            ),
        ] {
            install.call::<()>((name, source)).unwrap();
        }
        lua.load(include_str!("../../../plugins/task/picker.lua"))
            .exec()
            .unwrap();
        lua
    }
}
