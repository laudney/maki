//! The calls into a tool's own callbacks on the VM thread: its handler, its
//! permission scopes, its summary and its description.

use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use maki_agent::tools::PermissionScopes;
use maki_types::cancel::CancelToken;
use mlua::{Function, Lua, Value as LuaValue};
use serde_json::Value;

use super::AgentToolKeys;
use super::moves::{self, AgentTask, CallAnswer};
use crate::api::r#fn::{JobOwner, deliver_job_event};
use crate::api::tool::ToolCallReply;
use crate::api::util::convert::{TableExt, json_to_lua};
use crate::api::util::ctx::LuaCtx;
use crate::domains::{ToolRecords, plugin_fn};
use crate::runtime::{
    CANCELLED_MSG, DISPATCH_POLL_INTERVAL, KillReason, TaskCell, TaskHandle, TaskScope,
    fire_cancel_hooks, lock_cell, strip_traceback, until_abandoned, with_jobs,
};

const NIL_WITHOUT_FINISH_MSG: &str =
    "handler returned nil without calling ctx:finish() or starting jobs";
const SUMMARY_CALLBACK: &str = "summary";

pub(crate) async fn compute_permission_scopes(
    lua: &Lua,
    plugin: &str,
    tool: &str,
    input: Value,
    working_dir: Arc<Path>,
) -> Option<PermissionScopes> {
    let (func, lua_input) = plugin_fn(
        lua,
        plugin,
        tool,
        "permission_scopes",
        |tk: &AgentToolKeys| tk.permission_scopes.as_ref(),
        &input,
    )?;
    let mut cell = TaskCell::new(CancelToken::none(), None);
    cell.lineage.working_dir = Some(working_dir);
    let scope = TaskScope::new(lua, cell);
    let result: LuaValue = match scope.scope_future(func.call_async(lua_input)).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(plugin, tool, error = %e, "permission_scopes callback failed");
            return None;
        }
    };
    let table = match result {
        LuaValue::Table(t) => t,
        _ => return None,
    };
    let scopes_table: mlua::Table = table.get("scopes").ok()?;
    let mut scopes = Vec::new();
    for (_, s) in scopes_table.pairs::<usize, String>().flatten() {
        scopes.push(s);
    }
    if scopes.is_empty() {
        return None;
    }
    let force_prompt: bool = table.opt_bool("force_prompt").unwrap_or(false);
    Some(PermissionScopes {
        scopes,
        force_prompt,
    })
}

/// Called synchronously, so a summary that yields fails into the header
/// fallback instead of holding the dispatcher that would have to run the
/// work it waits on. The slice lets the watchdog stop a spinning one.
pub(crate) fn compute_summary(
    lua: &Lua,
    plugin: &str,
    tool: &str,
    input: &Value,
) -> Option<String> {
    let (func, lua_input) = plugin_fn(
        lua,
        plugin,
        tool,
        SUMMARY_CALLBACK,
        |tk: &AgentToolKeys| tk.summary.as_ref(),
        input,
    )?;
    let scope = TaskScope::detached(lua);
    let _active = scope.enter();
    lock_cell(scope.handle()).begin_slice(Instant::now());
    match func.call::<LuaValue>(lua_input) {
        Ok(LuaValue::String(s)) => match s.to_str() {
            Ok(s) => Some(s.to_owned()),
            Err(e) => {
                tracing::warn!(
                    plugin,
                    tool,
                    error = %e,
                    "summary callback returned invalid UTF-8"
                );
                None
            }
        },
        Ok(other) => {
            tracing::warn!(
                plugin,
                tool,
                returned = other.type_name(),
                "summary callback must return a string"
            );
            None
        }
        Err(e) => {
            tracing::warn!(plugin, tool, error = %e, "summary callback failed");
            None
        }
    }
}

/// The last slice a doomed handler gets: its cancel hooks run (firing twice
/// is free, they drain once), and a reply they queue through `ctx:finish`
/// wins, because it carries the output the user already watched stream by.
fn cancel_hook_reply(
    lua: &Lua,
    handle: &TaskHandle,
    finish_rx: &flume::Receiver<ToolCallReply>,
    reason: KillReason,
) -> Option<ToolCallReply> {
    fire_cancel_hooks(lua, handle, reason);
    finish_rx.try_recv().ok()
}

