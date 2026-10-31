//! Drawing a call with the renderer its tool registered: the call's data and
//! the render context as Lua sees them, and the snapshots sent back.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::buffer::buf::BufHandle;
use flume::Sender;
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use maki_config::ToolOutputLines;
use maki_types::cancel::CancelToken;
use maki_types::snapshot::BufferSnapshot;
use maki_types::tool::{ChildStatus, ToolData, ToolOutput};
use mlua::{Lua, LuaSerdeExt, Result as LuaResult, Table, Value as LuaValue};
use strum::IntoStaticStr;

use super::warm::{DrawnBy, evict, take_turn, warm, warm_data};
use crate::api::ui::renderer::{Renderer, find_renderer};
use crate::api::util::call::{CHILDREN_KEY, INDEX_KEY, call_to_lua};
use crate::domain::{DomainRequest, Vm};
use crate::runtime::{
    DrainHold, Narrowed, PendingAsyncTask, TaskCell, TaskHandle, lock_cell, run_awaited,
    run_work_fn, strip_traceback, try_active_task,
};

/// Keeps a renderer parked on an await from holding its gate slot for good.
const RENDER_DEADLINE: Duration = Duration::from_secs(10);
/// Inline tasks may queue more; the rounds cap that, as the bridge's do.
const INLINE_TASK_ROUNDS: usize = 8;
const STREAM_KEY: &str = "stream";
const CWD_KEY: &str = "cwd";
const HOME_KEY: &str = "home";
const TOOL_OUTPUT_LINES_KEY: &str = "tool_output_lines";
const WIDTH_KEY: &str = "width";
const REASON_KEY: &str = "reason";
const HEADER_RETURN_ERR: &str = "header must return a string, a buffer or nil";
const BODY_RETURN_ERR: &str = "body must return a buffer or nil";

/// One call as its renderer reads it.
#[derive(Debug, Clone)]
pub struct RenderCall {
    /// The tool use id, which keeps the call's data warm for its clicks.
    pub id: String,
    pub data: Arc<ToolData>,
    /// None while the call runs.
    pub output: Option<Arc<ToolOutput>>,
    pub status: ChildStatus,
    /// The text a running call streamed, by the path of the call that
    /// streamed it, empty for this call and a child's otherwise.
    pub streamed: Vec<(Vec<u32>, String)>,
}

/// What a renderer draws a call for. Every fact comes from the session the
/// call ran in, never from the drawing process.
#[derive(Debug, Clone)]
pub struct RenderCtx {
    pub cwd: PathBuf,
    /// The agent's home, unknown for a remote agent that does not tell it.
    pub home: Option<PathBuf>,
    pub tool_output_lines: ToolOutputLines,
    pub width: u16,
    pub reason: RenderReason,
}

/// Why a call is drawn. Lua sees the lowercase variant names as `ctx.reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum RenderReason {
    /// A running call, or one that just finished.
    Live,
    /// A session's transcript is being rebuilt, every call in order.
    Load,
    /// A click, a theme or a width change draws one call again.
    Rerender,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenderReply {
    /// The renderer drew. A part it left out draws with the default.
    Drawn {
        header: Option<BufferSnapshot>,
        body: Option<BufferSnapshot>,
    },
    /// No renderer is registered, or it raised: the default
    /// renderer draws the whole call, over anything drawn before.
    Default,
}

pub(crate) struct RenderTool {
    pub call: RenderCall,
    pub ctx: RenderCtx,
    /// Replayed in order after the draw, so the state they made survives it.
    pub clicks: Vec<usize>,
    pub reply: Sender<RenderReply>,
}

impl DomainRequest for RenderTool {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self {
            call,
            ctx,
            clicks,
            reply,
        } = *self;
        let (lua, gate) = (vm.rt.lua.clone(), Rc::clone(vm.gate));
        // Taken now, so a reload waits for a draw still queued for its turn.
        let hold = DrainHold::new(&gate);
        // Spawned: a renderer may yield, and every other request waits on
        // this loop.
        vm.ex.spawn_local(async move {
            let _hold = hold;
            let _turn = take_turn(&lua, &call.id).await;
            // A new draw starts from fresh data, whatever it turns out to be.
            evict(&lua, &call.id);
            let Some(renderer) = renderer_for(&lua, &call, &reply) else {
                return;
            };
            let deadline = Instant::now() + RENDER_DEADLINE;
            let drawn = run_awaited(
                &lua,
                &gate,
                TaskCell::new(CancelToken::none(), Some(deadline)),
                replay(&lua, &renderer, &call, &ctx, &clicks, deadline),
            )
            .await;
            answer(&lua, &renderer, &call, drawn, &reply);
        });
        Box::pin(async {})
    }
}

