pub mod r#async;
pub mod autocmd;
pub mod base64;
pub mod env;
pub mod r#fn;
pub mod fs;
pub mod image;
pub mod json;
pub mod log;
pub mod model;
pub mod net;
pub mod options;
pub mod pack;
pub mod plan;
pub mod session;
pub mod slot;
pub mod split;
pub mod task;
pub mod text;
pub mod top;
pub mod treesitter;
pub mod util;
pub mod uv;
pub mod yaml;

use std::any::Any;
use std::collections::HashSet;
use std::sync::Arc;

use mlua::{
    AnyUserData, Function, Lua, MetaMethod, MultiValue, Result as LuaResult, Table, UserData,
    UserDataMethods, Value,
};

use crate::api::options::PluginOpts;
use crate::api::util::convert::TableExt;
use crate::domain::{Domain, LoadScope};
use crate::plugin_permissions::{Permission, PluginReach, warn_invalid_net_hosts};

/// The namespaces the UI domain fills and the names it installs on each,
/// which a composition without it guards. `keymap` is the UI's alone, so it
/// is created empty for the guard. A test in `maki-lua` keeps the names equal
/// to what the UI domain registers.
pub const UI_NAMES: [(&str, &[&str]); 4] = [
    (
        "ui",
        &[
            "action",
            "flash",
            "has_renderer",
            "input",
            "input_decorate",
            "input_edit",
            "input_rules",
            "input_submit",
            "open_editor",
            "open_win",
            "register_renderer",
            "renderer",
            "set_mode",
            "set_status_hint",
            "set_window_title",
        ],
    ),
    (
        "api",
        &["register_action", "register_command", "run_command"],
    ),
    ("fn", &["mode", "winrestview", "winsaveview"]),
    ("keymap", &["del", "normalize", "set"]),
];
/// UI names whose stub in a config load returns its first argument, because
/// config code keys tables by what they return, and one shared value would
/// fold every key into one.
const ECHOED_UI_NAMES: [(&str, &str); 1] = [("keymap", "normalize")];
/// What a guarded namespace raises for a name it lacks, after the name.
pub const NOT_IN_AGENT_RUNTIME: &str =
    "is not available in the agent runtime: move UI code to the plugin's ui.lua";
/// What a package's guarded namespace raises for a name it lacks, after the
/// name: a package's UI code belongs in its own `ui/` entry.
pub const PACKAGE_NOT_IN_AGENT_RUNTIME: &str =
    "is not available in the agent runtime: move UI code to the package's ui/ entry";
/// The namespaces the agent domain alone fills, which a composition without
/// it creates empty and guards whole.
pub const AGENT_NAMESPACES: [&str; 3] = ["agent", "interpreter", "provider"];
/// The names the agent domain installs on the shared `maki.api`, which a
/// composition without it guards. A test in `maki-lua` keeps the names equal
/// to what the agent domain registers.
pub const AGENT_API_NAMES: [&str; 6] = [
    "get_tool",
    "get_tools",
    "register_permission_rule",
    "register_prompt_hint",
    "register_tool",
    "set_prompt",
];
/// What a guarded agent name raises, after the name.
pub const NOT_IN_UI_RUNTIME: &str =
    "is not available in the UI runtime: move agent code to the plugin's init.lua";
/// What a config load logs for a UI name it reaches, after the name and its
/// owner.
const IGNORED_IN_AGENT_RUNTIME: &str = "does nothing in the agent runtime";

/// The code a `maki` global is built for, which decides what a composition
/// without the UI domain does with a UI name it lacks.
#[derive(Clone, Copy)]
pub enum LoadKind {
    /// A bundled or single-file plugin: the name raises, naming its
    /// `ui.lua`.
    Plugin,
    /// A package: the name raises, naming its `ui/` entry.
    Package,
    /// The user's `init.lua`: a UI name is the inert [`Ignored`] value,
    /// because the docs tell users to bind keys there and one config serves
    /// every composition. Any other name raises as for a plugin.
    Config,
}

/// What a config load gets for a UI name: indexing or calling it gives
/// itself, and it reads as "" as a string, so code that keeps a handle,
/// a key or an action from the UI API runs on without a UI.
struct Ignored;

impl UserData for Ignored {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_function(MetaMethod::Index, |_, (this, _): (AnyUserData, Value)| {
            Ok(this)
        });
        methods.add_meta_function(
            MetaMethod::Call,
            |_, (this, _): (AnyUserData, MultiValue)| Ok(this),
        );
        methods.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok(String::new()));
        methods.add_meta_function(MetaMethod::Concat, |_, (left, right): (Value, Value)| {
            Ok(ignored_as_empty(&left)? + &ignored_as_empty(&right)?)
        });
    }
}