/// Handler returned nil, meaning it went async. Polls job events
/// until `ctx:finish()`, all jobs die, or the deadline expires.
async fn dispatch_async(
    lua: &Lua,
    handle: TaskHandle,
    plugin: &str,
    tool: &str,
    finish_rx: flume::Receiver<ToolCallReply>,
) -> ToolCallReply {
    let owner = JobOwner::Task(lock_cell(&handle).id);
    if with_jobs(lua, |store| store.is_empty(&owner)) {
        lua.gc_collect().ok();
        tokio::time::sleep(DISPATCH_POLL_INTERVAL).await;
        return match finish_rx.try_recv() {
            Ok(reply) => reply,
            _ => ToolCallReply::err(NIL_WITHOUT_FINISH_MSG),
        };
    }

    loop {
        // No grace here: the handler already returned, so there is no Lua
        // frame left to unwind. Bound before the match so the guard drops
        // before `timeout_reply`, which locks the same cell.
        let kill = lock_cell(&handle).doomed(Instant::now());
        if let Some(reason) = kill {
            if let Some(reply) = cancel_hook_reply(lua, &handle, &finish_rx, reason) {
                return reply;
            }
            return match reason {
                KillReason::Cancelled => ToolCallReply::err(CANCELLED_MSG),
                KillReason::Deadline => timeout_reply(&handle, plugin, tool),
            };
        }

        match finish_rx.try_recv() {
            Ok(reply) => return reply,
            Err(flume::TryRecvError::Disconnected) => {
                return ToolCallReply::err(NIL_WITHOUT_FINISH_MSG);
            }
            Err(flume::TryRecvError::Empty) => {}
        }

        if let Some((job_id, event)) = with_jobs(lua, |store| store.next_event(&owner)) {
            if let Err(e) = deliver_job_event(lua, job_id, &event).await {
                return ToolCallReply::err(format!("job callback error: {}", strip_traceback(&e)));
            }
            tokio::task::yield_now().await;
            continue;
        }

        if with_jobs(lua, |store| store.is_empty(&owner)) {
            tokio::time::sleep(DISPATCH_POLL_INTERVAL).await;
            return match finish_rx.try_recv() {
                Ok(reply) => reply,
                _ => ToolCallReply::err(NIL_WITHOUT_FINISH_MSG),
            };
        }
        tokio::time::sleep(DISPATCH_POLL_INTERVAL).await;
    }
}

/// Only for a tool whose cancel hooks queued nothing; one that words its
/// own partial reply never gets here. The message format is load-bearing
/// for those: the bash plugin's `restore` parses it to re-render the
/// timeout sentinel on session reload.
fn timeout_reply(handle: &TaskHandle, plugin: &str, tool: &str) -> ToolCallReply {
    let secs = lock_cell(handle).deadline_secs.get().unwrap_or(0);
    let qualified = if plugin == tool || plugin.is_empty() {
        tool.to_owned()
    } else {
        format!("{plugin}.{tool}")
    };

    ToolCallReply::err(format!("tool {qualified} timed out after {secs}s"))
}

pub(crate) fn run_describe(lua: &Lua, plugin: &str, tool: &str, dctx: &Value) -> Option<String> {
    let func: Function = {
        let records = lua.app_data_ref::<ToolRecords<AgentToolKeys>>()?;
        let key = records.0.get(plugin)?.get(tool)?.describe.as_ref()?;
        lua.registry_value(key).ok()?
    };
    let arg = match json_to_lua(lua, dctx) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(plugin, tool, error = %e, "describe dctx conversion failed");
            return None;
        }
    };
    // Runs inline on the dispatcher, so no ScopedFuture publishes the task or
    // starts its slice. Both by hand, or a spinning callback would never be
    // stopped.
    let scope = TaskScope::detached(lua);
    let _active = scope.enter();
    lock_cell(scope.handle()).begin_slice(Instant::now());
    match func.call::<String>(arg) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(plugin, tool, error = %e, "describe callback failed");
            None
        }
    }
}

