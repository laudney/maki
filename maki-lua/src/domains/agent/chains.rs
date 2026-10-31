//! The host-owned slot chains the agent fires: tool and agent hooks, and
//! `permission.prompt`.

use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use maki_agent::permissions::{LayerAnswer, LayerVerdict};
use maki_agent::tools::hook::Verdict;
use maki_types::cancel::CancelToken;
use mlua::{Lua, MultiValue, Table, Value as LuaValue};
use serde_json::Value;

use crate::api::slot::{
    ChainObserver, LayerCost, PERMISSION_PROMPT_SLOT, layer_plugins, run_host_chain,
    run_host_chain_with,
};
use crate::api::util::convert::{TableExt, json_to_lua, lua_to_json_within};
use crate::api::util::ctx::LuaCtx;
use crate::runtime::{
    InflightGate, PluginMap, TaskCell, layer_delegation, run_awaited, strip_traceback,
};

const ASK_FIELD: &str = "ask";

/// One firing of a host-owned slot, as the call being filtered described it.
pub(crate) struct HookRun {
    pub working_dir: Option<Arc<Path>>,
    /// Innermost first, run as one chain. See [`run_host_chain`].
    pub slots: Vec<String>,
    pub cost: LayerCost<'static>,
    /// The filtered call's own cancellation and window. The chain runs on the
    /// Lua thread, so without them nothing here knows when the caller stopped
    /// waiting, and a layer that parks outlives the call it filters.
    pub cancel: CancelToken,
    pub deadline: Instant,
    pub value: Value,
    pub call: Value,
    /// Only a tool input can be put in front of the user, so anywhere else an
    /// `ask` is dropped and a rewrite next to it still lands.
    pub may_ask: bool,
}

/// One call that would have prompted, for the `permission.prompt` chain.
pub(crate) struct PromptRun {
    pub cost: LayerCost<'static>,
    pub cancel: CancelToken,
    pub deadline: Instant,
    pub req: Value,
    pub ctx: Box<LuaCtx>,
}

/// Fires a host-owned chain and reads back the one contract every host slot
/// shares: a table replaces the value, `nil` leaves it alone, and
/// `nil, reason` stops the call with a reason the model reads. A second value
/// of `{ ask = reason }` instead escalates the call to the user, with or
/// without a rewrite in front of it, where [`HookRun::may_ask`] allows it.
///
/// Every failure below is a pass-through, because a layer is an opinion about a
/// call and never a precondition for making it.
///
/// [`run_awaited`] is what makes the chain answerable to the call waiting on
/// it, down to a layer that never comes back ending at the window dispatch
/// gave it.
pub(crate) async fn run_hook(
    lua: &Lua,
    plugins: &PluginMap,
    gate: &Rc<InflightGate>,
    run: HookRun,
) -> Verdict {
    let HookRun {
        working_dir,
        slots,
        cost,
        cancel,
        deadline,
        value,
        call,
        may_ask,
    } = run;
    let names: Vec<&str> = slots.iter().map(String::as_str).collect();
    let Some(&slot) = names.first() else {
        return Verdict::Unchanged;
    };
    let args = match (json_to_lua(lua, &value), json_to_lua(lua, &call)) {
        (Ok(value), Ok(call)) => MultiValue::from_vec(vec![value, call]),
        _ => return Verdict::Unchanged,
    };
    let allow_layer = layer_delegation(lua, plugins, cost);
    let chain = run_host_chain(lua, &names, args, &allow_layer);
    let mut cell = TaskCell::new(cancel, Some(deadline));
    cell.lineage.working_dir = working_dir;
    let returned = match run_awaited(lua, gate, cell, chain).await {
        Ok(Ok(Some(values))) => values,
        Ok(Ok(None)) => return Verdict::Unchanged,
        Ok(Err(e)) => {
            tracing::warn!(slot, error = %strip_traceback(&e), "slot chain failed");
            return Verdict::Unchanged;
        }
        Err(msg) => {
            tracing::warn!(slot, reason = msg, "slot chain abandoned");
            return Verdict::Unchanged;
        }
    };

    let mut returned = returned.into_iter();
    let first = returned.next();
    let ask = match returned.next() {
        Some(LuaValue::Table(second)) => match second.opt::<String>(ASK_FIELD) {
            Ok(reason) => reason,
            Err(e) => {
                tracing::warn!(slot, error = %e, "slot answered `ask` with something other than a string");
                None
            }
        },
        Some(LuaValue::String(reason)) if matches!(first, None | Some(LuaValue::Nil)) => {
            return Verdict::Denied(reason.to_string_lossy());
        }
        _ => None,
    };
    let ask = match ask {
        Some(reason) if !may_ask => {
            tracing::warn!(
                slot,
                reason,
                "only a tool input layer can ask, so the ask was dropped"
            );
            None
        }
        ask => ask,
    };
    let replaced = match first {
        Some(table @ LuaValue::Table(_)) => match lua_to_json_within(lua, &table, &value) {
            // The identity default hands back the value it was given, so a
            // layer that only deferred returns a table too. Comparing is the
            // only way to tell that apart from a rewrite, and it keeps the
            // original `Value` in play instead of a re-encode of it.
            Ok(replacement) if replacement == value => None,
            Ok(replacement) => Some(replacement),
            Err(e) => {
                tracing::warn!(slot, error = %strip_traceback(&e), "slot returned a table that is not json");
                None
            }
        },
        None | Some(LuaValue::Nil) => None,
        Some(other) => {
            tracing::warn!(
                slot,
                returned = other.type_name(),
                "slot must return a table, nil, or nil plus a reason"
            );
            None
        }
    };
    match (ask, replaced) {
        (Some(reason), input) => Verdict::Ask { reason, input },
        (None, Some(replacement)) => Verdict::Replaced(replacement),
        (None, None) => Verdict::Unchanged,
    }
}