fn ignored_as_empty(value: &Value) -> LuaResult<String> {
    match value {
        Value::UserData(data) if data.is::<Ignored>() => Ok(String::new()),
        other => other.to_string(),
    }
}

/// The UI names a config load already logged, so each logs once.
struct IgnoredUiNames(HashSet<(Arc<str>, String)>);

/// The error a guarded `maki.{namespace}` raises for {name}.
pub fn not_in_agent_runtime(namespace: &str, name: &str) -> String {
    format!("maki.{namespace}.{name} {NOT_IN_AGENT_RUNTIME}")
}

/// The error a package's guarded `maki.{namespace}` raises for {name}.
pub fn package_not_in_agent_runtime(namespace: &str, name: &str) -> String {
    format!("maki.{namespace}.{name} {PACKAGE_NOT_IN_AGENT_RUNTIME}")
}

/// The error a guarded agent name `maki.{namespace}.{name}` raises.
pub fn not_in_ui_runtime(namespace: &str, name: &str) -> String {
    format!("maki.{namespace}.{name} {NOT_IN_UI_RUNTIME}")
}

pub fn create_maki_global(
    lua: &Lua,
    domain: &dyn Domain,
    plugin: Arc<str>,
    reach: &PluginReach,
    opts: PluginOpts,
    grant: &dyn Any,
    kind: LoadKind,
) -> LuaResult<Table> {
    let maki = lua.create_table()?;
    let reach = &reach.owned_by(Arc::clone(&plugin));
    let permissions = reach.permissions();

    let api = lua.create_table()?;
    options::add_options_fns(&api, lua, Arc::clone(&plugin), opts)?;
    autocmd::add_autocmd_methods(
        &api,
        lua,
        Arc::clone(&plugin),
        permissions.is_allowed(Permission::SessionRead),
    )?;
    slot::add_slot_methods(&api, lua, Arc::clone(&plugin), permissions.clone())?;
    maki.set("api", api)?;
    maki.set(
        "fs",
        fs::create_fs_table(lua, permissions, Arc::clone(&plugin))?,
    )?;
    maki.set("env", env::create_env_table(lua, permissions)?)?;
    maki.set("log", log::create_log_table(lua, Arc::clone(&plugin))?)?;
    maki.set("treesitter", treesitter::create_treesitter_table(lua)?)?;
    maki.set("uv", uv::create_uv_table(lua, permissions)?)?;
    maki.set("base64", base64::create_base64_table(lua)?)?;
    maki.set("image", image::create_image_table(lua)?)?;
    maki.set("json", json::create_json_table(lua)?)?;
    maki.set("yaml", yaml::create_yaml_table(lua)?)?;
    warn_invalid_net_hosts(&plugin, reach.egress().declared());
    maki.set(
        "net",
        net::create_net_table(
            lua,
            permissions,
            reach.egress().clone(),
            Arc::clone(&plugin),
        )?,
    )?;
    maki.set("text", text::create_text_table(lua)?)?;
    maki.set(
        "fn",
        r#fn::create_fn_table(lua, Arc::clone(&plugin), permissions, permissions.clone())?,
    )?;
    split::split__register(&maki, lua)?;
    top::add_top_methods(&maki, lua, Arc::clone(&plugin))?;
    maki.set("NIL", Value::NULL)?;
    maki.set(
        "async",
        r#async::create_async_table(lua, Arc::clone(&plugin))?,
    )?;
    pack::add_packadd(lua, &maki)?;
    maki.set("pack", pack::create_pack_read_table(lua)?)?;

    let load = LoadScope {
        plugin: &plugin,
        reach,
        grant,
    };
    domain.extend(lua, &maki, &load)?;
    if domain.component() != crate::plugin_permissions::Component::Ui {
        guard_ui_namespaces(lua, &maki, Arc::clone(&plugin), kind)?;
    } else {
        guard_agent_names(lua, &maki)?;
    }

    // `notify` sits on the metatable's `__index` rather than on the table
    // itself, because Lua only fires `__newindex` for keys missing from the
    // raw table. That is what gives `maki.notify = fn` somewhere to be caught
    // and routed into the one shared slot, instead of quietly shadowing notify
    // for the assigning plugin alone.
    let index = lua.create_table()?;
    top::notify__register(&index, lua, Arc::clone(&plugin))?;
    let notify_router = lua.create_function(
        move |lua, (t, k, v): (Table, String, Value)| match k.as_str() {
            "notify" => top::install_notify_handler(lua, Arc::clone(&plugin), v),
            _ => t.raw_set(k, v),
        },
    )?;
    let meta = lua.create_table()?;
    meta.set("__index", index)?;
    meta.set("__newindex", notify_router)?;
    maki.set_metatable(Some(meta))?;

    Ok(maki)
}