/// Two layers of deadline enforcement: the watchdog interrupt catches
/// tight CPU loops, the dispatch loop catches I/O waits.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_tool_call(
    lua: Lua,
    plugin: Arc<str>,
    tool: Arc<str>,
    input: Value,
    mut ctx: Box<LuaCtx>,
    deadline: Option<Instant>,
    shutdown: Arc<AtomicBool>,
    can_move: bool,
    answer: Rc<CallAnswer>,
) -> ToolCallReply {
    let handler: Function = {
        let records = lua.app_data_ref::<ToolRecords<AgentToolKeys>>();
        let Some(tools) = records.as_ref().and_then(|records| records.0.get(&*plugin)) else {
            return ToolCallReply::err(format!("plugin not loaded: {plugin}"));
        };
        let Some(tool_keys) = tools.get(&*tool) else {
            return ToolCallReply::err(format!("tool not found: {tool}"));
        };
        match lua.registry_value(&tool_keys.handler) {
            Ok(f) => f,
            Err(e) => return ToolCallReply::err(strip_traceback(&e)),
        }
    };
    if shutdown.load(Ordering::Acquire) {
        return ToolCallReply::err("plugin host shutting down");
    }

    let (finish_tx, finish_rx) = flume::bounded::<ToolCallReply>(1);
    ctx.finish_tx = Some(finish_tx);
    let cancel = ctx.cancel.clone();

    let input_lua = match json_to_lua(&lua, &input) {
        Ok(v) => v,
        Err(e) => return ToolCallReply::err(strip_traceback(&e)),
    };
    let session = ctx.session();
    let agent = ctx.agent();
    let working_dir = agent.map(|agent| Arc::clone(&agent.working_dir));
    let call = agent.and_then(|agent| agent.call.clone());
    let live_id = agent.and_then(|agent| agent.tool_use_id.clone());
    let chat = agent
        .and_then(|agent| agent.session_id.as_ref())
        .map(|id| id.id());
    let mailbox = agent.and_then(|agent| agent.mailbox.clone());
    let subagent = agent.is_some_and(|agent| agent.task_id.is_some());
    let ctx_ud = match lua.create_userdata(*ctx) {
        Ok(u) => u,
        Err(e) => return ToolCallReply::err(strip_traceback(&e)),
    };

    let mut cell = TaskCell::new(cancel.clone(), deadline);
    cell.lineage.session = session;
    cell.lineage.working_dir = working_dir;
    cell.exts.insert(AgentTask {
        call,
        ..AgentTask::default()
    });
    let scope = TaskScope::new(&lua, cell);
    let handle = Arc::clone(scope.handle());

    let task = live_id.as_ref().map(|id| {
        moves::register(
            id.clone(),
            Arc::clone(&handle),
            Arc::clone(&plugin),
            Arc::clone(&tool),
            input,
            chat,
            mailbox,
            subagent,
            can_move,
            answer,
        )
    });
    let async_thread = handler.call_async::<LuaValue>((input_lua, ctx_ud));
    let call_future = scope.scope_future(async {
        match until_abandoned(async_thread, &handle)
            .await
            .unwrap_or_else(|msg| Err(mlua::Error::runtime(msg)))
        {
            Ok(LuaValue::Nil) => {
                dispatch_async(&lua, Arc::clone(&handle), &plugin, &tool, finish_rx).await
            }
            Ok(val) => {
                ToolCallReply::from_lua_value(&lua, &val)
            }
            // Bound before the `and_then` so the guard drops before the
            // hooks run: they lock the same cell.
            Err(e) => {
                let kill = lock_cell(&handle).doomed(Instant::now());
                match kill.and_then(|reason| cancel_hook_reply(&lua, &handle, &finish_rx, reason)) {
                    // The reply wins, but a doom-window error is still the
                    // only trace of a plugin bug that raised on its way out.
                    Some(reply) => {
                        tracing::debug!(%tool, error = %strip_traceback(&e), "handler error superseded by cancel hook reply");
                        reply
                    }
                    None if kill == Some(KillReason::Deadline) => timeout_reply(&handle, &plugin, &tool),
                    None => ToolCallReply::err(strip_traceback(&e)),
                }
            }
        }
    });

    // `tool.rs` timeout is the absolute backstop; the dispatch loop
    // and watchdog interrupt enforce the per-plugin deadline from TaskCell.
    let expired = async {
        moves::expire_to_background(&lua, &handle, task.as_deref()).await;
        std::future::pending().await
    };
    let reply = tokio::select! {
        biased;
        reply = call_future => reply,
        reply = expired => reply,
    };
    if let Some(id) = &live_id {
        moves::remove(id);
    }
    drop(scope);
    reply
}

#[cfg(test)]
mod tests {
    use maki_types::cancel::CancelToken;
    use std::thread;
    use std::time::Duration;

    use test_case::test_case;

    use super::*;
    use crate::api::r#fn::JobSpec;
    use crate::runtime::{HANDLER_TIMEOUT_MSG, KILL_GRACE, register_cancel_hook};

    /// Generous vs the expected reply; only a wedged dispatch loop gets here.
    const WATCHDOG_TEST_TIMEOUT: Duration = Duration::from_secs(10);

    fn cancelled_token() -> CancelToken {
        let (trigger, token) = CancelToken::new();
        trigger.cancel();
        token
    }

    const DISPATCH_TEST_JOB: &str = "sleep 1";
    const DISPATCH_TEST_PLUGIN: &str = "shell";
    const DISPATCH_TEST_TOOL: &str = "run";
    const DISPATCH_TEST_DEADLINE_SECS: u64 = 7;