pub(crate) struct ClickTool {
    pub call: RenderCall,
    pub ctx: RenderCtx,
    /// The clicked line of the body, 1-based, or 0 outside it.
    pub row: usize,
    /// The call's clicks before this one, in order, replayed when its data
    /// is no longer warm.
    pub earlier: Vec<usize>,
    pub reply: Sender<RenderReply>,
}

impl DomainRequest for ClickTool {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self {
            call,
            ctx,
            row,
            earlier,
            reply,
        } = *self;
        let (lua, gate) = (vm.rt.lua.clone(), Rc::clone(vm.gate));
        let hold = DrainHold::new(&gate);
        vm.ex.spawn_local(async move {
            let _hold = hold;
            // The data is read once the clicks before this one are done
            // with it.
            let _turn = take_turn(&lua, &call.id).await;
            let Some(renderer) = renderer_for(&lua, &call, &reply) else {
                evict(&lua, &call.id);
                return;
            };
            let warm = warm_data(&lua, &call.id, &drawn_by(&renderer, &call));
            let deadline = Instant::now() + RENDER_DEADLINE;
            let run = |fut| {
                run_awaited(
                    &lua,
                    &gate,
                    TaskCell::new(CancelToken::none(), Some(deadline)),
                    fut,
                )
            };
            let drawn = match warm {
                Some(data) => {
                    run(click_warm(&lua, &renderer, data, &ctx, row, deadline).boxed_local()).await
                }
                None => {
                    let clicks: Vec<usize> = earlier.into_iter().chain([row]).collect();
                    run(replay(&lua, &renderer, &call, &ctx, &clicks, deadline).boxed_local()).await
                }
            };
            answer(&lua, &renderer, &call, drawn, &reply);
        });
        Box::pin(async {})
    }
}

/// The renderer registered for {call}'s tool. Without one, or for a call
/// whose output a hook changed, the default draws and no renderer sees the
/// call.
fn renderer_for(lua: &Lua, call: &RenderCall, reply: &Sender<RenderReply>) -> Option<Renderer> {
    let renderer = (!call.data.rewritten)
        .then(|| find_renderer(lua, &call.data.tool))
        .flatten();
    if renderer.is_none() {
        let _ = reply.send(RenderReply::Default);
    }
    renderer
}

fn drawn_by(renderer: &Renderer, call: &RenderCall) -> DrawnBy {
    DrawnBy {
        plugin: Arc::clone(&renderer.plugin),
        tool: Arc::clone(&call.data.tool),
        version: call.data.version,
    }
}

/// Sends what a draw or a click drew, and keeps the call's data warm. A
/// failure draws the default, and its data, half changed, is not kept.
fn answer(
    lua: &Lua,
    renderer: &Renderer,
    call: &RenderCall,
    drawn: Result<LuaResult<(Table, RenderReply)>, &'static str>,
    reply: &Sender<RenderReply>,
) {
    let failure = match drawn {
        Ok(Ok((data, drawn))) => {
            warm(lua, &call.id, drawn_by(renderer, call), &data);
            let _ = reply.send(drawn);
            return;
        }
        Ok(Err(e)) => strip_traceback(&e),
        Err(reason) => reason.to_owned(),
    };
    evict(lua, &call.id);
    tracing::warn!(
        id = call.id,
        plugin = %renderer.plugin,
        tool = %call.data.tool,
        version = call.data.version,
        error = %failure,
        "renderer failed, drawing the default"
    );
    let _ = reply.send(RenderReply::Default);
}

/// Draws {call}, then takes {clicks} in order as a warm call would have:
/// each click, then a draw, so a click reads the layout the draw before it
/// left on the data.
async fn replay(
    lua: &Lua,
    renderer: &Renderer,
    call: &RenderCall,
    ctx: &RenderCtx,
    clicks: &[usize],
    deadline: Instant,
) -> LuaResult<(Table, RenderReply)> {
    let inline = InlineTasks::hold(lua)?;
    let data = call_to_lua(lua, &call.data, call.output.as_deref(), call.status)?;
    for (path, text) in &call.streamed {
        if let Some(streamer) = call_at(&data, path)? {
            streamer.set(STREAM_KEY, text.as_str())?;
        }
    }
    let ctx = ctx_to_lua(lua, ctx)?;
    let mut drawn = draw(&inline, renderer, &data, &ctx, deadline).await?;
    for &row in clicks {
        drawn = click(&inline, renderer, &data, &ctx, row, deadline).await?;
    }
    Ok((data, drawn))
}

