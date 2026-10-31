use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::agent::tool_dispatch::silent_mutation_check;
use maki_agent::cancel::CancelToken;
use maki_agent::tools::{
    Deadline, FileKey, MAIN_TASK_ID, ProgressScope, ToolAudience, ToolContext, ToolLive,
    ToolOutputMode,
};
use maki_agent::{AgentEvent, AgentSeed, Envelope, SessionMailbox, ToolPart};
use maki_config::{AgentConfig, ToolOutputLines};
use maki_storage::id::{MakiId, SessionRef};
use maki_types::paths::HOME;
use mlua::{
    Function, Lua, LuaSerdeExt, MultiValue, Result as LuaResult, Table, UserData, UserDataMethods,
    Value as LuaValue,
};

use crate::api::tool::ToolCallReply;
use crate::api::util::convert::{TableExt, json_to_lua, lua_to_json};
use crate::api::util::pair::{Pair, err_pair, pair};
use crate::domains::agent::moves::{AgentTask, MoveOutcome, SUBAGENT_SELF_MOVE_MSG, move_self};
use crate::runtime::{active_task, lock_cell};

const DEADLINE_ALREADY_SET_MSG: &str = "ctx:set_deadline() already called";
const STREAM_NOT_STRING_MSG: &str = "ctx:stream(text) expects a string";
const SET_STATE_CONTEXT: &str = "ctx:set_state(value)";
const WRITE_TOOL: &str = "write";
const BACKGROUND_HOOK_ALREADY_SET_MSG: &str = "ctx:on_background() already called";
const NOT_A_MODEL_CALL_MSG: &str =
    "on_background: only the handler of a model's tool call can move to the background";
/// Model-facing: bash and task quote it when `background = true` cannot be
/// honoured.
const NESTED_CALL_MSG: &str = "background is ignored for a call inside batch or another \
     tool; call the tool directly to run it in the background";
const NO_BACKGROUND_HOOK_MSG: &str = "set_movable: call ctx:on_background() first";
const NO_BACKGROUND_HOOK_FOR_MOVE_MSG: &str = "background: call ctx:on_background() first";
const NESTED_PARTS_MSG: &str =
    "set_parts: only a model's own call is a task, a call nested in another has no parts";
const PART_STATUS_ERR: &str =
    "set_parts: status must be \"working\", \"done\", \"error\" or \"cancelled\"";
const EXPIRE_BACKGROUND: &str = "background";
const EXPIRE_TIMEOUT: &str = "timeout";

/// What `ctx:set_deadline` does when the deadline passes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DeadlineExpire {
    #[default]
    Timeout,
    /// Move the call to the background instead, falling back to a timeout
    /// when it cannot move.
    Background,
}

impl DeadlineExpire {
    fn parse(opts: Option<&Table>) -> mlua::Result<Self> {
        let Some(opts) = opts else {
            return Ok(Self::Timeout);
        };
        match opts.opt::<String>("expire")?.as_deref() {
            None | Some(EXPIRE_TIMEOUT) => Ok(Self::Timeout),
            Some(EXPIRE_BACKGROUND) => Ok(Self::Background),
            Some(other) => Err(mlua::Error::runtime(format!(
                "set_deadline: unknown expire {other:?}, expected \"{EXPIRE_TIMEOUT}\" or \"{EXPIRE_BACKGROUND}\""
            ))),
        }
    }
}

const CWD_KEY: &str = "cwd";
const HOME_KEY: &str = "home";

fn could_move(agent: &AgentContext) -> bool {
    agent.mailbox.is_some()
        && agent
            .session_id
            .as_ref()
            .is_some_and(|session| AgentSeed::live(session.id()).is_some())
}

fn announce_movable(agent: &AgentContext, movable: bool) {
    if let Some(id) = &agent.tool_use_id {
        if could_move(agent) {
            agent.send_event(AgentEvent::ToolMovable {
                id: id.clone(),
                movable,
            });
        }
    } else if let Some(sink) = &agent.live_sink {
        let _ = sink.send(ToolLive::Movable(movable));
    }
}

