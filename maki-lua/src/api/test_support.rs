use std::sync::Arc;

use flume::{Receiver, Sender};
use maki_lua_core::domain::LoadScope;
use maki_lua_core::facade::{self, FacadeOps, UiReply};
use maki_lua_core::plugin_permissions::PluginReach;
use maki_lua_ui::UiAction;
use maki_lua_ui::test_support::ui_ops;
use mlua::{FromLuaMulti, Lua, Table};

use crate::plugin_permissions::PluginPermissions;

const PLUGIN: &str = "test";
const EXPECTED_REQUEST: &str = "expected a facade request";

pub(super) fn lua(tx: Option<Sender<UiAction>>) -> Lua {
    granted(tx, PluginPermissions::trusted())
}

pub(super) fn granted(tx: Option<Sender<UiAction>>, grants: PluginPermissions) -> Lua {
    let lua = Lua::new();
    let ops = match ui_ops(tx) {
        Some(ui) => FacadeOps::Ui(Some(ui)),
        None => crate::domains::facade::facade_ops(),
    };
    let maki = lua.create_table().unwrap();
    let load = LoadScope {
        plugin: &Arc::from(PLUGIN),
        reach: &PluginReach::new(grants),
        grant: &(),
    };
    facade::extend(&lua, &maki, &load, &ops).unwrap();
    for entry in maki.pairs::<String, Table>() {
        let (name, table) = entry.unwrap();
        lua.globals().set(name, table).unwrap();
    }
    lua
}

pub(super) fn serving_ui(answer: impl Fn(UiAction) + Send + 'static) -> Lua {
    let (tx, rx) = flume::unbounded();
    std::thread::spawn(move || {
        for action in rx {
            answer(action);
        }
    });
    lua(Some(tx))
}

pub(super) fn replying_ui(reply: UiReply) -> (Lua, Receiver<UiAction>) {
    let (tx, requests) = flume::unbounded();
    let lua = serving_ui(move |action| {
        let reply_tx = match &action {
            UiAction::Model { reply_tx, .. }
            | UiAction::Session { reply_tx, .. }
            | UiAction::Task { reply_tx, .. }
            | UiAction::Plan { reply_tx, .. } => reply_tx.clone(),
            _ => panic!("{EXPECTED_REQUEST}"),
        };
        tx.send(action).unwrap();
        reply_tx.send(reply.clone()).unwrap();
    });
    (lua, requests)
}

pub(super) fn eval<T: FromLuaMulti>(lua: &Lua, code: &str) -> T {
    crate::test_support::runtime()
        .block_on(lua.load(code).eval_async())
        .unwrap()
}