async fn click_warm(
    lua: &Lua,
    renderer: &Renderer,
    data: Table,
    ctx: &RenderCtx,
    row: usize,
    deadline: Instant,
) -> LuaResult<(Table, RenderReply)> {
    let inline = InlineTasks::hold(lua)?;
    let ctx = ctx_to_lua(lua, ctx)?;
    let drawn = click(&inline, renderer, &data, &ctx, row, deadline).await?;
    Ok((data, drawn))
}

/// Runs the renderer's parts over {data} and its inline tasks, then reads
/// what they drew.
async fn draw(
    inline: &InlineTasks,
    renderer: &Renderer,
    data: &Table,
    ctx: &Table,
    deadline: Instant,
) -> LuaResult<RenderReply> {
    let header = match &renderer.header {
        Some(header) => Some(header.call_async::<LuaValue>((data, ctx)).await?),
        None => None,
    };
    let body = match &renderer.body {
        Some(body) => Some(body.call_async::<LuaValue>((data, ctx)).await?),
        None => None,
    };
    inline.run(deadline).await;
    Ok(RenderReply::Drawn {
        header: header.map(header_snapshot).transpose()?.flatten(),
        body: body
            .map(|body| buf_snapshot(body, BODY_RETURN_ERR))
            .transpose()?
            .flatten(),
    })
}

/// Changes the renderer's state on {data}, then draws the call again from
/// it. What `click` returns is ignored.
async fn click(
    inline: &InlineTasks,
    renderer: &Renderer,
    data: &Table,
    ctx: &Table,
    row: usize,
    deadline: Instant,
) -> LuaResult<RenderReply> {
    if let Some(click) = &renderer.click {
        click.call_async::<()>((data, ctx, row)).await?;
        inline.run(deadline).await;
    }
    draw(inline, renderer, data, ctx, deadline).await
}

/// The `maki.async.run` work of one draw or click, queued on its scope so
/// it lands in the buffers before they are read, as the bridge's restore
/// does. Whatever is still queued when it drops, after a raise, a timeout
/// or the last round, gives its function back.
struct InlineTasks {
    lua: Lua,
    scope: TaskHandle,
}

impl InlineTasks {
    fn hold(lua: &Lua) -> LuaResult<Self> {
        let scope = try_active_task(lua)?;
        lock_cell(&scope).inline_spawn = Some(Vec::new());
        Ok(Self {
            lua: lua.clone(),
            scope,
        })
    }

    fn take(&self) -> Vec<PendingAsyncTask> {
        lock_cell(&self.scope)
            .inline_spawn
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// Runs the queued tasks under {deadline}. A task may queue more, hence
    /// the rounds.
    async fn run(&self, deadline: Instant) {
        for _ in 0..INLINE_TASK_ROUNDS {
            let round: Vec<Queued<'_>> = self
                .take()
                .into_iter()
                .map(|task| Queued {
                    lua: &self.lua,
                    task: Some(task),
                })
                .collect();
            if round.is_empty() {
                return;
            }
            for task in round {
                task.run(deadline).await;
            }
        }
    }
}

pub(crate) async fn finish_inline(lua: &Lua) {
    let Ok(scope) = try_active_task(lua) else {
        return;
    };
    let deadline = {
        let cell = lock_cell(&scope);
        cell.inline_spawn.as_ref().and(cell.deadline.get())
    };
    if let Some(deadline) = deadline {
        InlineTasks {
            lua: lua.clone(),
            scope,
        }
        .run(deadline)
        .await;
    }
}

impl Drop for InlineTasks {
    fn drop(&mut self) {
        for task in self.take() {
            self.lua.remove_registry_value(task.work_fn).ok();
        }
    }
}

/// A task taken off the queue. Its function goes back to the registry when
/// it drops, whether it ran, ran out of time or never started.
struct Queued<'a> {
    lua: &'a Lua,
    task: Option<PendingAsyncTask>,
}

