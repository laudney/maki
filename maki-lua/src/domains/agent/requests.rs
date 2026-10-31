//! What the agent asks of its plugins on the VM thread: tool calls, their
//! permission scopes, summaries and descriptions, host hook chains, prompt
//! slots and provider hooks.

use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use flume::Sender;
use futures::future::LocalBoxFuture;
use maki_agent::permissions::LayerAnswer;
use maki_agent::prompt::ResolvedPrompts;
use maki_agent::tools::PermissionScopes;
use maki_agent::tools::hook::Verdict;
use maki_types::cancel::CancelToken;
use serde_json::Value;

use super::chains::{HookRun, PromptRun, run_hook, run_prompt};
use super::moves::{self, CallAnswer};
use super::prompt_hints::collect_prompt_slots;
use super::tools::{compute_permission_scopes, compute_summary, run_describe, run_tool_call};
use crate::api::provider::{self, HookAnswer, HookSlot, LuaHookKeys};
use crate::api::tool::ToolCallReply;
use crate::api::util::ctx::LuaCtx;
use crate::domain::{DomainRequest, Vm};
use crate::plugin_permissions::Permission;
use crate::runtime::{DrainHold, TaskCell, covered, run_abandonable};

/// One call into a hook a plugin registered with `maki.provider`.
///
/// Carries the hook handles themselves rather than a plugin name: the call
/// runs against the functions it started with, so an unload mid-flight
/// cannot swap the code out from under it. Rides the priority lane and is
/// spawned like a tool call, but takes no [`InflightGate`] slot: a provider
/// hook is not a tool, and the model is waiting on it.
///
/// Ends like a tool call does, though: at `deadline`, or once `cancel`
/// fires because the caller stopped waiting. A hook nobody waits for must
/// not keep spending a refresh token behind the caller's back.
///
/// [`InflightGate`]: crate::runtime::InflightGate
pub(crate) struct CallProviderHook {
    pub hook: Arc<LuaHookKeys>,
    pub slot: HookSlot,
    pub payload: Value,
    pub cancel: CancelToken,
    pub deadline: Option<Instant>,
    pub answer: HookAnswer,
}

impl DomainRequest for CallProviderHook {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self {
            hook,
            slot,
            payload,
            cancel,
            deadline,
            answer,
        } = *self;
        Box::pin(async move {
            let lua = vm.rt.lua.clone();
            tokio::task::spawn_local(async move {
                let returned = run_abandonable(
                    &lua,
                    TaskCell::new(cancel, deadline),
                    provider::run_hook(&lua, &hook, slot, payload),
                )
                .await
                .unwrap_or_else(|why| Err(why.to_owned()));
                answer(&lua, returned);
            });
        })
    }
}

pub(crate) struct CallTool {
    pub plugin: Arc<str>,
    pub tool: Arc<str>,
    pub input: Value,
    pub ctx: Box<LuaCtx>,
    pub deadline: Option<Instant>,
    pub reply: Sender<ToolCallReply>,
    /// Runs on the caller's slot instead of taking one of its own.
    /// See [`under_inflight_slot`](crate::runtime::under_inflight_slot).
    pub nested: bool,
}

impl DomainRequest for CallTool {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self {
            plugin,
            tool,
            input,
            ctx,
            deadline,
            reply,
            nested,
        } = *self;
        Box::pin(async move {
            let lua = vm.rt.lua.clone();
            let shutdown_ref = Arc::clone(&vm.rt.shutdown);
            let g = Rc::clone(vm.gate);
            let cancel = ctx.cancel.clone();
            let call = ctx.agent().and_then(|agent| agent.call.clone());
            let live_id = ctx.agent().and_then(|agent| agent.tool_use_id.clone());
            let can_move = vm
                .rt
                .plugins
                .borrow()
                .get(&plugin)
                .is_some_and(|owner| owner.permissions().is_allowed(Permission::Agent));
            tokio::task::spawn_local(async move {
                let _hold = nested.then(|| DrainHold::under(&g, moves::drain_owner(call)));
                let slot = if nested {
                    None
                } else {
                    match g.acquire_before_abandoned(&cancel, deadline).await {
                        Ok(guard) => Some(guard),
                        Err(msg) => {
                            let _ = reply.send(ToolCallReply::err(msg));
                            return;
                        }
                    }
                };
                let answer = Rc::new(CallAnswer::new(reply, slot));
                let res = covered(
                    None,
                    run_tool_call(
                        lua.clone(),
                        plugin,
                        tool,
                        input,
                        ctx,
                        deadline,
                        shutdown_ref,
                        can_move,
                        Rc::clone(&answer),
                    ),
                )
                .await;
                answer.finish(&lua, live_id.as_deref(), res);
            });
        })
    }
}