/// Captured snapshot of the parent `ToolContext`. Per-call state (deadline,
/// output lines) is reset so child calls start clean. The instruction handles
/// stay: a nested call is still the same session and the same model call.
///
/// Routing state is kept verbatim, `local_tools` included: a name a Lua tool
/// dispatches must land where the same name lands when the model calls it, or
/// shadowing quietly inverts for nested calls. A child session does not inherit
/// them anyway, `session()` always sets its own.
#[derive(Clone)]
pub(crate) struct AgentContext(ToolContext);

impl From<&ToolContext> for AgentContext {
    fn from(ctx: &ToolContext) -> Self {
        let mut c = ctx.clone();
        c.deadline = Deadline::default();
        c.tool_output_lines = ToolOutputLines::default();
        Self(c)
    }
}

impl Deref for AgentContext {
    type Target = ToolContext;
    fn deref(&self) -> &ToolContext {
        &self.0
    }
}

impl AgentContext {
    fn send_event(&self, event: AgentEvent) {
        if let Some(home) = self.call.as_ref().and_then(|call| call.home()) {
            SessionMailbox::task_event(
                home.chat,
                Arc::clone(&home.plugin),
                Envelope {
                    event,
                    subagent: None,
                    run_id: 0,
                },
            );
        } else {
            let _ = self.event_tx.send(event);
        }
    }

    /// Drops `tool_use_id` so an inner tool never emits UI events under the
    /// outer call's id, `live_sink` so a grandchild never streams into a sink
    /// meant for its parent, and `progress`, which only a dispatched child
    /// gets, as its own scope: see [`Self::child_progress`].
    pub(crate) fn to_tool_context(&self) -> ToolContext {
        let mut c = self.0.clone();
        c.tool_use_id = None;
        c.live_sink = None;
        c.progress = None;
        c
    }

    /// The scope of the next call this handler dispatches, recorded under
    /// it with the next invocation index.
    pub(crate) fn child_progress(&self) -> Option<ProgressScope> {
        self.0.progress.as_ref().map(ProgressScope::child)
    }
}

pub(crate) struct LuaCtx {
    agent: Box<AgentContext>,
    pub(crate) cancel: CancelToken,
    tool_output_lines: ToolOutputLines,
    /// Which chat the run serves, see [`ToolContext::session_id`] and
    /// [`ToolContext::task_id`].
    session_id: Option<SessionRef>,
    task_id: Option<Arc<str>>,
    pub(crate) finish_tx: Option<flume::Sender<ToolCallReply>>,
}

struct SessionPlace {
    cwd: PathBuf,
    home: Option<PathBuf>,
}

impl LuaCtx {
    fn new(ctx: &ToolContext, agent: AgentContext) -> Self {
        Self {
            agent: Box::new(agent),
            cancel: ctx.cancel.clone(),
            tool_output_lines: ctx.tool_output_lines,
            session_id: ctx.session_id.clone(),
            task_id: ctx.task_id.clone(),
            finish_tx: None,
        }
    }

    pub(crate) fn handler(ctx: &ToolContext) -> Self {
        Self::new(ctx, AgentContext::from(ctx))
    }

    /// A handler ctx for code that runs beside a call rather than as it, such
    /// as a permission layer: what it streams or dispatches is no part of the
    /// call, so it gets no progress scope.
    pub(crate) fn detached_handler(ctx: &ToolContext) -> Self {
        let mut agent = AgentContext::from(ctx);
        agent.0.progress = None;
        Self::new(ctx, agent)
    }

    pub(crate) fn session(&self) -> Option<MakiId> {
        self.session_id.as_ref().map(SessionRef::id)
    }

    /// Dispatch capability: only handler ctxs can call `maki.agent.*`.
    pub(crate) fn agent(&self) -> Option<&AgentContext> {
        Some(&self.agent)
    }

    fn config(&self) -> Option<&AgentConfig> {
        Some(&self.agent.config)
    }