impl Queued<'_> {
    async fn run(mut self, deadline: Instant) {
        let lua = self.lua;
        let Some(task) = self
            .task
            .as_mut()
            .filter(|task| !task.cancel.is_cancelled())
        else {
            return;
        };
        let mut cell = TaskCell::new(task.cancel.clone(), Some(deadline));
        cell.exts = std::mem::take(&mut task.exts);
        cell.lineage = task.lineage.clone();
        let handle = cell.into_handle();
        let run = run_work_fn(lua, &task.work_fn, &handle);
        if let Err(e) = Narrowed::under(lua, task.ceiling.clone(), run).await {
            tracing::debug!(error = %strip_traceback(&e), "renderer inline task failed");
        }
    }
}

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            self.lua.remove_registry_value(task.work_fn).ok();
        }
    }
}

fn header_snapshot(value: LuaValue) -> LuaResult<Option<BufferSnapshot>> {
    match value {
        LuaValue::String(text) => Ok(Some(BufferSnapshot::plain_text(text.to_str()?.to_owned()))),
        other => buf_snapshot(other, HEADER_RETURN_ERR),
    }
}

fn buf_snapshot(value: LuaValue, err: &'static str) -> LuaResult<Option<BufferSnapshot>> {
    match value {
        LuaValue::Nil => Ok(None),
        LuaValue::UserData(ud) => {
            let handle = ud
                .borrow::<BufHandle>()
                .map_err(|_| mlua::Error::runtime(err))?;
            Ok(Some(handle.buf.take()))
        }
        _ => Err(mlua::Error::runtime(err)),
    }
}

/// The table of the call at {path} under {data}, by invocation index, or
/// `None` when no record of it arrived yet.
fn call_at(data: &Table, path: &[u32]) -> LuaResult<Option<Table>> {
    let mut table = data.clone();
    for &index in path {
        let children: Table = table.get(CHILDREN_KEY)?;
        let mut found = None;
        for child in children.sequence_values::<Table>() {
            let child = child?;
            if child.get::<i64>(INDEX_KEY)? == i64::from(index) + 1 {
                found = Some(child);
                break;
            }
        }
        match found {
            Some(child) => table = child,
            None => return Ok(None),
        }
    }
    Ok(Some(table))
}

fn ctx_to_lua(lua: &Lua, ctx: &RenderCtx) -> LuaResult<Table> {
    let table = lua.create_table()?;
    table.set(CWD_KEY, ctx.cwd.display().to_string())?;
    table.set(
        HOME_KEY,
        ctx.home.as_ref().map(|home| home.display().to_string()),
    )?;
    table.set(TOOL_OUTPUT_LINES_KEY, lua.to_value(&ctx.tool_output_lines)?)?;
    table.set(WIDTH_KEY, ctx.width)?;
    table.set(REASON_KEY, <&'static str>::from(ctx.reason))?;
    Ok(table)
}

#[cfg(test)]
mod tests {
    use mlua::Function;

    use super::*;
    use crate::runtime::{Lineage, TaskExts, TaskLifetime, TaskScope};

    const WEAK_TABLE: &str = "return setmetatable({}, { __mode = 'v' })";

    /// The queued function is held only by its task and a weak table, so it
    /// is collected once the dropped queue gives it back.
    #[test]
    fn a_dropped_queue_gives_back_its_inline_tasks() {
        let lua = Lua::new();
        let weak: Table = lua.load(WEAK_TABLE).eval().unwrap();
        let work: Function = lua.create_function(|_, ()| Ok(())).unwrap();
        weak.raw_set(1, &work).unwrap();
        let scope = TaskScope::detached(&lua);
        let active = scope.enter();
        let inline = InlineTasks::hold(&lua).unwrap();
        lock_cell(scope.handle())
            .inline_spawn
            .as_mut()
            .unwrap()
            .push(PendingAsyncTask {
                work_fn: lua.create_registry_value(work).unwrap(),
                cancel: CancelToken::none(),
                deadline: None,
                exts: TaskExts::default(),
                lineage: Lineage::default(),
                ceiling: None,
                lifetime: TaskLifetime::Held,
            });
        drop(inline);
        drop(active);
        lua.gc_collect().unwrap();
        assert_eq!(weak.raw_get::<LuaValue>(1).unwrap(), LuaValue::Nil);
    }
}
