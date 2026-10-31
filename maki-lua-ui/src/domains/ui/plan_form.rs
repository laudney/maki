//! The `ui.plan_form*` chains a landed plan fires: whether the built-in form
//! opens, and which rows it draws.

use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use maki_types::cancel::CancelToken;
use mlua::{Lua, MultiValue, Value as LuaValue};

use crate::api::slot::{
    ChainObserver, LayerCost, PLAN_FORM_ACTIONS_SLOT, PLAN_FORM_SLOT, layer_plugins,
    run_host_chain, run_host_chain_with,
};
use crate::api::util::command::{PlanFormRow, PlanMenu};
use crate::runtime::{
    InflightGate, PluginMap, TaskCell, layer_delegation, run_awaited, strip_traceback,
};

use super::plan_rows::{RowOwners, install_row_handlers, rows_from_table, rows_to_table};

/// Wall clock the `ui.plan_form*` chains get, together, before the host stops
/// waiting and opens the built-in form, generous because a layer may shell out
/// or fetch before it decides. It rules out a parked layer stranding the user
/// with a finished plan and no surface for it.
///
/// One budget covers both chains a draft fires, so this is also the whole
/// wait the user can see and the number the UI's own fallback is derived
/// from. Two budgets would add up to a host worst case no reader of either
/// constant could predict.
pub const PLAN_FORM_SLOT_DEADLINE: Duration = Duration::from_secs(5);
/// What layering either `ui.plan_form*` chain costs. A layer decides what the
/// menu says and what pressing Enter on a row does, up to a build-mode turn
/// with every tool behind it, and it can keep a built-in row's wording while
/// swapping the outcome under it. No narrower price would be honest.
const PLAN_FORM_COST: LayerCost<'static> = LayerCost::TOTAL;
/// Wall clock a picked plan row's handler gets. The row's built-in action
/// waits behind it, so a handler that parks for good would cost the user the
/// outcome the row promised as well as the one the plugin wanted.
///
/// The whole budget a pick can take here, so the UI derives its own fallback
/// from it rather than from a number of its own.
pub const PLAN_ROW_HANDLER_DEADLINE: Duration = Duration::from_secs(30);

/// The event both `ui.plan_form*` chains see: which draft landed, and whose.
fn plan_form_event(lua: &Lua, path: &str, session: &str) -> mlua::Result<MultiValue> {
    let ev = lua.create_table()?;
    ev.set("path", path)?;
    ev.set("session", session)?;
    Ok(MultiValue::from_vec(vec![LuaValue::Table(ev)]))
}

/// A `ui.plan_form*` chain that ended without an answer, logged with the
/// plugins layering it, since a chain that never came back cannot say which
/// of them parked.
fn plan_chain_abandoned(lua: &Lua, slot: &str, reason: &str) {
    tracing::warn!(
        slot,
        plugins = %layer_plugins(lua, slot),
        reason,
        "plan form slot chain abandoned, falling back to the built-in form"
    );
}

/// Asks the `ui.plan_form` chain whether the built-in plan form should open
/// for the draft that just landed. `true` means every layer deferred and the
/// chain reached the host default.
///
/// Layering costs [`PLAN_FORM_COST`], and a layer that throws or parks
/// for good leaves the built-in form, never a dead surface.
async fn plan_form_opens(
    lua: &Lua,
    plugins: &PluginMap,
    gate: &Rc<InflightGate>,
    deadline: Instant,
    args: MultiValue,
) -> bool {
    let allow_layer = layer_delegation(lua, plugins, PLAN_FORM_COST);
    let chain = run_host_chain(lua, &[PLAN_FORM_SLOT], args, &allow_layer);
    match run_awaited(
        lua,
        gate,
        TaskCell::new(CancelToken::none(), Some(deadline)),
        chain,
    )
    .await
    {
        // `None` is "nothing layered it", so there is nobody else to draw it.
        Ok(Ok(None)) => true,
        // The host default hands the event table back, so a chain every layer
        // deferred through answers truthy. A layer that returns nothing, or
        // `false`, keeps the form closed and owns the surface.
        Ok(Ok(Some(values))) => values
            .into_iter()
            .next()
            .is_some_and(|v| !matches!(v, LuaValue::Nil | LuaValue::Boolean(false))),
        Ok(Err(e)) => {
            tracing::warn!(
                slot = PLAN_FORM_SLOT,
                plugins = %layer_plugins(lua, PLAN_FORM_SLOT),
                error = %strip_traceback(&e),
                "plan form slot chain failed"
            );
            true
        }
        Err(reason) => {
            plan_chain_abandoned(lua, PLAN_FORM_SLOT, reason);
            true
        }
    }
}

