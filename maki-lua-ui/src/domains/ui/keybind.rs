//! Running a plugin's handler for a key the host consumed.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mlua::{Function, Lua, Table, Value as LuaValue};

use super::host_ui_send;
use crate::api::keymap::KeybindTicket;
use crate::api::util::command::UiAction;
use crate::api::util::pair::{Pair, err_pair};
use crate::plugin_permissions::Permission;
use crate::runtime::{GateGuard, InflightGate, run_command_scoped, strip_traceback};

/// How long a keystroke's handler holds a drain-barrier slot. Long enough for
/// a handler that makes a round-trip or two, short enough that a picker left
/// open does not hold up a `/reload`.
const KEYBIND_GATE_HOLD: Duration = Duration::from_secs(10);
pub const FALLBACK_SPENT_ERR: &str = "fallback already ran, or the key handler has returned";
const EV_INPUT: &str = "input";

/// Runs a keybind callback. The host consumed the key to get here, and the key
/// is spent whatever the handler does with it.
///
/// The return value is not read, and a handler that raises is logged and
/// nothing more. Replaying a key because a plugin has a bug is strictly worse
/// than dropping it: seconds later the UI is not the one the user pressed it
/// against, and a `<CR>` landing on the permission prompt that opened in the
/// meantime approves its default answer. Fall-through is the host's decision,
/// taken on the UI thread before any of this runs.
///
/// Every way out ends in [`report_keybind_done`], because the UI holds the keys
/// typed after this one until it hears back. It lets them go after a second
/// anyway, so a report lost to a dying host only costs that second.
///
/// `ev.fallback()` is spent once the handler returns. The UI would ignore a
/// late one anyway, because it only runs the fallback of the key it is still
/// holding for, but the plugin deserves to hear that it did nothing.
///
/// The handler and its drain-barrier slot are two tasks, because they live
/// for different times. A [`GateGuard`] goes with the keystroke, the way
/// `run_host_hook` takes one, so a `/reload` waiting on `drain_barrier`
/// cannot land in the middle of a handler. But a key that opens a float and
/// loops on `win:recv()` runs for as long as the user keeps it up, and a
/// picker left open would then hold up the barrier with it. So the guard goes
/// after [`KEYBIND_GATE_HOLD`] even if the handler is still running, which is
/// also why a command handler takes no guard at all.
pub(crate) fn spawn_keybind_callback(
    lua: &Lua,
    ex: &Rc<tokio::task::LocalSet>,
    gate: &Rc<InflightGate>,
    mut ticket: KeybindTicket,
) {
    let (id, key) = (ticket.id(), ticket.key());
    let plugin = Arc::clone(ticket.plugin());
    if !ticket.plugin_live() {
        tracing::warn!(key = %key.notation(), ticket = id, plugin = %plugin, "keybind key dropped: plugin unloaded");
        report_keybind_done(lua, id);
        return;
    }
    let spent = Arc::new(AtomicBool::new(false));
    let call = lua
        .registry_value::<Function>(ticket.callback())
        .and_then(|func| Ok((func, keybind_event(lua, &mut ticket, Arc::clone(&spent))?)));
    let (func, ev) = match call {
        Ok(call) => call,
        Err(e) => {
            tracing::warn!(key = %key.notation(), ticket = id, plugin = %plugin, error = %e, "keybind key dropped: callback unreachable");
            report_keybind_done(lua, id);
            return;
        }
    };
    // Dropped when the handler returns, which is how the task below learns it
    // may hand the slot back before the hold lapses.
    let (done_tx, done_rx) = flume::bounded::<()>(1);
    let handler_lua = lua.clone();
    let handler_plugin = Arc::clone(&plugin);
    ex.spawn_local(async move {
        let _done = done_tx;
        if let Err(e) = run_command_scoped(&handler_lua, 0, func.call_async::<LuaValue>(ev)).await {
            tracing::warn!(key = %key.notation(), ticket = id, plugin = %handler_plugin, error = %strip_traceback(&e), "keybind callback failed, key spent");
        }
        spent.store(true, Ordering::Release);
        report_keybind_done(&handler_lua, id);
    });
    let gate = Rc::clone(gate);
    ex.spawn_local(async move {
        let _guard = GateGuard::new(&gate);
        futures_lite::future::or(
            async {
                let _ = done_rx.recv_async().await;
            },
            async {
                tokio::time::sleep(KEYBIND_GATE_HOLD).await;
                tracing::warn!(key = %key.notation(), ticket = id, plugin = %plugin, "keybind handler outlived its drain-barrier slot");
            },
        )
        .await;
    });
}

/// The `ev` a keybind handler is called with.
///
/// `ev.input` is built on first read, since a handler bound to every
/// printable key mostly never reads it, and the text can be long. It and
/// `ev.text` are left out for a plugin that may not read the session.
///
/// `fallback` only asks. The UI runs it as the host's own handling of the key
/// it holds the later keys for, so it lands after every `maki.ui.*` call the
/// handler made before it and before any key typed after this one.
fn keybind_event(
    lua: &Lua,
    ticket: &mut KeybindTicket,
    spent: Arc<AtomicBool>,
) -> mlua::Result<Table> {
    let ev = lua.create_table()?;
    ev.set("key", ticket.key().notation())?;
    let reads_session = ticket.grants().now(lua).is_allowed(Permission::SessionRead);
    if reads_session && let Some(input) = ticket.take_input() {
        let read_input = lua.create_function(move |lua, (ev, key): (Table, LuaValue)| {
            if !key.as_string().is_some_and(|key| key == EV_INPUT) {
                return Ok(LuaValue::Nil);
            }
            let table = input.to_lua(lua)?;
            ev.raw_set(EV_INPUT, &table)?;
            Ok(LuaValue::Table(table))
        })?;
        let meta = lua.create_table()?;
        meta.set("__index", read_input)?;
        ev.set_metatable(Some(meta))?;
    }
    if reads_session && let Some(text) = ticket.text() {
        ev.set("text", text)?;
    }
    let id = ticket.id();
    let fallback = lua.create_function(move |lua, ()| -> mlua::Result<Pair<bool>> {
        if spent.swap(true, Ordering::AcqRel) {
            return Ok(err_pair(FALLBACK_SPENT_ERR));
        }
        host_ui_send(lua, UiAction::KeybindFallback(id));
        Ok((Some(true), None))
    })?;
    ev.set("fallback", fallback)?;
    Ok(ev)
}

/// The UI lets the keys typed after {ticket} go once this arrives, so it has
/// to come after everything the handler did. It does for free: it shares the
/// channel with every `maki.ui.*` call, `input_edit` waits for its answer,
/// and `set_mode` is in place the moment it returns.
fn report_keybind_done(lua: &Lua, ticket: u64) {
    host_ui_send(lua, UiAction::KeybindDone(ticket));
}