/// Fires `permission.prompt` for a call that would have prompted. The host
/// default answers nothing, which is what leaves the call to the real prompt,
/// and so does every layer failure: a broken layer costs the user a prompt,
/// never an approval.
pub(crate) async fn run_prompt(
    lua: &Lua,
    plugins: &PluginMap,
    gate: &Rc<InflightGate>,
    run: PromptRun,
) -> Option<LayerAnswer> {
    let slot = PERMISSION_PROMPT_SLOT;
    let PromptRun {
        cost,
        cancel,
        deadline,
        req,
        ctx,
    } = run;
    let working_dir = ctx.agent().map(|agent| Arc::clone(&agent.working_dir));
    let (Ok(req), Ok(ctx), Ok(default)) = (
        json_to_lua(lua, &req),
        lua.create_userdata(*ctx),
        lua.create_function(|_, _: MultiValue| Ok(())),
    ) else {
        return None;
    };
    let args = MultiValue::from_vec(vec![req, LuaValue::UserData(ctx)]);
    // The answer a layer passes back up still belongs to the layer that
    // built it, and the chain unwinds innermost first, so the first layer to
    // return a given table is the one that decided.
    let decided = Arc::new(Mutex::new(None::<(Table, Arc<str>)>));
    let observer = Arc::clone(&decided);
    let observe: ChainObserver = Arc::new(move |plugin: &Arc<str>, values: &MultiValue| {
        let Some(LuaValue::Table(table)) = values.iter().next() else {
            return;
        };
        let mut decided = observer.lock().expect("prompt decider");
        if decided.as_ref().is_none_or(|(seen, _)| seen != table) {
            *decided = Some((table.clone(), Arc::clone(plugin)));
        }
    });
    let allow_layer = layer_delegation(lua, plugins, cost);
    let names = [slot];
    let chain = run_host_chain_with(lua, &names, default, args, &allow_layer, Some(observe));
    let mut cell = TaskCell::new(cancel, Some(deadline));
    cell.lineage.working_dir = working_dir;
    let table = match run_awaited(lua, gate, cell, chain).await {
        Ok(Ok(Some(values))) => match values.into_iter().next() {
            Some(LuaValue::Table(table)) => table,
            _ => return None,
        },
        Ok(Ok(None)) => return None,
        Ok(Err(e)) => {
            tracing::warn!(
                slot,
                plugins = %layer_plugins(lua, slot),
                error = %strip_traceback(&e),
                "slot chain failed, prompting"
            );
            return None;
        }
        Err(reason) => {
            tracing::warn!(
                slot,
                plugins = %layer_plugins(lua, slot),
                reason,
                "slot chain abandoned, prompting"
            );
            return None;
        }
    };
    let (_, plugin) = decided
        .lock()
        .expect("prompt decider")
        .take()
        .filter(|(seen, _)| *seen == table)?;
    match prompt_answer(&table) {
        Ok(verdict) => Some(LayerAnswer { plugin, verdict }),
        Err(e) => {
            tracing::warn!(slot, %plugin, error = %e, "slot answered off contract, prompting");
            None
        }
    }
}

/// Reads a `permission.prompt` answer, `{ decision, guidance? }`. The prompt's
/// own answers like `allow_session` are refused, see [`LayerVerdict`] for why.
fn prompt_answer(table: &Table) -> mlua::Result<LayerVerdict> {
    let decision: String = table.get("decision")?;
    let guidance: Option<String> = table.opt("guidance")?;
    match decision.as_str() {
        "allow" => Ok(LayerVerdict::Allow),
        "deny" => Ok(LayerVerdict::Deny {
            guidance: guidance.filter(|g| !g.is_empty()),
        }),
        other => Err(mlua::Error::runtime(format!(
            "a permission.prompt layer answers 'allow' or 'deny', not '{other}'"
        ))),
    }
}