    /// Drives the job-polling loop of [`dispatch_async`] to its reply. The
    /// cell needs a live job or the loop is never entered. Helper thread
    /// again: `timeout_reply` locks the very cell the loop inspects, so a
    /// regression there deadlocks and must fail rather than hang the suite.
    fn drive_dispatch(cell: TaskCell) -> (Result<String, String>, Duration) {
        drive_dispatch_with(cell, |_, _| {})
    }

    /// `setup` runs on the dispatch thread, standing in for a handler that
    /// armed cancel hooks.
    fn drive_dispatch_with(
        cell: TaskCell,
        setup: impl FnOnce(&Lua, flume::Sender<ToolCallReply>) + Send + 'static,
    ) -> (Result<String, String>, Duration) {
        let (tx, rx) = flume::bounded(1);
        thread::spawn(move || {
            let lua = Lua::new();
            let scope = TaskScope::new(&lua, cell);
            let owner = JobOwner::Task(lock_cell(scope.handle()).id);
            with_jobs(&lua, |store| {
                store.start(JobSpec::new(owner, DISPATCH_TEST_JOB))
            })
            .unwrap();
            let (finish_tx, finish_rx) = flume::bounded(1);
            {
                let _active = scope.enter();
                setup(&lua, finish_tx);
            }

            let start = Instant::now();
            let reply = crate::test_support::runtime().block_on(dispatch_async(
                &lua,
                Arc::clone(scope.handle()),
                DISPATCH_TEST_PLUGIN,
                DISPATCH_TEST_TOOL,
                finish_rx,
            ));
            drop(tx.send((reply.result, start.elapsed())));
        });
        rx.recv_timeout(WATCHDOG_TEST_TIMEOUT)
            .expect("dispatch_async never replied: the cell lock is likely held across the reply")
    }

    /// A timed-out async tool must report the timeout the bash plugin's
    /// `restore` parses, and must not wedge the whole Lua thread on the
    /// non-reentrant cell mutex while doing it.
    #[test]
    fn dispatch_async_replies_with_timeout_without_deadlocking() {
        let cell = TaskCell::new(CancelToken::none(), Some(Instant::now()));
        cell.deadline_secs.set(Some(DISPATCH_TEST_DEADLINE_SECS));

        let (result, _) = drive_dispatch(cell);

        assert_eq!(
            result,
            Err(format!(
                "tool {DISPATCH_TEST_PLUGIN}.{DISPATCH_TEST_TOOL} timed out after {DISPATCH_TEST_DEADLINE_SECS}s"
            ))
        );
    }

    /// Esc on an async tool whose deadline also lapsed must say "cancelled",
    /// not "timed out after 0s", and must reply at once: the handler already
    /// returned, so there is nothing left to clean up.
    #[test]
    fn dispatch_async_cancel_outranks_deadline_with_no_grace() {
        let cell = TaskCell::new(cancelled_token(), Some(Instant::now()));

        let (result, elapsed) = drive_dispatch(cell);

        assert_eq!(result, Err(CANCELLED_MSG.to_owned()));
        assert!(
            elapsed < KILL_GRACE,
            "dispatch loop must not wait out a grace"
        );
    }

    const PARTIAL_REPLY_OUTPUT: &str = "partial output";

    /// A doomed handler's cancel hooks get the last word: the reply they
    /// queue carries the output the user already saw, so it must beat the
    /// generic cancelled/timeout error. The reason they are handed is what
    /// lets a tool word its marker.
    #[test_case(true, CANCELLED_MSG ; "cancelled")]
    #[test_case(false, HANDLER_TIMEOUT_MSG ; "deadline")]
    fn dispatch_async_prefers_the_reply_its_cancel_hooks_queued(
        cancelled: bool,
        expected_reason: &str,
    ) {
        let cell = if cancelled {
            TaskCell::new(cancelled_token(), None)
        } else {
            let cell = TaskCell::new(CancelToken::none(), Some(Instant::now()));
            cell.deadline_secs.set(Some(DISPATCH_TEST_DEADLINE_SECS));
            cell
        };

        let (result, _) = drive_dispatch_with(cell, |lua, finish_tx| {
            let hook = lua
                .create_function(move |_, reason: String| {
                    finish_tx
                        .send(ToolCallReply::err(format!(
                            "{PARTIAL_REPLY_OUTPUT}:{reason}"
                        )))
                        .ok();
                    Ok(())
                })
                .unwrap();
            register_cancel_hook(lua, hook).unwrap();
        });

        assert_eq!(
            result,
            Err(format!("{PARTIAL_REPLY_OUTPUT}:{expected_reason}"))
        );
    }
}