    fn workflow(&self) -> Option<bool> {
        Some(self.agent.workflow)
    }

    fn audience(&self) -> Option<ToolAudience> {
        Some(self.agent.audience)
    }

    fn output_mode(&self) -> ToolOutputMode {
        self.agent.output_mode
    }

    fn task_id(&self) -> &str {
        self.task_id.as_deref().unwrap_or(MAIN_TASK_ID)
    }

    /// `None` outside a model call and its nested calls, where nothing
    /// streams.
    fn progress(&self) -> Option<&ProgressScope> {
        self.agent()?.progress.as_ref()
    }

    /// A call runs in its session's working directory, with the agent's
    /// home. A restore draws a call for the UI, which knows the session it
    /// belongs to.
    fn place(&self) -> Option<SessionPlace> {
        let cwd = &*self.agent.working_dir;
        Some(SessionPlace {
            cwd: cwd.to_path_buf(),
            home: HOME.clone(),
        })
    }

    fn state(&self) -> Option<&serde_json::Value> {
        None
    }

    fn kind(&self) -> &'static str {
        "handler"
    }

    pub(crate) fn cap_err(&self, method: &str) -> String {
        format!("{method} not available in {} ctx", self.kind())
    }

    pub(crate) fn cap_err_pair<T>(&self, method: &str) -> Pair<T> {
        (None, Some(self.cap_err(method)))
    }
}

impl UserData for LuaCtx {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancelled", |_, this, ()| Ok(this.cancel.is_cancelled()));

        methods.add_method("output_mode", |_, this, ()| Ok(this.output_mode().as_str()));

        methods.add_method("workflow", |_, this, ()| {
            let Some(workflow) = this.workflow() else {
                return Ok(this.cap_err_pair("workflow"));
            };
            Ok((Some(workflow), None))
        });

        methods.add_method("audience", |_, this, ()| {
            let Some(audience) = this.audience() else {
                return Ok(this.cap_err_pair("audience"));
            };
            Ok((Some(audience.name().unwrap_or("main").to_string()), None))
        });

        // The session that called this tool, or whose transcript a restore
        // re-renders, which under concurrent sessions is not always the
        // focused one `maki.session.current()` reports. Nil when the run has
        // no session, as in the `maki index` one-shot.
        methods.add_method("session_id", |_, this, ()| {
            Ok(this.session_id.as_ref().map(|id| id.id().to_string()))
        });

        methods.add_method("task_id", |_, this, ()| Ok(this.task_id().to_owned()));

        methods.add_method("config", |lua, this, args: MultiValue| {
            let Some(config) = this.config() else {
                return Ok(this.cap_err_pair("config"));
            };
            let config_val = lua.to_value(config)?;
            if args.is_empty() {
                return Ok((Some(config_val), None));
            }
            let key: String = lua.from_value(args[0].clone())?;
            let default = args.get(1).cloned().unwrap_or(LuaValue::Nil);
            let val = match config_val {
                LuaValue::Table(ref tbl) => {
                    let val = tbl.raw_get::<LuaValue>(key.as_str())?;
                    if matches!(val, LuaValue::Nil) {
                        default
                    } else {
                        val
                    }
                }
                _ => default,
            };
            Ok((Some(val), None))
        });

        methods.add_method("tool_output_lines", |lua, this, ()| {
            lua.to_value(&this.tool_output_lines)
        });

        // The session's directories, for shortening paths as the session
        // sees them: `{ cwd, home }`, `home` nil when unknown, or nil for a
        // restore the UI gave no place.
        methods.add_method("place", |lua, this, ()| {
            this.place()
                .map(|place| place_to_lua(lua, &place))
                .transpose()
        });

        methods.add_method("state", |lua, this, ()| match this.state() {
            Some(v) => json_to_lua(lua, v),
            None => Ok(LuaValue::Nil),
        });

        methods.add_method("tool_use_id", |_, this, ()| {
            Ok(this.agent.tool_use_id.clone())
        });