/// Makes a name missing from {maki}'s UI namespaces raise, so an agent entry
/// that calls the UI fails at the call, with the migration named. A config
/// load gets the inert value for a UI name instead, logged once per name.
fn guard_ui_namespaces(lua: &Lua, maki: &Table, plugin: Arc<str>, kind: LoadKind) -> LuaResult<()> {
    // A real function, so a config can also pass a UI name where a function
    // is required, as `maki.notify = maki.ui.flash` does.
    let ignored = lua.create_userdata(Ignored)?;
    let stub = lua.create_function(move |_, _: MultiValue| Ok(ignored.clone()))?;
    let echo = lua.create_function(|_, first: Value| Ok(first))?;
    for (namespace, ui_names) in UI_NAMES {
        let table = match maki.opt::<Table>(namespace)? {
            Some(table) => table,
            None => {
                let table = lua.create_table()?;
                maki.raw_set(namespace, &table)?;
                table
            }
        };
        let plugin = Arc::clone(&plugin);
        let stub = stub.clone();
        let echo = echo.clone();
        let missing = lua.create_function(move |lua, (_, key): (Table, Value)| {
            let Value::String(name) = key else {
                return Ok(Value::Nil);
            };
            let name = name.to_string_lossy();
            match kind {
                LoadKind::Config if ui_names.contains(&&*name) => {
                    log_ignored_once(lua, &plugin, format!("maki.{namespace}.{name}"));
                    let function = if ECHOED_UI_NAMES.contains(&(namespace, &*name)) {
                        &echo
                    } else {
                        &stub
                    };
                    Ok(Value::Function(function.clone()))
                }
                LoadKind::Package => Err(mlua::Error::runtime(package_not_in_agent_runtime(
                    namespace, &name,
                ))),
                _ => Err(mlua::Error::runtime(not_in_agent_runtime(namespace, &name))),
            }
        })?;
        set_index(lua, &table, missing)?;
    }
    Ok(())
}

/// Makes {maki}'s agent names raise, so a UI entry that calls the agent fails
/// at the call, with the migration named. Any other name on `maki.api` goes
/// on to the UI guard's `__index`, when the composition has that guard too.
fn guard_agent_names(lua: &Lua, maki: &Table) -> LuaResult<()> {
    for namespace in AGENT_NAMESPACES {
        let table = lua.create_table()?;
        let missing = lua.create_function(move |_, (_, key): (Table, Value)| match key {
            Value::String(name) => Err(mlua::Error::runtime(not_in_ui_runtime(
                namespace,
                &name.to_string_lossy(),
            ))),
            _ => Ok(Value::Nil),
        })?;
        set_index(lua, &table, missing)?;
        maki.raw_set(namespace, table)?;
    }
    let api: Table = maki.get("api")?;
    let inner = api
        .metatable()
        .and_then(|meta| meta.get::<Function>("__index").ok());
    let missing = lua.create_function(move |_, (table, key): (Table, Value)| {
        if let Value::String(name) = &key {
            let name = name.to_string_lossy();
            if AGENT_API_NAMES.contains(&&*name) {
                return Err(mlua::Error::runtime(not_in_ui_runtime("api", &name)));
            }
        }
        match &inner {
            Some(inner) => inner.call::<Value>((table, key)),
            None => Ok(Value::Nil),
        }
    })?;
    set_index(lua, &api, missing)
}

fn set_index(lua: &Lua, table: &Table, index: Function) -> LuaResult<()> {
    let meta = lua.create_table()?;
    meta.set("__index", index)?;
    table.set_metatable(Some(meta))
}

fn log_ignored_once(lua: &Lua, plugin: &Arc<str>, qualified: String) {
    let key = (Arc::clone(plugin), qualified.clone());
    let first = match lua.app_data_mut::<IgnoredUiNames>() {
        Some(mut ignored) => ignored.0.insert(key),
        None => {
            lua.set_app_data(IgnoredUiNames(HashSet::from([key])));
            true
        }
    };
    if first {
        tracing::info!(
            plugin = &**plugin,
            name = qualified,
            "{qualified} in {plugin} {IGNORED_IN_AGENT_RUNTIME}"
        );
    }
}