/// Asks the `ui.plan_form.actions` chain for the menu the form draws, and
/// stashes the handlers of the plugin rows that came back so a pick can find
/// them, under the generation the form has to echo back.
///
/// The default answers with {proposed}, the host's own rows, which is also
/// what a layer that throws, parks for good, or answers off contract leaves
/// behind. Layering costs [`PLAN_FORM_COST`], since a row decides what
/// the agent does next.
async fn plan_form_rows(
    lua: &Lua,
    plugins: &PluginMap,
    gate: &Rc<InflightGate>,
    deadline: Instant,
    session: &str,
    args: MultiValue,
    proposed: Vec<PlanFormRow>,
) -> PlanMenu {
    let allow_layer = layer_delegation(lua, plugins, PLAN_FORM_COST);
    let host_rows = proposed.clone();
    // Which layer put which handler in the menu, the one thing the answered
    // table cannot say on its own.
    let owners: Arc<Mutex<RowOwners>> = Arc::default();
    let observer = Arc::clone(&owners);
    let observe: ChainObserver = Arc::new(move |plugin: &Arc<str>, values: &MultiValue| {
        if let Some(LuaValue::Table(table)) = values.iter().next() {
            observer.lock().expect("row owners").observe(plugin, table);
        }
    });
    let answered =
        match lua.create_function(move |lua, _: MultiValue| rows_to_table(lua, &host_rows)) {
            Ok(default) => {
                let chain = run_host_chain_with(
                    lua,
                    &[PLAN_FORM_ACTIONS_SLOT],
                    default,
                    args,
                    &allow_layer,
                    Some(observe),
                );
                run_awaited(
                    lua,
                    gate,
                    TaskCell::new(CancelToken::none(), Some(deadline)),
                    chain,
                )
                .await
            }
            Err(e) => Ok(Err(e)),
        };
    let answered = match answered {
        Ok(Ok(Some(values))) => match values.into_iter().next() {
            Some(LuaValue::Table(table)) => {
                let owners = owners.lock().expect("row owners");
                rows_from_table(lua, table, &owners)
                    .inspect_err(|e| {
                        tracing::warn!(
                            slot = PLAN_FORM_ACTIONS_SLOT,
                            plugins = %layer_plugins(lua, PLAN_FORM_ACTIONS_SLOT),
                            error = %e,
                            "plan form actions slot answered off contract"
                        );
                    })
                    .ok()
            }
            _ => None,
        },
        Ok(Ok(None)) => None,
        Ok(Err(e)) => {
            tracing::warn!(
                slot = PLAN_FORM_ACTIONS_SLOT,
                plugins = %layer_plugins(lua, PLAN_FORM_ACTIONS_SLOT),
                error = %strip_traceback(&e),
                "plan form actions slot chain failed"
            );
            None
        }
        Err(reason) => {
            plan_chain_abandoned(lua, PLAN_FORM_ACTIONS_SLOT, reason);
            None
        }
    };
    // A plugin that wants no form keeps `ui.plan_form` closed instead of
    // answering with an empty menu.
    let (rows, handlers) = answered
        .filter(|answered| !answered.0.is_empty())
        .unwrap_or_else(|| (proposed, Vec::new()));
    match install_row_handlers(lua, session.to_owned(), handlers) {
        Ok(generation) => PlanMenu { generation, rows },
        Err(e) => {
            // Nothing was stashed, so no row may go on claiming a handler: a
            // pick would echo a generation that answers for nothing, and the
            // built-in outcome the row named would go with it.
            tracing::warn!(error = %e, "could not stash the plan form row handlers");
            PlanMenu {
                generation: 0,
                rows: rows.into_iter().filter_map(without_handler).collect(),
            }
        }
    }
}

/// A row the host could not stash a handler for. What is left is the built-in
/// outcome it named, and a row that named none is one nobody can run.
fn without_handler(mut row: PlanFormRow) -> Option<PlanFormRow> {
    row.plugin = None;
    row.action.is_some().then_some(row)
}

/// Both chains a landed plan fires, under one deadline: they run back to back
/// for a single wait the user sees, so a budget each would let the host take
/// twice what either constant says. `None` is a layer having taken the form
/// over, so the host draws nothing for this draft.
pub(crate) async fn open_plan_form(
    lua: &Lua,
    plugins: &PluginMap,
    gate: &Rc<InflightGate>,
    path: String,
    session: &str,
    rows: Vec<PlanFormRow>,
) -> Option<PlanMenu> {
    let deadline = Instant::now() + PLAN_FORM_SLOT_DEADLINE;
    let args = match plan_form_event(lua, &path, session) {
        Ok(args) => args,
        Err(e) => {
            tracing::warn!(error = %e, "could not build the plan form slot event");
            return Some(PlanMenu {
                generation: 0,
                rows,
            });
        }
    };
    if !plan_form_opens(lua, plugins, gate, deadline, args.clone()).await {
        return None;
    }
    Some(plan_form_rows(lua, plugins, gate, deadline, session, args, rows).await)
}