        methods.add_method(
            "on_background",
            |lua, this, hook: Function| -> mlua::Result<Pair<bool>> {
                let Some(agent) = this.agent() else {
                    return Ok(this.cap_err_pair("on_background"));
                };
                let Some(call) = &agent.call else {
                    return Ok(err_pair(NOT_A_MODEL_CALL_MSG));
                };
                let handle = active_task(lua);
                {
                    let cell = lock_cell(&handle);
                    if !cell
                        .exts
                        .get::<AgentTask>()
                        .and_then(|task| task.call.as_ref())
                        .is_some_and(|own| Arc::ptr_eq(own, call))
                    {
                        return Ok(err_pair(NOT_A_MODEL_CALL_MSG));
                    }
                    if cell
                        .exts
                        .get::<AgentTask>()
                        .is_some_and(|task| task.background_hook.is_some())
                    {
                        return Err(mlua::Error::runtime(BACKGROUND_HOOK_ALREADY_SET_MSG));
                    }
                }
                // Made outside the lock: allocating can run the GC, and a
                // finalizer may want the same cell.
                let key = lua.create_registry_value(hook)?;
                lock_cell(&handle)
                    .exts
                    .get_or_insert_with(AgentTask::default)
                    .background_hook = Some(key);
                announce_movable(agent, true);
                Ok((Some(true), None))
            },
        );

        // A hook that would decline right now (a batch waiting on an edit)
        // hides the hint until it would accept again.
        methods.add_method("set_movable", |lua, this, movable: bool| {
            let Some(agent) = this.agent() else {
                return Ok(this.cap_err_pair("set_movable"));
            };
            let handle = active_task(lua);
            if lock_cell(&handle)
                .exts
                .get::<AgentTask>()
                .is_none_or(|task| task.background_hook.is_none())
            {
                return Ok(err_pair(NO_BACKGROUND_HOOK_MSG));
            }
            announce_movable(agent, movable);
            Ok((Some(true), None))
        });

        // Parts hang under the model's own call, the only call with a tab in
        // `/tasks`. A nested call has no tab, so it gets an error instead.
        methods.add_method("set_parts", |lua, this, parts: Vec<Table>| {
            let Some(agent) = this.agent() else {
                return Ok(this.cap_err_pair("set_parts"));
            };
            let Some(id) = agent.tool_use_id.clone() else {
                return Ok(err_pair(NESTED_PARTS_MSG));
            };
            let parts = parts
                .iter()
                .map(|part| {
                    Ok(ToolPart {
                        id: part.get::<String>("id")?,
                        name: part.get::<String>("name")?,
                        tool: part.get::<String>("tool")?.into(),
                        status: lua
                            .from_value(part.get::<LuaValue>("status")?)
                            .map_err(|_| mlua::Error::runtime(PART_STATUS_ERR))?,
                        body: None,
                    })
                })
                .collect::<mlua::Result<Vec<_>>>()?;
            agent.send_event(AgentEvent::ToolParts { id, parts });
            Ok((Some(true), None))
        });

        // The call moves itself through its own `on_background` hook, as if
        // the user pressed Ctrl+B. Gated on the plugin's `agent` permission
        // by the runtime.
        methods.add_async_method("background", |lua, this, ()| async move {
            let Some(agent) = this.agent() else {
                return Ok(this.cap_err_pair("background"));
            };
            if agent.task_id.is_some() {
                return Ok(err_pair(SUBAGENT_SELF_MOVE_MSG));
            }
            let Some(call) = agent.call.clone() else {
                return Ok(err_pair(NOT_A_MODEL_CALL_MSG));
            };
            // A nested call shares its parent's scope but has no id of its own.
            let Some(id) = agent.tool_use_id.clone() else {
                return Ok(err_pair(NESTED_CALL_MSG));
            };
            // Nothing may hold the ctx borrow across the wait: the hook runs
            // meanwhile and may call ctx methods.
            drop(this);
            let outcome = move_self(&lua, &id).await;
            // Moved by a Ctrl+B press, before or during the wait: done all
            // the same.
            if call.home().is_some() {
                return Ok((Some(true), None));
            }
            Ok(match outcome {
                MoveOutcome::Moved => (Some(true), None),
                MoveOutcome::NoHook => err_pair(NO_BACKGROUND_HOOK_FOR_MOVE_MSG),
                MoveOutcome::NotMovable(reason) => (None, Some(reason)),
            })
        });

