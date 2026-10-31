//! `maki.api`'s slash commands: registering one and running one by name.

use std::sync::Arc;

use flume::Sender;
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, Result as LuaResult, Table, Value as LuaValue};

use crate::api::util::command::{
    CommandEntry, CommandHandlerMap, LuaCommandWriter, UiAction, publish_command_snapshot,
    ui_roundtrip,
};
use crate::api::util::pair::{Pair, try_pair};
use crate::plugin_permissions::PluginReach;
use crate::runtime::Lineage;

const NARGS_ERR: &str = r#"register_command: 'nargs' must be 0, 1, "?", "*", or "+""#;

/// Register a slash-command that appears in the user input bar.
///
/// Slash commands let the user trigger plugin actions by typing `/name` in the
/// input. Use them for interactive workflows that do not need the model, like
/// browsing memory files or toggling settings.
///
/// @param spec table Command specification:
///   name        (string)   Required. The command name (e.g. "/hello"; a leading
///                            slash is added when missing).
///   description (string)   Optional. Short description shown in the command palette.
///   nargs       (integer|string) Optional. How many arguments the command
///                          takes, spelled like nvim's nargs: 0 (default),
///                          1, "?" (zero or one), "*" (any number), or "+"
///                          (one or more). An argument is a whitespace
///                          separated word. Type more than allowed and the
///                          command quietly stops matching: the input goes
///                          to the model as a normal message. Only the upper
///                          bound is checked, so with "+" you still need to
///                          handle an empty `opts.args` yourself.
///   handler     (function) Required. Called when the user runs the command,
///                          with one opts table: `opts.args` is the raw
///                          argument string (whitespace kept, may be empty)
///                          and `opts.fargs` is the same split into words.
/// @return
/// @example
/// maki.api.register_command({
///   name = "/hello",
///   description = "Say hello",
///   handler = function()
///     maki.ui.flash("Hello from my plugin!")
///   end,
/// })
#[lua_fn]
fn register_command(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    #[ctx] reach: PluginReach,
    spec: Table,
) -> LuaResult<()> {
    register_command_from_lua(lua, &spec, plugin, reach)
}

/// Runs a slash command by name, exactly as typing it in the input would.
/// Works for built-ins, custom `/project:` and `/user:` commands, MCP
/// prompts, and commands other plugins registered.
///
/// Use it to alias a command you like under a name you prefer, instead of
/// reimplementing what it does. See `maki.ui.action` for the same idea
/// applied to keybound UI actions.
///
/// Pass the whole command line, arguments included: `"/cd ~/src"`. The
/// leading slash is optional. Names match exactly apart from case, so a typo
/// reports an error instead of running the closest command, and a cycle of
/// aliases stops with one too.
///
/// This returns as soon as the command has been dispatched, not when it
/// finishes, so aliasing something long-running like `/compact` does not
/// block your handler.
///
/// A command outside the calling plugin's grants returns an error, and a few
/// only run when you type them. [Commands](/docs/commands/#plugin-access)
/// lists what each one needs.
///
/// @param cmdline string Command line, e.g. `"/new"` or `"/cd ~/src"`.
/// @return (boolean|nil, string|nil) `true` once dispatched, or nil and an error message for an unknown or denied command.
/// @example
/// -- /resume as an alias for the built-in session picker:
/// maki.api.register_command({
///   name = "/resume",
///   description = "Alias for /sessions",
///   handler = function()
///     local ok, err = maki.api.run_command("/sessions")
///     if not ok then
///       maki.ui.flash("could not run /sessions: " .. err)
///     end
///   end,
/// })
#[lua_fn]
async fn run_command(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    #[ctx] caller: PluginReach,
    cmdline: String,
) -> LuaResult<Pair<bool>> {
    let depth = Lineage::current(&lua).command_depth.saturating_add(1);
    let reply = try_pair!(
        ui_roundtrip(tx.as_ref(), |reply_tx| UiAction::RunCommand {
            cmdline,
            depth,
            caller: caller.now(&lua),
            reply_tx,
        })
        .await
    );
    try_pair!(reply);
    Ok((Some(true), None))
}

lua_table! {
    extend "maki.api" => fn add_command_fns(plugin: Arc<str>, reach: PluginReach), COMMAND_DOCS [
        register_command(plugin, reach),
    ]
}

lua_table! {
    extend "maki.api" => fn add_run_command_fns(ui_action_tx: Option<Sender<UiAction>>, reach: PluginReach), RUN_COMMAND_DOCS [
        run_command(ui_action_tx, reach),
    ]
}

/// The UI's part of `maki.api`, documented by [`COMMAND_DOCS`] and
/// [`RUN_COMMAND_DOCS`].
pub(crate) fn add_ui_fns(
    t: &Table,
    lua: &Lua,
    ui_action_tx: Option<Sender<UiAction>>,
    reach: PluginReach,
    plugin: Arc<str>,
) -> LuaResult<()> {
    add_command_fns(t, lua, plugin, reach.clone())?;
    add_run_command_fns(t, lua, ui_action_tx, reach)
}

/// Matching only needs an upper bound, so "+" and "*" both become MAX;
/// minimums ("+" vs "*") are left for handlers to enforce.
fn parse_nargs(spec: &Table) -> LuaResult<usize> {
    match spec.get::<LuaValue>("nargs")? {
        LuaValue::Nil | LuaValue::Integer(0) | LuaValue::Number(0.0) => Ok(0),
        LuaValue::Integer(1) | LuaValue::Number(1.0) => Ok(1),
        LuaValue::String(s) => match s.to_string_lossy().as_ref() {
            "?" => Ok(1),
            "*" | "+" => Ok(usize::MAX),
            _ => Err(mlua::Error::runtime(NARGS_ERR)),
        },
        _ => Err(mlua::Error::runtime(NARGS_ERR)),
    }
}

fn register_command_from_lua(
    lua: &Lua,
    spec: &Table,
    plugin: Arc<str>,
    reach: PluginReach,
) -> LuaResult<()> {
    let mut name: String = spec
        .get("name")
        .map_err(|_| mlua::Error::runtime("register_command: missing 'name'"))?;
    if name.is_empty() {
        return Err(mlua::Error::runtime(
            "register_command: name must be non-empty",
        ));
    }
    if !name.starts_with('/') {
        name.insert(0, '/');
    }
    let description: String = spec.get("description").unwrap_or_default();
    let max_args = parse_nargs(spec)?;
    let handler: Function = spec
        .get("handler")
        .map_err(|_| mlua::Error::runtime("register_command: missing 'handler'"))?;

    let handler_key = lua.create_registry_value(handler)?;
    let name: Arc<str> = Arc::from(name.as_str());
    let description: Arc<str> = Arc::from(description.as_str());

    {
        let mut map = lua
            .app_data_mut::<CommandHandlerMap>()
            .ok_or_else(|| mlua::Error::runtime("register_command: not initialized"))?;
        map.entry(Arc::clone(&plugin)).or_default().insert(
            Arc::clone(&name),
            CommandEntry {
                handler: handler_key,
                description,
                reach,
                max_args,
            },
        );
    }

    let map = lua
        .app_data_ref::<CommandHandlerMap>()
        .ok_or_else(|| mlua::Error::runtime("register_command: not initialized"))?;
    let writer = lua
        .app_data_ref::<LuaCommandWriter>()
        .ok_or_else(|| mlua::Error::runtime("register_command: not initialized"))?;
    publish_command_snapshot(&map, &writer);

    Ok(())
}