pub(crate) struct ComputePermissionScopes {
    pub working_dir: Arc<Path>,
    pub plugin: Arc<str>,
    pub tool: Arc<str>,
    pub input: Value,
    pub reply: Sender<Option<PermissionScopes>>,
}

impl DomainRequest for ComputePermissionScopes {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self {
            working_dir,
            plugin,
            tool,
            input,
            reply,
        } = *self;
        Box::pin(async move {
            let res =
                compute_permission_scopes(&vm.rt.lua, &plugin, &tool, input, working_dir).await;
            let _ = reply.send(res);
        })
    }
}

pub(crate) struct ComputeSummary {
    pub plugin: Arc<str>,
    pub tool: Arc<str>,
    pub input: Value,
    pub reply: Sender<Option<String>>,
}

impl DomainRequest for ComputeSummary {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self {
            plugin,
            tool,
            input,
            reply,
        } = *self;
        Box::pin(async move {
            let _ = reply.send(compute_summary(&vm.rt.lua, &plugin, &tool, &input));
        })
    }
}

/// A host-owned slot chain (`tool.<name>.input`, `tool.<name>.output`,
/// `agent.*`). Only sent when the slot has layers, so the idle case never
/// reaches the request loop at all.
pub(crate) struct RunHook {
    pub run: HookRun,
    pub reply: Sender<Verdict>,
}

impl DomainRequest for RunHook {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self { run, reply } = *self;
        Box::pin(async move {
            // Spawned rather than awaited: a layer may park, and every other
            // session is waiting on this request loop.
            let lua = vm.rt.lua.clone();
            let plugins = Rc::clone(&vm.rt.plugins);
            let gate = Rc::clone(vm.gate);
            tokio::task::spawn_local(async move {
                let verdict = run_hook(&lua, &plugins, &gate, run).await;
                let _ = reply.send(verdict);
            });
        })
    }
}

/// The `permission.prompt` chain, sent only when it has layers.
pub(crate) struct RunPrompt {
    pub run: PromptRun,
    pub reply: Sender<Option<LayerAnswer>>,
}

impl DomainRequest for RunPrompt {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self { run, reply } = *self;
        Box::pin(async move {
            let lua = vm.rt.lua.clone();
            let plugins = Rc::clone(&vm.rt.plugins);
            let gate = Rc::clone(vm.gate);
            tokio::task::spawn_local(async move {
                let answer = run_prompt(&lua, &plugins, &gate, run).await;
                let _ = reply.send(answer);
            });
        })
    }
}

pub(crate) struct CollectPromptSlots {
    pub reply: Sender<ResolvedPrompts>,
}

impl DomainRequest for CollectPromptSlots {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self { reply } = *self;
        Box::pin(async move {
            let slots = collect_prompt_slots(&vm.rt.lua).await;
            let _ = reply.send(slots);
        })
    }
}

pub(crate) struct Describe {
    pub plugin: Arc<str>,
    pub tool: Arc<str>,
    pub dctx: Value,
    pub reply: Sender<Option<String>>,
}

impl DomainRequest for Describe {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self {
            plugin,
            tool,
            dctx,
            reply,
        } = *self;
        Box::pin(async move {
            let _ = reply.send(run_describe(&vm.rt.lua, &plugin, &tool, &dctx));
        })
    }
}