        methods.add_method("backgrounded", |_, this, ()| {
            Ok(this
                .agent()
                .and_then(|agent| agent.call.as_ref())
                .is_some_and(|call| call.home().is_some()))
        });

        methods.add_method(
            "set_deadline",
            |lua, _this, (secs, opts): (u64, Option<Table>)| {
                let expire = DeadlineExpire::parse(opts.as_ref())?;
                let handle = active_task(lua);
                let cell = lock_cell(&handle);
                if cell.deadline_secs.get().is_some() {
                    return Err(mlua::Error::runtime(DEADLINE_ALREADY_SET_MSG));
                }
                // Only a model call can move, anything else just times out.
                if let Some(task) = cell.exts.get::<AgentTask>()
                    && task.call.is_some()
                {
                    task.deadline_expire.set(expire);
                }
                cell.deadline_secs.set(Some(secs));
                cell.deadline
                    .set(Some(Instant::now() + Duration::from_secs(secs)));
                cell.deadline_changed.notify(usize::MAX);
                Ok((Some(true), None::<String>))
            },
        );

        methods.add_method("stream", |_, this, text: LuaValue| {
            let LuaValue::String(text) = text else {
                return Err(mlua::Error::runtime(format!(
                    "{STREAM_NOT_STRING_MSG}, got {}",
                    text.type_name()
                )));
            };
            if let Some(scope) = this.progress() {
                scope.stream(&text.to_str()?);
            }
            Ok(())
        });

        methods.add_method("set_state", |lua, this, value: LuaValue| {
            let state = lua_to_json(lua, &value)
                .map_err(|e| mlua::Error::runtime(format!("{SET_STATE_CONTEXT}: {e}")))?;
            if let Some(scope) = this.progress() {
                scope.set_state(state);
            }
            Ok(())
        });

        // The matching check before a write is not exposed: the dispatcher
        // runs it under the file lock for every tool declaring `mutable_path`,
        // and a handler-side copy would race its own sibling.
        methods.add_method("record_read", |_, this, path: String| {
            let Some(agent) = this.agent() else {
                return Ok(this.cap_err_pair("record_read"));
            };
            agent
                .file_access
                .record_read(&FileKey::new(&agent.resolve_path(Path::new(&path))));
            Ok((Some(true), None::<String>))
        });

        // A plugin that changes the disk itself has no tool call to hang a
        // prompt on, so this says yes only where the write tool would run
        // without asking anyone.
        methods.add_method("can_write", |_, this, path: String| {
            let Some(agent) = this.agent() else {
                return Ok(this.cap_err_pair("can_write"));
            };
            Ok(pair(
                silent_mutation_check(agent, WRITE_TOOL, &path).map(|()| true),
            ))
        });

        methods.add_async_method(
            "load_instructions",
            |_, this, dir_path: String| async move {
                let Some(agent) = this.agent() else {
                    return Ok(this.cap_err_pair("load_instructions"));
                };
                let cwd = Arc::clone(&agent.working_dir);
                let loaded = agent.loaded_instructions.clone();
                let call = agent.call_instructions.clone();
                let files = Arc::clone(&agent.instruction_files);
                // Nothing may hold the ctx borrow across the wait: a cancel
                // hook firing meanwhile needs `ctx:finish`, which takes it
                // mutably.
                drop(this);
                let blocks = tokio::task::spawn_blocking(move || {
                    let abs = resolve_abs_with_cwd(dir_path, &cwd);
                    maki_agent::find_subdirectory_instructions(&abs, &cwd, &loaded, &files)
                })
                .await
                .map_err(mlua::Error::external)?;
                call.record(blocks);
                Ok((Some(true), None::<String>))
            },
        );

        methods.add_method("is_instruction_file", |_, this, name: String| {
            Ok(this.agent().is_some_and(|agent| {
                maki_agent::is_instruction_file(&agent.instruction_files, &name)
            }))
        });

        methods.add_method_mut("finish", |lua, this, val: LuaValue| {
            let tx = this
                .finish_tx
                .take()
                .ok_or_else(|| mlua::Error::runtime("ctx:finish() already called"))?;

            let _ = tx.send(ToolCallReply::from_lua_value(lua, &val));
            Ok((Some(true), None::<String>))
        });
    }
}

fn place_to_lua(lua: &Lua, place: &SessionPlace) -> LuaResult<mlua::Table> {
    let table = lua.create_table()?;
    table.set(CWD_KEY, place.cwd.display().to_string())?;
    table.set(
        HOME_KEY,
        place.home.as_ref().map(|home| home.display().to_string()),
    )?;
    Ok(table)
}

fn resolve_abs_with_cwd(path: String, cwd: &Path) -> PathBuf {
    if Path::new(&path).is_absolute() {
        path.into()
    } else {
        cwd.join(&path)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use maki_agent::tools::test_support::stub_ctx_with;
    use maki_agent::tools::{LocalTool, ToolAudience};
    use maki_agent::{AgentMode, InstructionBlock};
    use test_case::test_case;

    use super::*;

    const TOOL_USE_ID: &str = "tu-1";
    const INSTRUCTION_PATH: &str = "/tmp/nested/AGENTS.md";
    const LOCAL_TOOL_NAME: &str = "sess_tool";
    /// Arbitrary ids are rejected: `SessionRef` parses base58 or a uuid.
    const SESSION_ID: &str = "01965087-4c71-7f00-8000-000000000000";

    fn session_ref() -> SessionRef {
        SESSION_ID.parse().expect("valid session id")
    }

    fn populated_ctx() -> ToolContext {
        let mut ctx = stub_ctx_with(&AgentMode::Build, None, Some(TOOL_USE_ID));
        ctx.session_id = Some(session_ref());
        ctx.deadline = Deadline::after(Duration::from_secs(60));
        ctx.tool_output_lines = ToolOutputLines {
            bash: 999,
            ..ToolOutputLines::default()
        };
        assert!(
            !ctx.loaded_instructions
                .contains_or_insert(PathBuf::from(INSTRUCTION_PATH))
        );
        ctx.call_instructions.record(vec![InstructionBlock {
            path: INSTRUCTION_PATH.into(),
            content: String::new(),
        }]);
        let mut tools: HashMap<String, LocalTool> = HashMap::new();
        tools.insert(
            LOCAL_TOOL_NAME.into(),
            maki_agent::tools::local_tool(ToolAudience::MODEL, |_, _| {
                Box::pin(async { Ok(String::new()) })
            }),
        );
        ctx.local_tools = Arc::new(tools);
        ctx.live_sink = Some(flume::unbounded().0);
        ctx
    }

    #[test]
    fn agent_context_keeps_tool_use_id_and_resets_per_call_state() {
        let agent = AgentContext::from(&populated_ctx());
        assert_eq!(agent.tool_use_id.as_deref(), Some(TOOL_USE_ID));
        assert_eq!(
            agent.session_id,
            Some(session_ref()),
            "the session owns the whole run, so it is not per-call state"
        );
        assert!(agent.deadline.at().is_none());
        assert_eq!(agent.tool_output_lines, ToolOutputLines::default());
        assert!(
            agent.local_tools.contains_key(LOCAL_TOOL_NAME),
            "a nested call must route names exactly like the model's own call"
        );
        assert!(
            agent
                .loaded_instructions
                .contains_or_insert(PathBuf::from(INSTRUCTION_PATH)),
            "session state, shared with nested calls"
        );
        assert_eq!(
            agent.call_instructions.take().len(),
            1,
            "per model call, shared with nested calls"
        );
    }

    #[test]
    fn agent_context_to_tool_context_drops_tool_use_id_and_sink() {
        let agent = AgentContext::from(&populated_ctx());
        assert!(
            agent.live_sink.is_some(),
            "the sink set by the caller must survive into AgentContext"
        );
        let inner = agent.to_tool_context();
        assert_eq!(inner.tool_use_id, None);
        assert!(inner.live_sink.is_none(), "sink must not be inherited");
        assert_eq!(agent.tool_use_id.as_deref(), Some(TOOL_USE_ID));
        assert_eq!(
            inner.session_id,
            Some(session_ref()),
            "a dispatched child runs in the same session, unlike tool_use_id"
        );
    }

    const CTX_GLOBAL: &str = "ctx";
    const STREAM_TEXT: &str = "chunk";
    const BAD_STREAM_ARGS: &str = "ctx:stream(42)";
    const MISSING_STREAM_ARG: &str = "ctx:stream()";
    const UNCONVERTIBLE_STATE: &str = "ctx:set_state({ f = print })";
    const STATE_FIELD: &str = "n";
    const STATE_VALUE: u64 = 1;

    fn valid_calls() -> String {
        format!(r#"ctx:stream("{STREAM_TEXT}") ctx:set_state({{ {STATE_FIELD} = {STATE_VALUE} }})"#)
    }

    fn run_on(lua_ctx: LuaCtx, chunk: &str) -> mlua::Result<()> {
        let lua = mlua::Lua::new();
        lua.globals().set(CTX_GLOBAL, lua_ctx)?;
        lua.load(chunk).exec()
    }

    #[test_case(BAD_STREAM_ARGS,     STREAM_NOT_STRING_MSG ; "stream_number")]
    #[test_case(MISSING_STREAM_ARG,  STREAM_NOT_STRING_MSG ; "stream_nil")]
    #[test_case(UNCONVERTIBLE_STATE, SET_STATE_CONTEXT     ; "state_with_a_function")]
    fn stream_api_raises_on_bad_args(chunk: &str, expected: &str) {
        let mut ctx = populated_ctx();
        ctx.progress = Some(ProgressScope::default());
        let err = run_on(LuaCtx::handler(&ctx), chunk).unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
    }

    /// A nested call streams under its own path, numbered in the order its
    /// parent dispatched it.
    #[test_case(0, &[]     ; "model_call")]
    #[test_case(1, &[0]    ; "first_child")]
    #[test_case(2, &[0, 0] ; "grandchild")]
    fn stream_api_fills_the_scope_of_the_call(depth: usize, path: &[u32]) {
        let mut ctx = populated_ctx();
        let root = ProgressScope::default();
        ctx.progress = Some(root.clone());
        for _ in 0..depth {
            let agent = AgentContext::from(&ctx);
            ctx = ToolContext {
                progress: agent.child_progress(),
                ..agent.to_tool_context()
            };
        }
        run_on(LuaCtx::handler(&ctx), &valid_calls()).unwrap();
        let taken = root.take();
        assert_eq!(taken.len(), 1, "{taken:?}");
        assert_eq!(taken[0].path, path);
        assert_eq!(taken[0].append, STREAM_TEXT);
        assert_eq!(
            taken[0].state,
            Some(serde_json::json!({ STATE_FIELD: STATE_VALUE }))
        );
    }

    #[test]
    fn sibling_children_take_the_next_index() {
        let mut ctx = populated_ctx();
        let root = ProgressScope::default();
        ctx.progress = Some(root.clone());
        let agent = AgentContext::from(&ctx);
        for _ in 0..2 {
            let child = agent.child_progress().expect("a child scope");
            child.stream(STREAM_TEXT);
        }
        let paths: Vec<_> = root.take().into_iter().map(|p| p.path).collect();
        assert_eq!(paths, [vec![0], vec![1]]);
        assert!(
            agent.to_tool_context().progress.is_none(),
            "only a dispatched child gets a scope"
        );
    }
}
