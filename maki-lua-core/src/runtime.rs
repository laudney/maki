use std::any::Any;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::{CStr, c_int};
use std::fmt::Display;
use std::future::{Future, poll_fn};
use std::io::Result as IoResult;
use std::panic::catch_unwind;
use std::path::{Path, PathBuf};
use std::pin::{Pin, pin};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use event_listener::Event;

use mlua::chunk::{Chunk, ChunkMode, Compiler};
use mlua::{Function, Lua, MaybeSend, MultiValue, RegistryKey, Table, Value as LuaValue, ffi};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::task::LocalSet;

use maki_storage::id::MakiId;
use maki_types::cancel::{CancelMap, CancelSlot, CancelToken};
use maki_types::event::SessionEndReason;

use crate::agent_autocmd::HostEvent;
use crate::api::autocmd::AutocmdStore;
use crate::api::r#fn::{JobEvent, JobOwner, JobStore, deliver_job_event};
use crate::api::options::{PluginOptionSpecs, PluginOpts, collect_plugin_options};
use crate::api::slot::{LayerCost, LayeredTools, SlotLayer, SlotStore};
use crate::api::util::convert::{TableExt, json_to_lua};
use crate::api::util::setup::{ConfigStore, Setup};
use crate::api::{LoadKind, create_maki_global};
use crate::bundled::Bundled;
use crate::docs::{ModuleDoc, compose_docs};
use crate::docs_render;
use crate::domain::{Domain, DomainRequest, LoadScope, Vm};
use crate::error::PluginError;
use crate::host_sink::{host_flash, host_unloaded};
use crate::plugin_permissions::{
    Ceiling, Component, Permission, PluginPermissions, PluginReach, grant_hint,
    load_plugin_permissions,
};

const INTERRUPT_SHUTDOWN_MSG: &str = "plugin interrupted: host shutting down";
const INTERRUPT_CANCELLED_MSG: &str = "plugin interrupted: task cancelled";
const INTERRUPT_DEADLINE_MSG: &str = "plugin interrupted: deadline exceeded";
pub const DISPATCH_POLL_INTERVAL: Duration = Duration::from_millis(50);
pub const CANCELLED_MSG: &str = "cancelled";
pub const OUTSIDE_TASK_ERR: &str = "not available at a plugin's top level: call it from a callback";
pub const HANDLER_TIMEOUT_MSG: &str = "timeout";
pub const MAX_INFLIGHT_TOOLS: usize = 64;
const GC_STEP_INTERVAL: usize = 4;
/// Only sets how fast the one-shot interrupt is re-armed after it fires, so
/// the kill still lands within a poll of [`KILL_GRACE`]. The thread ticks even
/// when no Lua runs, so prefer the slowest interval the grace can hide.
pub const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long a doomed task may run without yielding before the watchdog
/// shoots it. Cleanup after a cancel or a timeout (batch marking its
/// children cancelled, rerendering its buf) is plain Lua running with the
/// interrupt already armed, so killing at the first safepoint strands the
/// UI mid-flight. Every yield hands back a fresh budget, so cleanup may
/// take as long as it needs, while a loop that never yields dies within
/// one grace.
pub const KILL_GRACE: Duration = Duration::from_millis(500);
/// A slice is one poll of a task, the stretch where it holds the one Lua
/// thread and every plugin, keymap and the UI wait behind it. A slice this
/// long gets a log line naming the plugin, so its author knows to yield. The
/// UI waits just as long for a key handler before it lets the next keys go.
pub const SLICE_WARN: Duration = Duration::from_secs(1);
/// A slice this long is raised even with no deadline in sight. A deadline
/// caps how long a task runs in total, this caps how long it starves the rest.
pub const SLICE_KILL: Duration = Duration::from_secs(5);
const UNKNOWN_PLUGIN: &str = "<unknown>";
const UNKNOWN_LOCATION: &str = "?";
/// Wall clock a cancelled handler gets before the host stops waiting for
/// it. The watchdog alone never ends a task that parks in an await: it
/// runs no Lua to interrupt and renews its grace at every yield. Long
/// enough for cleanup that waits on children, short enough that the next
/// prompt is not stuck behind abandoned work.
pub const CANCEL_ABANDON_AFTER: Duration = Duration::from_secs(5);
const OPT_LEVEL_JIT: u8 = 2;
const OPT_LEVEL_DEBUGGABLE: u8 = 1;
const DEBUG_INFO_FULL: u8 = 2;
const ASYNC_RUN_DEFAULT_DEADLINE: Duration = Duration::from_secs(60);
static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);
const TURN_END_EVENT: &str = "TurnEnd";
/// Cap on the last delivery pass a scope makes before it reaps its jobs. A job
/// printing faster than we deliver always has another event queued, so an
/// unbounded pass would never reach the reap.
const FINAL_DRAIN_BUDGET: usize = 256;
const NO_PLUGIN_HOST: &str = "no plugin host is running this code";
const NOT_A_CUSTOM_LOAD: &str = "pack loader: not a custom load";
const SPAWN_QUEUE_NOT_INIT: &str = "spawn queue not initialized";
/// Without a cap, a runaway plugin OOM-kills the whole process.
/// With one, it hits a catchable Lua error instead.
pub(crate) const LUA_MEMORY_LIMIT: usize = 512 * 1024 * 1024;

pub type LoadResult = Result<(), PluginError>;

#[cfg(any(test, feature = "test-support"))]
pub fn block_on<F: Future>(future: F) -> F::Output {
    crate::host::HOST_RUNTIME.block_on(future)
}

/// One source file of a plugin.
///
/// A bundled plugin has one per entry it loads. An external package has one
/// per entry file, and they share a single owner and a single environment, so
/// what one registers the next can see.
#[derive(Debug)]
pub struct LoadChunk {
    /// Names the chunk in Lua errors, so a failure points at the file the user
    /// wrote rather than at the package.
    pub name: String,
    pub source: String,
    origin: ChunkOrigin,
}

/// Where a chunk's source came from, which decides whether it goes through
/// [`load_user_source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkOrigin {
    /// Shipped in the binary and checked by its tests.
    Bundled,
    /// Anything a user can change: a config file, a package, a plugin file.
    User,
}

impl LoadChunk {
    /// Source a user can change, of the agent component. The default, so a
    /// new load path cannot skip the checks user source gets by forgetting to
    /// ask for them.
    pub fn new(name: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source: source.into(),
            origin: ChunkOrigin::User,
        }
    }

    /// Source that ships in the binary.
    pub fn bundled(name: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            origin: ChunkOrigin::Bundled,
            ..Self::new(name, source)
        }
    }
}

/// One step of a load: a chunk, or a custom loader `maki.pack.add` declared
/// for a package, which supplies {component}'s entry in its place.
#[derive(Debug)]
pub enum LoadStep {
    Chunk(LoadChunk),
    Loader {
        declared: crate::api::pack::Declared,
    },
}

impl From<LoadChunk> for LoadStep {
    fn from(chunk: LoadChunk) -> Self {
        Self::Chunk(chunk)
    }
}

thread_local! {
    static PLUGIN_LOADING: Cell<bool> = const { Cell::new(false) };
}

/// Runs a plugin's top level, where every UI roundtrip fails at once. The
/// host blocks on the load and starts the event loop only after every plugin
/// loaded, so an answer would never come. The flag is up only while {load}
/// itself is polled: a task the executor runs while the load is parked, and
/// a callback the load scheduled, still reach the UI.
pub async fn as_plugin_load<F: Future>(load: F) -> F::Output {
    let mut load = pin!(load);
    poll_fn(|cx| {
        let outer = PLUGIN_LOADING.replace(true);
        let polled = load.as_mut().poll(cx);
        PLUGIN_LOADING.set(outer);
        polled
    })
    .await
}

/// Whether this thread is polling a plugin's top level right now.
pub fn in_plugin_load() -> bool {
    PLUGIN_LOADING.get()
}

/// What reads each user source as it enters the VM, such as the UI domain's
/// key lint. Kept in Lua app data by the domain that installs it.
struct UserSourceCheck(Box<SourceCheckFn>);

/// Called with a user source's chunk name and text.
type SourceCheckFn = dyn Fn(&str, &str) + Send;

/// Hands {check} every user source [`load_user_source`] lets in, as its
/// chunk name and text.
pub fn install_user_source_check(lua: &Lua, check: impl Fn(&str, &str) + Send + 'static) {
    lua.set_app_data(UserSourceCheck(Box::new(check)));
}

/// Where user Lua enters the VM, for chunk loads and `require` alike, so the
/// installed source check sees every user file that runs. A new load path of
/// user source should call this instead of `lua.load`.
fn load_user_source<'a>(lua: &'a Lua, name: &str, source: &'a str) -> Chunk<'a> {
    if let Some(check) = lua.app_data_ref::<UserSourceCheck>() {
        (check.0)(name, source);
    }
    lua.load(source).set_name(name)
}

/// Everything a load needs besides the code itself.
///
/// One value rather than a row of positional arguments: it travels unchanged
/// from the caller through the request channel into the runtime, and the
/// fields only a package cares about would otherwise be spelled out, at their
/// boring default, by every other caller.
pub struct LoadContext {
    pub plugin_dir: Option<PathBuf>,
    pub permissions: PluginPermissions,
    pub opts: PluginOpts,
    /// Shared lock on the revision directory being loaded, held until the
    /// owner is dropped so a prune cannot delete code that is still running.
    /// `None` for anything that is not a managed package checkout.
    pub revision_guard: Option<Arc<maki_pack::lock::Lock>>,
    /// Whether this owner is a package, which is what `pack.get` reports as
    /// active.
    pub package: bool,
    /// The binding-specific grant, dropped when a load fails.
    pub grant: DomainGrant,
}

/// The per-load state of [`LoadScope::grant`], opaque to the core: each
/// domain downcasts it to its own type.
pub type DomainGrant = Box<dyn Any + Send>;

impl LoadContext {
    /// A load with no package behind it: a builtin, a config file, or a
    /// single plugin file.
    pub fn plain(
        plugin_dir: Option<PathBuf>,
        permissions: PluginPermissions,
        grant: DomainGrant,
    ) -> Self {
        Self {
            plugin_dir,
            permissions,
            opts: PluginOpts::default(),
            revision_guard: None,
            package: false,
            grant,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigScope {
    Global,
    Project,
    UiGlobal,
    UiProject,
    Named(String),
}

impl ConfigScope {
    /// The scopes of the user's own config files, in the order they run.
    const USER_FILES: [Self; 4] = [Self::Global, Self::Project, Self::UiGlobal, Self::UiProject];

    pub fn label(&self) -> &str {
        match self {
            Self::Global => "global/init.lua",
            Self::Project => "project/init.lua",
            Self::UiGlobal => "global/ui.lua",
            Self::UiProject => "project/ui.lua",
            Self::Named(name) => name,
        }
    }

    /// The scope of the user's config file whose load is named {label},
    /// `None` for any other load.
    pub fn of_user_file(label: &str) -> Option<Self> {
        Self::USER_FILES
            .into_iter()
            .find(|scope| scope.label() == label)
    }

    pub fn is_project(&self) -> bool {
        matches!(self, Self::Project | Self::UiProject)
    }

    /// The component the file speaks for: `ui.lua` the UI's, any other
    /// config the agent's.
    pub fn component(&self) -> Component {
        match self {
            Self::UiGlobal | Self::UiProject => Component::Ui,
            Self::Global | Self::Project | Self::Named(_) => Component::Agent,
        }
    }

    /// Whether `maki.pack.add` may declare packages here: only the global
    /// files, whose declarations resolve against the user's own site.
    fn declares_packages(&self) -> bool {
        matches!(self, Self::Global | Self::UiGlobal)
    }
}

#[derive(Clone, Copy)]
struct ConfigLoad<'a> {
    store: &'a ConfigStore,
    scope: &'a ConfigScope,
}

/// Load/clear drain in-flight tools first so we never mutate a
/// plugin environment while a tool call is still running.
pub enum Request {
    /// Plugins are loaded, so native codegen may start using idle time. Sent
    /// last so it never interleaves with the loads themselves.
    WarmJit,
    /// Takes the package operations Lua recorded, leaving the queue empty.
    TakePackOps {
        reply: flume::Sender<Vec<String>>,
    },
    /// Closes the queue and hands over whatever is still in it.
    ///
    /// Both halves in one message on purpose: a Lua task can record an
    /// activation between a read and a separate close, and closing without
    /// taking would strand it in a queue nobody reads again.
    SealPackOps {
        reply: flume::Sender<Vec<String>>,
    },
    /// Runs {steps} in order as one load of plugin {name}.
    LoadSource {
        name: Arc<str>,
        steps: Vec<LoadStep>,
        context: LoadContext,
        reply: flume::Sender<LoadResult>,
    },
    ClearPlugin {
        plugin: Arc<str>,
        reply: flume::Sender<()>,
    },
    RunInitLua {
        source: String,
        scope: ConfigScope,
        plugin_dir: Option<PathBuf>,
        grant: DomainGrant,
        reply: flume::Sender<Result<Setup, PluginError>>,
    },
    CollectPluginOptions {
        reply: flume::Sender<PluginOptionSpecs>,
    },
    /// Packages `init.lua` declared. Read after the init files have run, since
    /// that is when the declared set is complete.
    CollectPackages {
        reply: flume::Sender<Vec<crate::api::pack::Declared>>,
    },
    CollectPackageContext {
        reply: flume::Sender<(Vec<crate::api::pack::Declared>, BTreeSet<String>)>,
    },
    Shutdown,
    FireAutocmd {
        event: String,
        data: Value,
    },
    EndSession(EndSession),
    Domain(Box<dyn DomainRequest>),
}

impl Request {
    pub fn domain(request: impl DomainRequest) -> Self {
        Self::Domain(Box::new(request))
    }

    /// The domain request inside, when it is a `T`.
    pub fn into_domain<T: DomainRequest>(self) -> Option<T> {
        let Self::Domain(request) = self else {
            return None;
        };
        let request: Box<dyn Any> = request;
        request.downcast().ok().map(|request| *request)
    }
}

/// Host-fired hooks, taken off the request loop. Their handlers can suspend
/// (job and `maki.fs` awaits), and awaiting one inline would stop the loop
/// from serving anything else, including the priority lane. One consumer
/// keeps them in the order the host fired them.
enum HostHook {
    Autocmd { event: String, data: Value },
    EndSession(EndSession),
}

/// Reap session-owned jobs and fire `SessionEnd`.
pub struct EndSession {
    pub session: MakiId,
    pub reason: SessionEndReason,
    /// Set on the paths that block on the dispatch: the instant the caller
    /// stops waiting, and where to answer it. Absent on the queued paths.
    pub wait: Option<(Instant, flume::Sender<()>)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KillReason {
    Cancelled,
    Deadline,
}

/// Why the watchdog raises. Kept apart from [`KillReason`] because a stuck
/// task is not doomed: once the raise unwinds, nobody has stopped waiting
/// for its reply, so it gets no cancel hooks and its error is the reply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kill {
    Doomed(KillReason),
    Stuck,
}

fn stuck_message(plugin: &str) -> String {
    format!(
        "plugin {plugin} blocked the Lua thread for {}s without yielding",
        SLICE_KILL.as_secs()
    )
}

/// Lua is single-threaded so this Mutex never contends, but
/// `Lua::app_data` requires `Send + Sync` with the `send` feature.
pub struct TaskCell {
    pub id: u64,
    pub cancel: CancelToken,
    /// End of the current [`KILL_GRACE`], armed by the first watchdog poke
    /// that sees a doomed or stuck task and cleared at every yield.
    kill_at: Cell<Option<Instant>>,
    /// `None` while the task is parked in an await.
    slice_started: Cell<Option<Instant>>,
    /// The watchdog pokes every 50ms, and a slow slice should log once.
    slice_warned: Cell<bool>,
    pub deadline: Cell<Option<Instant>>,
    pub deadline_secs: Cell<Option<u64>>,
    /// Notified by `ctx:set_deadline`, so [`until_abandoned`] re-arms on the
    /// new deadline instead of staying parked on the one it started with.
    pub deadline_changed: Event,
    /// What the domain keeps on this task, see [`TaskExt`].
    pub exts: TaskExts,
    /// When `Some`, `maki.async.run` tasks queue here instead of the global
    /// `SpawnQueue` so restore can run them inline before snapshotting.
    pub inline_spawn: Option<Vec<PendingAsyncTask>>,
    /// Cleared for delivery scopes: a job may not bind to a scope that dies
    /// with one event-delivery batch.
    owns_jobs: bool,
    /// `maki.async.on_cancel` callbacks, fired once by [`ScopedFuture::poll`]
    /// and dropped, so a handler parked in an await still gets to paint the
    /// cancelled state before the host stops waiting for it.
    cancel_hooks: Vec<RegistryKey>,
    pub lineage: Lineage,
    /// True while running a command the user typed or a key they pressed, so
    /// `maki.task.stop` knows a person asked for it. `maki.async` tasks start
    /// without it, since they can run long after the keypress. So do commands
    /// run through `maki.api.run_command`, which any plugin code can call.
    user_action: bool,
    /// Set under a `maki.async.spawn` task and passed down to the runs it
    /// queues. [`TaskLifetime::Detached`] says why.
    detached: bool,
}

/// What a task passes down to the `maki.async.run` tasks it queues. It travels
/// as one value, so a new field here reaches every queued task without
/// touching each path that queues one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lineage {
    /// Slash-command hops that led to this task, so `maki.api.run_command`
    /// can refuse a chain that never ends. A keybind or tool starts at 0.
    pub command_depth: u8,
    /// The session whose tool call this task serves. Its windows go to that
    /// session's tab, which is not always the one on screen.
    pub session: Option<MakiId>,
    pub working_dir: Option<Arc<Path>>,
}

impl Lineage {
    pub fn current(lua: &Lua) -> Self {
        lua.app_data_ref::<TaskHandle>()
            .map(|handle| lock_cell(&handle).lineage.clone())
            .unwrap_or_default()
    }
}

pub(crate) fn working_dir(lua: &Lua) -> IoResult<PathBuf> {
    match Lineage::current(lua).working_dir {
        Some(path) => Ok(path.to_path_buf()),
        None => std::env::current_dir(),
    }
}

impl TaskCell {
    pub fn new(cancel: CancelToken, deadline: Option<Instant>) -> Self {
        Self {
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            cancel,
            kill_at: Cell::new(None),
            slice_started: Cell::new(None),
            slice_warned: Cell::new(false),
            deadline: Cell::new(deadline),
            deadline_secs: Cell::new(None),
            deadline_changed: Event::new(),
            exts: TaskExts::default(),
            inline_spawn: None,
            cancel_hooks: Vec::new(),
            owns_jobs: true,
            lineage: Lineage::default(),
            user_action: false,
            detached: false,
        }
    }

    pub fn into_handle(self) -> TaskHandle {
        Arc::new(Mutex::new(self))
    }

    /// Cancel outranks the deadline: once nobody waits for the reply,
    /// reporting a timeout would only mislead.
    pub fn live_deadline(&self) -> Option<Instant> {
        self.deadline
            .get()
            .filter(|_| !self.exts.0.iter().any(|ext| ext.holds_deadline()))
    }

    pub fn doomed(&self, now: Instant) -> Option<KillReason> {
        if self.cancel.is_cancelled() {
            Some(KillReason::Cancelled)
        } else if self.live_deadline().is_some_and(|d| now > d) {
            Some(KillReason::Deadline)
        } else {
            None
        }
    }

    /// [`Self::doomed`] gated by [`KILL_GRACE`]: the task is only shot
    /// once it has burned a whole grace inside one execution slice. A slice
    /// past [`SLICE_KILL`] is shot too, doomed or not.
    fn kill_due(&self, now: Instant) -> Option<Kill> {
        let kill = match self.doomed(now) {
            Some(reason) => Kill::Doomed(reason),
            None if self.held_for(now).is_some_and(|held| held > SLICE_KILL) => Kill::Stuck,
            None => {
                self.renew_kill_grace();
                return None;
            }
        };
        match self.kill_at.get() {
            Some(kill_at) if now <= kill_at => None,
            // No stamp yet, or the grace just ran out. Either way a slice
            // starts now: a raise is usually caught (a `pcall`, or a
            // `gather` child dying inside its parent's slice) and what
            // runs next is the cleanup the grace exists for. A stuck slice
            // already had its seconds, so only its catcher gets a grace.
            stamp => {
                self.kill_at.set(Some(now + KILL_GRACE));
                (stamp.is_some() || kill == Kill::Stuck).then_some(kill)
            }
        }
    }

    /// A poll is starting, so the task yielded and earns a fresh grace. A
    /// poll nested in another poll of this cell keeps the outer start, or
    /// nesting would hand the task a new budget. Pass the result to
    /// [`Self::end_slice`].
    pub fn begin_slice(&self, now: Instant) -> Option<Instant> {
        self.renew_kill_grace();
        let outer = self.slice_started.get();
        if outer.is_none() {
            self.slice_started.set(Some(now));
            self.slice_warned.set(false);
        }
        outer
    }

    fn end_slice(&self, outer: Option<Instant>) {
        self.slice_started.set(outer);
    }

    fn held_for(&self, now: Instant) -> Option<Duration> {
        self.slice_started
            .get()
            .map(|started| now.saturating_duration_since(started))
    }

    /// How long the slice has held the thread, only the first time it is
    /// past [`SLICE_WARN`].
    fn slice_warning(&self, now: Instant) -> Option<Duration> {
        let held = self.held_for(now)?;
        (held > SLICE_WARN && !self.slice_warned.replace(true)).then_some(held)
    }

    /// The task yielded, so its grace starts over: a task parked in an await
    /// would otherwise burn the whole budget before it gets to clean up.
    fn renew_kill_grace(&self) {
        self.kill_at.set(None);
    }

    /// Hooks of a task that ended without a cancel never fire, so this is
    /// the only thing that unpins their closures and captures.
    fn clear_cancel_hooks(&mut self, lua: &Lua) {
        for key in self.cancel_hooks.drain(..) {
            lua.remove_registry_value(key).ok();
        }
    }
}

/// What one domain keeps on a task's cell. `Send` because the cell goes into
/// Lua app data. A cell can outlive its scope, so the scope's end is
/// [`Self::end`], not the drop.
pub trait TaskExt: Any + Send {
    /// The share a `maki.async.run` queued under this scope takes; `inline`
    /// for a task queued inside its parent's cell.
    fn inherit(&self, inline: bool) -> Option<Box<dyn TaskExt>>;
    fn cancel_reason(&self, _lua: &Lua) -> Option<&'static str> {
        None
    }
    fn owner_cancel(&self) -> Option<CancelToken> {
        None
    }
    fn drain_owner(&self) -> Option<Arc<dyn DrainOwner>> {
        None
    }
    fn holds_deadline(&self) -> bool {
        false
    }
    /// The scope ended, after its jobs were reaped and `Domain::end_task` ran.
    fn end(&mut self) {}
}

pub trait DrainOwner: Send + Sync {
    fn released(&self) -> bool;
    fn changed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// A cell's extensions, at most one of each type.
#[derive(Default)]
pub struct TaskExts(Vec<Box<dyn TaskExt>>);

impl TaskExts {
    pub fn get<T: TaskExt>(&self) -> Option<&T> {
        self.0
            .iter()
            .find_map(|ext| (&**ext as &dyn Any).downcast_ref())
    }

    pub fn get_mut<T: TaskExt>(&mut self) -> Option<&mut T> {
        self.0
            .iter_mut()
            .find_map(|ext| (&mut **ext as &mut dyn Any).downcast_mut())
    }

    pub fn get_or_insert_with<T: TaskExt>(&mut self, make: impl FnOnce() -> T) -> &mut T {
        if self.get::<T>().is_none() {
            self.0.push(Box::new(make()));
        }
        self.get_mut().expect("the extension was just inserted")
    }

    pub fn insert<T: TaskExt>(&mut self, ext: T) {
        self.0.retain(|old| !(&**old as &dyn Any).is::<T>());
        self.0.push(Box::new(ext));
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn owner_cancel(&self) -> Option<CancelToken> {
        self.0.iter().find_map(|ext| ext.owner_cancel())
    }

    fn drain_owner(&self) -> Option<Arc<dyn DrainOwner>> {
        self.0.iter().find_map(|ext| ext.drain_owner())
    }

    fn inherit(&self, inline: bool) -> Self {
        Self(
            self.0
                .iter()
                .filter_map(|ext| ext.inherit(inline))
                .collect(),
        )
    }

    fn end(&mut self) {
        for ext in &mut self.0 {
            ext.end();
        }
    }
}

pub type TaskHandle = Arc<Mutex<TaskCell>>;

pub fn lock_cell(handle: &TaskHandle) -> std::sync::MutexGuard<'_, TaskCell> {
    handle.lock().unwrap_or_else(|e| e.into_inner())
}

/// Backs `maki.async.on_cancel`. An already cancelled task has no
/// transition left for [`ScopedFuture::poll`] to ride, so it fires inline,
/// and only after registering, so a raising hook is contained either way
/// instead of blowing up whoever armed it.
pub fn register_cancel_hook(lua: &Lua, callback: Function) -> Result<(), mlua::Error> {
    let handle = try_active_task(lua)?;
    let key = lua.create_registry_value(callback)?;
    let cancelled = {
        let mut cell = lock_cell(&handle);
        cell.cancel_hooks.push(key);
        cell.cancel.is_cancelled()
    };
    if cancelled {
        fire_cancel_hooks(lua, &handle, KillReason::Cancelled);
    }
    Ok(())
}

pub fn fire_cancel_hooks(lua: &Lua, handle: &TaskHandle, reason: KillReason) {
    // Hooks word their partial-output marker from this string.
    let reason = match reason {
        KillReason::Cancelled => lock_cell(handle)
            .exts
            .0
            .iter()
            .find_map(|ext| ext.cancel_reason(lua))
            .unwrap_or(CANCELLED_MSG),
        KillReason::Deadline => HANDLER_TIMEOUT_MSG,
    };
    let hooks = std::mem::take(&mut lock_cell(handle).cancel_hooks);
    for key in hooks {
        if let Err(e) = lua
            .registry_value::<Function>(&key)
            .and_then(|f| f.call::<()>(reason))
        {
            tracing::warn!(error = %strip_traceback(&e), "cancel hook failed");
        }
        lua.remove_registry_value(key).ok();
    }
}

/// Sole place the `--no-jit` flag touches VM state. Called once at VM
/// creation, before any chunk (init.lua included) is compiled, and hands back
/// the compiler so bundled modules compile the same way. Jit off drops to the
/// O1 interpreter with full debug info, the combination that keeps the most
/// usable backtraces.
///
/// Native codegen stays off at load time either way: mlua runs it inline from
/// `Lua::load`, and doing that for every plugin was the single largest cost of
/// startup. Loaded chunks go to a [`CodegenQueue`] instead.
fn install_compiler(lua: &Lua, jit: bool) -> Compiler {
    lua.enable_jit(false);
    let compiler = if jit {
        Compiler::new().set_optimization_level(OPT_LEVEL_JIT)
    } else {
        Compiler::new()
            .set_optimization_level(OPT_LEVEL_DEBUGGABLE)
            .set_debug_level(DEBUG_INFO_FULL)
    };
    lua.set_compiler(compiler.clone());
    compiler
}

/// Many plugins require the same bundled module, and each one needs a separate
/// instance because the module closes over the plugin's `maki`. Only the
/// instantiation has to repeat, so the source is compiled to bytecode once per
/// VM rather than once per plugin.
#[derive(Clone)]
struct BundledModules {
    source: &'static Bundled,
    compiler: Compiler,
    bytecode: Arc<Mutex<HashMap<String, Arc<Vec<u8>>>>>,
}

impl BundledModules {
    fn bytecode(&self, rel_path: &str) -> Result<Option<Arc<Vec<u8>>>, mlua::Error> {
        let mut cache = self.bytecode.lock().expect("bytecode cache");
        if let Some(cached) = cache.get(rel_path) {
            return Ok(Some(Arc::clone(cached)));
        }
        let Some(source) = self.source.source(rel_path) else {
            return Ok(None);
        };
        let compiled = Arc::new(self.compiler.compile(source)?);
        cache.insert(rel_path.to_owned(), Arc::clone(&compiled));
        Ok(Some(compiled))
    }
}

/// Chunks awaiting native codegen, `None` when jit is off. Compiling a chunk's
/// main function also compiles every function nested in it, and the native code
/// lives on the shared proto, so closures already handed to the tool registry
/// get faster too.
type CodegenQueue = Option<Arc<Mutex<Vec<Function>>>>;

fn queue_codegen(queue: &CodegenQueue, func: &Function) {
    if let Some(queue) = queue {
        queue.lock().expect("codegen queue").push(func.clone());
    }
}

/// Runs `f` against the package declarations `init.lua` built up.
///
/// Every reader goes through here: the store is app data behind a mutex, and
/// each site that spelled that lookup out by hand was another place to get the
/// missing-store case or the poisoned lock wrong.
fn with_packs<R: Default>(
    lua: &Lua,
    f: impl FnOnce(&mut crate::api::pack::PackDeclarations) -> R,
) -> R {
    let Some(store) = lua.app_data_ref::<crate::api::pack::PackStore>() else {
        return R::default();
    };
    let mut declarations = store.lock().expect("pack declarations");
    f(&mut declarations)
}

fn module_io_error(modname: &str, path: &Path, error: &std::io::Error) -> mlua::Error {
    mlua::Error::runtime(format!(
        "require '{modname}': cannot read {}: {error}",
        path.display()
    ))
}

fn sandbox_escape(modname: &str) -> mlua::Error {
    mlua::Error::runtime(format!("require: '{modname}' outside sandbox"))
}

/// The directory `require` searches.
///
/// `dir` is always resolved, so every later comparison sees one spelling. A
/// config file's `lua/` is the user's own tree and is searched as given; a
/// package is downloaded, and git carries symlinks, so a package root is
/// `sandboxed` and every hit under it must still resolve inside `dir`.
struct RequireRoot {
    dir: PathBuf,
    sandboxed: bool,
}

impl RequireRoot {
    /// A directory the user controls, such as the one holding their `init.lua`.
    fn trusted(dir: PathBuf) -> Self {
        Self {
            dir: dir.canonicalize().unwrap_or(dir),
            sandboxed: false,
        }
    }

    /// A package's `lua/`, or `None` when there is nothing safe to search.
    ///
    /// Resolving `lua/` is not enough on its own: if `lua` is itself a symlink
    /// out of the package, its target would become the sandbox root and
    /// everything beneath that target would pass the containment check. So the
    /// resolved root must still be inside the resolved package directory. A
    /// package with no `lua/` simply has no module root, which is not an error.
    fn sandboxed(plugin_dir: &Path) -> Option<Self> {
        let dir = plugin_dir.join("lua").canonicalize().ok()?;
        let root = plugin_dir
            .canonicalize()
            .unwrap_or_else(|_| plugin_dir.to_path_buf());
        if !dir.starts_with(&root) {
            tracing::warn!(
                plugin_dir = %plugin_dir.display(),
                resolved = %dir.display(),
                "lua directory resolves outside its package; modules will not load"
            );
            return None;
        }
        Some(Self {
            dir,
            sandboxed: true,
        })
    }
}

struct ModuleLoader {
    bundled: BundledModules,
    docs: Arc<[&'static ModuleDoc]>,
    require_root: Option<RequireRoot>,
    env: Table,
    codegen: CodegenQueue,
    loaded: Table,
    loading: Table,
}

impl ModuleLoader {
    /// Bundled modules are tried first, so a plugin cannot shadow
    /// `maki.truncate` and friends with a file of its own.
    fn plugin_source(&self, rel_path: &str, modname: &str) -> Result<Option<String>, mlua::Error> {
        let Some(root) = self.require_root.as_ref() else {
            return Ok(None);
        };
        let dir = &root.dir;
        let normalized = dir
            .join(rel_path)
            .components()
            .fold(PathBuf::new(), |mut acc, c| {
                match c {
                    std::path::Component::ParentDir => {
                        acc.pop();
                    }
                    std::path::Component::CurDir => {}
                    _ => acc.push(c),
                }
                acc
            });
        if !normalized.starts_with(dir) {
            return Err(sandbox_escape(modname));
        }
        // External packages are downloaded, and git carries symlinks, so the
        // lexical fold above is not enough: a link inside the package can still
        // point out of it. Resolve the real path and re-check.
        //
        // Only an absent file is a miss. An unreadable one, a symlink loop, or
        // a file that is not UTF-8 is reported, because reporting it as
        // "module not found" sends the user looking for a name they can see.
        let resolved = match std::fs::canonicalize(&normalized) {
            Ok(resolved) => resolved,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(module_io_error(modname, &normalized, &e)),
        };
        if root.sandboxed && !resolved.starts_with(dir) {
            return Err(sandbox_escape(modname));
        }
        match std::fs::read_to_string(&resolved) {
            Ok(source) => Ok(Some(source)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(module_io_error(modname, &resolved, &e)),
        }
    }

    fn bind(&self, chunk: Chunk<'_>, modname: &str) -> Result<Function, mlua::Error> {
        chunk
            .set_name(modname)
            .set_environment(self.env.clone())
            .into_function()
    }

    /// Bundled modules load as bytecode from the shared cache. Plugin files
    /// load as source, so Luau reports syntax errors against the file the user
    /// wrote.
    ///
    /// Both forms Neovim accepts are tried, `<mod>.lua` before
    /// `<mod>/init.lua`. Every bundled candidate is tried before any plugin
    /// file, so a package still cannot shadow a bundled module.
    fn load(&self, lua: &Lua, modname: &str) -> Result<LuaValue, mlua::Error> {
        let base = modname.replace('.', "/");
        let candidates = [format!("{base}.lua"), format!("{base}/init.lua")];

        let mut func = None;
        for rel_path in &candidates {
            if let Some(bytecode) = self.bundled.bytecode(rel_path)? {
                func = Some(self.bind(
                    lua.load(bytecode.as_slice()).set_mode(ChunkMode::Binary),
                    modname,
                )?);
                break;
            }
        }
        if func.is_none() {
            for rel_path in &candidates {
                if let Some(source) = self.plugin_source(rel_path, modname)? {
                    func = Some(self.bind(load_user_source(lua, modname, &source), modname)?);
                    break;
                }
            }
        }
        let Some(func) = func else {
            return Err(mlua::Error::runtime(format!(
                "require '{modname}': module not found"
            )));
        };

        queue_codegen(&self.codegen, &func);
        func.call(())
    }

    fn require(&self, lua: &Lua, modname: &str) -> Result<LuaValue, mlua::Error> {
        if modname.is_empty() {
            return Err(mlua::Error::runtime(
                "require: module name must be non-empty",
            ));
        }

        if let Ok(cached) = self.loaded.get::<LuaValue>(modname)
            && cached != LuaValue::Nil
        {
            return Ok(cached);
        }

        if self.loading.opt_bool(modname).unwrap_or(false) {
            return Ok(LuaValue::Boolean(true));
        }

        if let Some(module) =
            docs_render::virtual_module(lua, modname, self.bundled.source, &self.docs)
        {
            let module = module?;
            self.loaded.set(modname, module.clone())?;
            return Ok(LuaValue::Table(module));
        }

        // Cleared on every path, so a failed require never leaves the module
        // wedged as "in progress".
        self.loading.set(modname, true)?;
        let result = self.load(lua, modname);
        self.loading.set(modname, LuaValue::Nil)?;
        let result = result?;

        let stored = if result == LuaValue::Nil {
            LuaValue::Boolean(true)
        } else {
            result.clone()
        };
        self.loaded.set(modname, stored)?;
        Ok(result)
    }
}

type InterruptFn = unsafe extern "C-unwind" fn(*mut ffi::lua_State, c_int);

/// The poker thread and the VM thread race on this field, so the write
/// must be atomic to stay defined behavior on the Rust side.
fn store_interrupt(state: *mut ffi::lua_State, cb: Option<InterruptFn>) {
    let raw = cb.map_or(ptr::null_mut(), |f| f as *mut ());
    unsafe {
        let slot = &raw mut (*ffi::lua_callbacks(state)).interrupt;
        AtomicPtr::from_ptr(slot.cast::<*mut ()>()).store(raw, Ordering::Release);
    }
}

/// Shutdown flag mirrored into app data so the watchdog interrupt can
/// re-check it on the Lua thread.
struct ShutdownFlag(Arc<AtomicBool>);

/// Cancellation watchdog. A resident mlua interrupt fires at every
/// safepoint and costs ~100ns a pop, which ate most of the codegen win
/// (see `benches/luau_perf.rs`). So the VM runs with no interrupt at
/// all, and this thread arms a one-shot native one every poll tick.
/// Luau documents `lua_callbacks(L)->interrupt` as safe to assign from
/// another thread, and the VM only pays a null check per safepoint.
/// The callback re-checks shutdown/cancel/deadline on the Lua thread
/// before raising, so a stale poke never kills the wrong task.
struct Watchdog {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    fn spawn(lua: &Lua, shutdown: Arc<AtomicBool>) -> Self {
        lua.set_app_data(ShutdownFlag(shutdown));
        let main_state =
            lua.exec_raw_lua(|raw| unsafe { ffi::lua_mainthread(raw.state()) }) as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let stop = Arc::clone(&stop);
            // Keeps the VM alive while this thread can still write to it,
            // even if a refactor reorders drops.
            let keep_alive = lua.clone();
            move || {
                let _keep_alive = keep_alive;
                loop {
                    thread::park_timeout(WATCHDOG_POLL_INTERVAL);
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    store_interrupt(main_state as *mut ffi::lua_State, Some(watchdog_interrupt));
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

/// One-shot interrupt armed by [`Watchdog`]: disarms itself, re-checks the
/// kill conditions, and raises a plain string error that unwinds like any
/// Lua error. Must not raise during GC (`gc >= 0`), same rule mlua follows.
unsafe extern "C-unwind" fn watchdog_interrupt(state: *mut ffi::lua_State, gc: c_int) {
    if gc >= 0 {
        return;
    }
    store_interrupt(state, None);
    // A Rust panic must not unwind into the VM; treat it as "no kill".
    let msg = catch_unwind(|| interrupt_reason(state)).unwrap_or(None);
    if let Some(msg) = msg {
        unsafe {
            // A safepoint frame may have zero free slots; grow before pushing
            // (raw pushes assert a free slot). On failure the next poke retries.
            if ffi::lua_checkstack(state, 1) == 0 {
                return;
            }
            ffi::lua_pushlstring(state, msg.as_ptr().cast(), msg.len());
        }
        // `lua_error` can jump straight over Rust destructors, and the VM
        // has its own copy of the message by now.
        drop(msg);
        unsafe { ffi::lua_error(state) };
    }
}

fn interrupt_reason(state: *mut ffi::lua_State) -> Option<Cow<'static, str>> {
    let lua = unsafe { Lua::get_or_init_from_ptr(state) };
    if lua
        .app_data_ref::<ShutdownFlag>()
        .is_some_and(|f| f.0.load(Ordering::Relaxed))
    {
        return Some(INTERRUPT_SHUTDOWN_MSG.into());
    }
    let now = Instant::now();
    let (task, warning, kill) = {
        let handle = lua.app_data_ref::<TaskHandle>()?;
        let cell = lock_cell(&handle);
        (cell.id, cell.slice_warning(now), cell.kill_due(now))
    };
    if let Some(held) = warning {
        let blame = Blame::of(lua, state);
        tracing::warn!(
            plugin = %blame.plugin,
            at = %blame.location,
            task,
            held_ms = held.as_millis() as u64,
            kill_after_ms = SLICE_KILL.as_millis() as u64,
            "plugin is holding the Lua thread, long loops should call maki.async.sleep(0)"
        );
    }
    Some(match kill? {
        Kill::Doomed(KillReason::Cancelled) => INTERRUPT_CANCELLED_MSG.into(),
        Kill::Doomed(KillReason::Deadline) => INTERRUPT_DEADLINE_MSG.into(),
        Kill::Stuck => {
            let blame = Blame::of(lua, state);
            let msg = stuck_message(&blame.plugin);
            tracing::warn!(plugin = %blame.plugin, at = %blame.location, task, "{msg}");
            host_flash(lua, msg.clone());
            msg.into()
        }
    })
}

/// Each plugin's env table by address. Every function a plugin defines shares
/// that env, so the watchdog can tell whose code is running from the raw
/// stack alone.
#[derive(Default)]
pub(crate) struct PluginEnvs(HashMap<Arc<str>, usize>);

/// Whether {plugin} has a load in place: from the moment its chunks start
/// running until it is unloaded.
pub fn plugin_loaded(lua: &Lua, plugin: &str) -> bool {
    lua.app_data_ref::<PluginEnvs>()
        .is_some_and(|envs| envs.0.contains_key(plugin))
}

/// Puts a load of {plugin} in place on a VM no host runs.
#[cfg(any(test, feature = "test-support"))]
pub fn mark_loaded(lua: &Lua, plugin: &str) {
    let mut envs = lua.remove_app_data::<PluginEnvs>().unwrap_or_default();
    envs.0
        .insert(Arc::from(plugin), lua.globals().to_pointer() as usize);
    lua.set_app_data(envs);
}

/// Whether any plugin with a load in place passes {test}.
pub fn any_plugin_loaded(lua: &Lua, mut test: impl FnMut(&str) -> bool) -> bool {
    lua.app_data_ref::<PluginEnvs>()
        .is_some_and(|envs| envs.0.keys().any(|plugin| test(plugin)))
}

/// Which plugin was already told about which gap: an event it gets without
/// content, or a slot plus the permissions its layer lacks there. Keyed by
/// plugin first so the check every event makes needs no owned key.
#[derive(Default)]
struct PermissionWarnings(HashMap<String, HashSet<String>>);

/// A permission gap that silently changes what a plugin sees or does, shown
/// once per plugin per {subject}. Each of these is re-decided on every event
/// or call, so without the dedup it would repeat for the life of the
/// misconfiguration, and without the flash nobody would ever notice it.
pub(crate) fn warn_plugin_once(
    lua: &Lua,
    plugin: &str,
    subject: &str,
    msg: impl FnOnce() -> String,
) {
    if lua.app_data_ref::<PermissionWarnings>().is_none() {
        lua.set_app_data(PermissionWarnings::default());
    }
    {
        let Some(mut warned) = lua.app_data_mut::<PermissionWarnings>() else {
            return;
        };
        if warned
            .0
            .get(plugin)
            .is_some_and(|seen| seen.contains(subject))
        {
            return;
        }
        warned
            .0
            .entry(plugin.to_owned())
            .or_default()
            .insert(subject.to_owned());
    }
    let msg = msg();
    tracing::warn!(plugin, subject, "{msg}");
    host_flash(lua, msg);
}

pub(crate) fn quoted_list(items: &[impl Display]) -> String {
    items
        .iter()
        .map(|item| format!("'{item}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn layer_skipped_message(plugin: &str, slot: &str, missing: &[Permission]) -> String {
    format!(
        "plugin {plugin}: its layer on '{slot}' is skipped, it needs {}. {}",
        quoted_list(missing),
        grant_hint(missing)
    )
}

/// Who to name for the code a watchdog interrupt caught running.
struct Blame {
    plugin: Arc<str>,
    location: String,
}

impl Blame {
    /// Walks the stack from the innermost frame out. The location is the
    /// first Lua frame, the plugin the first frame whose env belongs to one.
    /// Innermost wins, so when plugin A calls a helper from plugin B and the
    /// helper spins, B gets the blame.
    fn of(lua: &Lua, state: *mut ffi::lua_State) -> Self {
        let mut plugin = None;
        let mut location = None;
        let envs = lua.app_data_ref::<PluginEnvs>();
        // Room for the frame's function and its env.
        if unsafe { ffi::lua_checkstack(state, 2) } != 0 {
            let mut ar: ffi::lua_Debug = unsafe { std::mem::zeroed() };
            let mut level = 0;
            while plugin.is_none()
                && unsafe { ffi::lua_getinfo(state, level, c"fsl".as_ptr(), &mut ar) } != 0
            {
                if location.is_none() && ar.currentline >= 0 {
                    let src = unsafe { CStr::from_ptr(ar.short_src) }.to_string_lossy();
                    location = Some(format!("{src}:{}", ar.currentline));
                }
                let env = unsafe {
                    ffi::lua_getfenv(state, -1);
                    let env = ffi::lua_topointer(state, -1) as usize;
                    ffi::lua_pop(state, 2);
                    env
                };
                plugin = envs
                    .as_ref()
                    .and_then(|envs| envs.0.iter().find(|(_, ptr)| **ptr == env))
                    .map(|(name, _)| Arc::clone(name));
                level += 1;
            }
        }
        Self {
            plugin: plugin.unwrap_or_else(|| Arc::from(UNKNOWN_PLUGIN)),
            location: location.unwrap_or_else(|| UNKNOWN_LOCATION.to_owned()),
        }
    }
}

/// Owns a task's cell for as long as the task runs. It does not make the task
/// the active one: [`Self::scope_future`] does that for each poll, and
/// [`Self::enter`] for Lua called synchronously.
pub struct TaskScope {
    lua: Lua,
    handle: TaskHandle,
}

impl TaskScope {
    pub fn new(lua: &Lua, mut cell: TaskCell) -> Self {
        if let Some(domain) = TaskDomain::of(lua) {
            domain.begin_task(lua, &mut cell.exts);
        }
        Self {
            lua: lua.clone(),
            handle: cell.into_handle(),
        }
    }

    /// A task of its own for a system callback. Without one the callback
    /// runs unwatched and task APIs like `jobstart` refuse it. Prefer
    /// [`run_detached`] over raw scopes.
    pub fn detached(lua: &Lua) -> Self {
        Self::new(lua, TaskCell::new(CancelToken::none(), None))
    }

    /// [`Self::detached`] for event-delivery batches: the scope dies with the
    /// batch, so a job bound to it would be killed microseconds after its
    /// callback returns. `job_task_id` returns `None` under it, turning that
    /// silent kill into a loud `jobstart` error.
    pub(crate) fn delivery(lua: &Lua) -> Self {
        let scope = Self::detached(lua);
        lock_cell(&scope.handle).owns_jobs = false;
        scope
    }

    pub fn handle(&self) -> &TaskHandle {
        &self.handle
    }

    pub fn scope_future<F>(&self, inner: F) -> ScopedFuture<F> {
        ScopedFuture::new(self.lua.clone(), Arc::clone(&self.handle), inner)
    }

    /// Makes this the active task until the guard drops. Only for Lua called
    /// without yielding: a guard held across an await would leak the task
    /// into whatever the executor runs meanwhile.
    pub fn enter(&self) -> AppDataGuard<'_, TaskHandle> {
        AppDataGuard::publish(&self.lua, Some(Arc::clone(&self.handle)))
    }
}

/// Puts {value} up as the `T` app data, or takes the `T` down for `None`,
/// and brings back the one it covered on drop.
///
/// The active task and the [`Ceiling`] are published only while their code
/// runs, for one poll of a future or the life of a [`TaskScope::enter`]
/// guard. So code entered with no scope of its own, like a plugin's top
/// level at load, sees neither, rather than whatever ran last.
#[must_use]
pub struct AppDataGuard<'a, T: MaybeSend + 'static> {
    lua: &'a Lua,
    prev: Option<T>,
}

impl<'a, T: MaybeSend + 'static> AppDataGuard<'a, T> {
    fn publish(lua: &'a Lua, value: Option<T>) -> Self {
        let prev = match value {
            Some(value) => lua.set_app_data(value),
            None => lua.remove_app_data::<T>(),
        };
        Self { lua, prev }
    }
}

impl<T: MaybeSend + 'static> Drop for AppDataGuard<'_, T> {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(prev) => {
                self.lua.set_app_data(prev);
            }
            None => {
                self.lua.remove_app_data::<T>();
            }
        }
    }
}

pin_project_lite::pin_project! {
    /// Puts {ceiling} up for each poll of {inner}, see [`AppDataGuard`].
    /// `None` keeps whatever ceiling the poller runs under away from it.
    pub struct Narrowed<F> {
        lua: Lua,
        ceiling: Option<Ceiling>,
        #[pin]
        inner: F,
    }
}

impl<F> Narrowed<F> {
    /// {inner} with at most {perms}, and never more than the code running
    /// now may use.
    pub fn to(lua: &Lua, perms: &PluginPermissions, inner: F) -> Self {
        Self::under(lua, Some(Ceiling(perms.now(lua))), inner)
    }

    pub fn under(lua: &Lua, ceiling: Option<Ceiling>, inner: F) -> Self {
        Self {
            lua: lua.clone(),
            ceiling,
            inner,
        }
    }
}

impl<F: Future> Future for Narrowed<F> {
    type Output = F::Output;
    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.project();
        let _ceiling = AppDataGuard::publish(this.lua, this.ceiling.clone());
        this.inner.poll(cx)
    }
}

/// Runs an async system callback under a [detached] scope so callers
/// can't forget to set one up.
///
/// Job callbacks (`on_stdout` etc.) are pumped whenever {fut} is
/// suspended, so a handler parked in e.g. `win:recv()` still streams
/// job output, like Neovim firing callbacks from its idle event loop.
///
/// [detached]: TaskScope::detached
pub async fn run_detached<F: Future>(lua: &Lua, fut: F) -> F::Output {
    run_scoped(lua, TaskScope::detached(lua), fut).await
}

/// The priority lane, reachable from off the Lua thread.
///
/// Provider hooks are called from the async side of the process, so the way
/// back in has to be something the Lua code that registers them can pick up
/// while it runs.
pub struct ProviderRequests(pub flume::Sender<Request>);

/// The two ways back to the Lua thread that a provider hook handle keeps: one
/// to ask for a call, one to hand its registry keys back when it dies. Taken
/// together because a handle without both is of no use.
pub fn host_senders(
    lua: &Lua,
) -> mlua::Result<(flume::Sender<Request>, flume::Sender<DeferredCallback>)> {
    let no_host = || mlua::Error::runtime(NO_PLUGIN_HOST);
    let requests = lua.app_data_ref::<ProviderRequests>().ok_or_else(no_host)?;
    let release = lua.app_data_ref::<DeferQueue>().ok_or_else(no_host)?;
    Ok((requests.0.clone(), release.tx.clone()))
}

/// [`run_detached`] for plugin code a host caller is blocked on, carrying every
/// obligation that waiting creates:
///
/// - counted against the [`InflightGate`], so a reload drains the code instead
///   of tearing its environment away;
/// - [`covered`], so a tool call the code makes rides this slot rather than
///   queueing behind a gate its own caller is holding;
/// - scoped to the caller's cancellation and `deadline`, so the watchdog can
///   interrupt code that spins past either;
/// - [`until_abandoned`], the only thing that ends code parked in an await,
///   which runs no Lua for the watchdog to interrupt.
///
/// Four properties one call away, so a request handler cannot pick up three and
/// quietly miss the fourth. `Err` says why the wait ended, never how the code
/// fared.
pub async fn run_awaited<F: Future>(
    lua: &Lua,
    gate: &Rc<InflightGate>,
    cell: TaskCell,
    fut: F,
) -> Result<F::Output, &'static str> {
    covered(Some(GateGuard::new(gate)), run_abandonable(lua, cell, fut)).await
}

/// [`run_awaited`] without the [`InflightGate`] slot, for code a reload must
/// not wait on, yet that still ends with its caller's cancel and deadline.
pub async fn run_abandonable<F: Future>(
    lua: &Lua,
    cell: TaskCell,
    fut: F,
) -> Result<F::Output, &'static str> {
    let scope = TaskScope::new(lua, cell);
    let handle = Arc::clone(scope.handle());
    until_abandoned(run_scoped(lua, scope, fut), &handle).await
}

/// [`run_detached`] for a slash-command handler or a keybind callback, seeding
/// the hop count that `maki.api.run_command` checks before extending the chain.
/// Depth 0 means the user acted directly, see [`TaskCell::user_action`].
pub async fn run_command_scoped<F: Future>(lua: &Lua, depth: u8, fut: F) -> F::Output {
    let scope = TaskScope::detached(lua);
    {
        let mut cell = lock_cell(scope.handle());
        cell.lineage.command_depth = depth;
        cell.user_action = depth == 0;
    }
    run_scoped(lua, scope, fut).await
}

async fn run_scoped<F: Future>(lua: &Lua, scope: TaskScope, fut: F) -> F::Output {
    let handle = Arc::clone(scope.handle());
    let owner = JobOwner::Task(lock_cell(&handle).id);
    let pump = async {
        loop {
            deliver_pending(lua, usize::MAX, || {
                with_jobs(lua, |store| store.next_event(&owner))
            })
            .await;
            tokio::time::sleep(DISPATCH_POLL_INTERVAL).await;
        }
    };
    let out = scope
        .scope_future(futures_lite::future::or(fut, pump))
        .await;
    // `or` drops the pump the moment {fut} wins, so one last pass under the
    // same scope delivers whatever arrived in between; the scope teardown
    // right after reaps any task job. Only a callback caught mid-suspend at
    // that instant is lost, never a whole event: `deliver_pending` leaves
    // events in the channel until the delivery that records them starts.
    scope
        .scope_future(deliver_pending(lua, FINAL_DRAIN_BUDGET, || {
            with_jobs(lua, |store| store.next_event(&owner))
        }))
        .await;
    drop(scope);
    out
}

async fn run_host_hook(lua: &Lua, domain: &dyn Domain, hook: HostHook) {
    match hook {
        HostHook::Autocmd { event, data } => {
            let data = json_to_lua(lua, &data).unwrap_or(LuaValue::Nil);
            let is_turn_end = event == TURN_END_EVENT;
            crate::api::autocmd::dispatch(lua.clone(), event, None, data).await;
            if is_turn_end {
                lua.gc_collect().ok();
            }
        }
        HostHook::EndSession(end) => {
            // Measured at dispatch, not when the request was queued, so a
            // handler is told what is left rather than what was promised.
            let deadline_ms = end
                .wait
                .as_ref()
                .map(|(at, _)| at.saturating_duration_since(Instant::now()).as_millis() as u64);
            // Handlers may still inspect or stop the jobs, so the event fires
            // before the reap.
            let reason = end.reason.to_string();
            let event = HostEvent::SessionEnd {
                reason: &reason,
                deadline_ms,
            };
            let data =
                json_to_lua(lua, &event.payload(Some(&end.session))).unwrap_or(LuaValue::Nil);
            crate::api::autocmd::dispatch(lua.clone(), event.name().to_owned(), None, data).await;
            with_jobs(lua, |store| store.kill_session(lua, end.session));
            domain.end_session(lua, end.session);
            if let Some((_, reply)) = end.wait {
                let _ = reply.send(());
            }
        }
    }
}

/// Fire job callbacks for up to {budget} queued events, one at a time so a
/// dropped caller cannot strand a batch. Failures are logged and the drain
/// continues: the event is already recorded and the next one still needs
/// delivering.
async fn deliver_pending(
    lua: &Lua,
    budget: usize,
    mut next: impl FnMut() -> Option<(u32, JobEvent)>,
) {
    for _ in 0..budget {
        let Some((job_id, event)) = next() else {
            return;
        };
        if let Err(e) = deliver_job_event(lua, job_id, &event).await {
            tracing::warn!(job_id, error = %strip_traceback(&e), "job callback failed");
        }
        // A callback with nothing to await never yields, so a job printing
        // faster than we deliver would hold the executor here forever.
        tokio::task::yield_now().await;
    }
}

/// Deliver what {job_id} has queued, then leave {delivering} so the next
/// event it queues gets a task again. The delivery scope republishes the
/// task handle across callback yields: an unscoped resume would run with no
/// task, out of the watchdog's reach.
async fn deliver_job(lua: Lua, job_id: u32, delivering: Rc<RefCell<HashSet<u32>>>) {
    let scope = TaskScope::delivery(&lua);
    scope
        .scope_future(deliver_pending(&lua, usize::MAX, || {
            with_jobs(&lua, |store| store.next_event_of(job_id)).map(|event| (job_id, event))
        }))
        .await;
    delivering.borrow_mut().remove(&job_id);
}

impl Drop for TaskScope {
    fn drop(&mut self) {
        let task_id = {
            let mut cell = lock_cell(&self.handle);
            cell.clear_cancel_hooks(&self.lua);
            cell.id
        };
        with_jobs(&self.lua, |store| {
            let leftovers = store.kill_owner(&self.lua, &JobOwner::Task(task_id));
            if !leftovers.is_empty() {
                tracing::warn!(
                    task = task_id,
                    jobs = ?leftovers,
                    "scope finished with live jobs; they were killed"
                );
            }
        });
        if let Some(domain) = TaskDomain::of(&self.lua) {
            domain.end_task(&self.lua, task_id);
        }
        lock_cell(&self.handle).exts.end();
    }
}

pin_project_lite::pin_project! {
    /// Re-publishes the task handle on every `poll` so concurrent tasks
    /// on the shared Lua each see their own `TaskCell`.
    pub struct ScopedFuture<F> {
        lua: Lua,
        handle: TaskHandle,
        // Waker registration on the task's token, dropped once the hooks have
        // fired. Without it nothing would poll us while the handler sits parked
        // in an await, and the hooks would wait on a child event that may
        // never come. Already `Box::pin`ned, so no structural pinning needed.
        // `+ Send` keeps every ScopedFuture usable under mlua's `send` feature,
        // including ones awaited inside `create_async_function` bodies.
        cancel_wait: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
        #[pin]
        inner: F,
    }
}

impl<F> ScopedFuture<F> {
    pub fn new(lua: Lua, handle: TaskHandle, inner: F) -> Self {
        let cancel = lock_cell(&handle).cancel.clone();
        Self {
            lua,
            handle,
            cancel_wait: Some(Box::pin(async move { cancel.cancelled().await })),
            inner,
        }
    }
}

impl<F: Future> Future for ScopedFuture<F> {
    type Output = F::Output;
    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.project();
        // A poll means the task yielded, the cooperation the grace rewards.
        let outer_slice = lock_cell(this.handle).begin_slice(Instant::now());
        let _active = AppDataGuard::publish(this.lua, Some(Arc::clone(this.handle)));
        if let Some(wait) = this.cancel_wait.as_mut()
            && wait.as_mut().poll(cx).is_ready()
        {
            *this.cancel_wait = None;
            fire_cancel_hooks(this.lua, this.handle, KillReason::Cancelled);
        }
        let result = this.inner.poll(cx);
        lock_cell(this.handle).end_slice(outer_slice);
        result
    }
}

pub fn active_task(lua: &Lua) -> TaskHandle {
    try_active_task(lua).expect("task accessor called outside a task scope")
}

/// [`active_task`] for what a plugin reaches from Lua, where its top level
/// runs outside any task.
pub fn try_active_task(lua: &Lua) -> mlua::Result<TaskHandle> {
    lua.app_data_ref::<TaskHandle>()
        .map(|r| Arc::clone(&*r))
        .ok_or_else(|| mlua::Error::runtime(OUTSIDE_TASK_ERR))
}

pub fn with_jobs<R>(lua: &Lua, f: impl FnOnce(&mut JobStore) -> R) -> R {
    if lua.app_data_ref::<JobStore>().is_none() {
        lua.set_app_data(JobStore::new());
    }
    let mut store = lua
        .app_data_mut::<JobStore>()
        .expect("job store was just installed");
    f(&mut store)
}

pub(crate) fn active_task_id(lua: &Lua) -> Option<u64> {
    let handle = lua.app_data_ref::<TaskHandle>()?;
    Some(lock_cell(&handle).id)
}

pub fn active_task_cancel(lua: &Lua) -> Option<(u64, CancelToken)> {
    let handle = lua.app_data_ref::<TaskHandle>()?;
    let cell = lock_cell(&handle);
    Some((cell.id, cell.cancel.clone()))
}

pub fn is_user_action(lua: &Lua) -> bool {
    lua.app_data_ref::<TaskHandle>()
        .is_some_and(|handle| lock_cell(&handle).user_action)
}

/// Task id for job ownership; `None` under a delivery scope.
pub(crate) fn job_task_id(lua: &Lua) -> Option<u64> {
    let handle = lua.app_data_ref::<TaskHandle>()?;
    let cell = lock_cell(&handle);
    cell.owns_jobs.then_some(cell.id)
}

/// A working wake lands in microseconds, so this is only about failing in
/// seconds instead of parking until nextest gives up on the suite.
#[cfg(test)]
const TEST_WAKE_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const TEST_WAKE_TIMEOUT_MSG: &str = "timed out waiting for a cancelled task to wake";

#[cfg(test)]
pub(crate) fn block_on_or_fail<T>(fut: impl Future<Output = T>) -> T {
    block_on(futures_lite::future::or(fut, async {
        tokio::time::sleep(TEST_WAKE_TIMEOUT).await;
        panic!("{TEST_WAKE_TIMEOUT_MSG}");
    }))
}

#[cfg(test)]
pub(crate) async fn wake_or_fail<T>(fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(TEST_WAKE_TIMEOUT, fut)
        .await
        .expect(TEST_WAKE_TIMEOUT_MSG)
}

/// The task inherits the [`Ceiling`] binding {plugin}, the one queuing it.
pub fn enqueue_async_task(
    lua: &Lua,
    plugin: &str,
    work_fn: RegistryKey,
) -> Result<(), mlua::Error> {
    let handle = lua.app_data_ref::<TaskHandle>();
    let (cancel, exts, lineage, detached) = match &handle {
        Some(h) => {
            let cell = lock_cell(h);
            (
                cell.exts
                    .owner_cancel()
                    .unwrap_or_else(|| cell.cancel.clone()),
                cell.exts.inherit(cell.inline_spawn.is_some()),
                cell.lineage.clone(),
                cell.detached,
            )
        }
        None => (
            CancelToken::none(),
            TaskExts::default(),
            Lineage::default(),
            false,
        ),
    };

    let task = PendingAsyncTask {
        work_fn,
        cancel,
        deadline: Some(Instant::now() + ASYNC_RUN_DEFAULT_DEADLINE),
        exts,
        lineage,
        ceiling: Ceiling::current_for(lua, plugin),
        lifetime: match detached {
            true => TaskLifetime::Detached,
            false => TaskLifetime::Held,
        },
    };

    // Inline tasks live inside the cell, so their share holds nothing that
    // would make a cycle with it; they run before the scope drops anyway.
    if let Some(h) = &handle
        && let Some(inline) = lock_cell(h).inline_spawn.as_mut()
    {
        inline.push(task);
        return Ok(());
    }

    let queue = lua
        .app_data_ref::<SpawnQueue>()
        .ok_or_else(|| mlua::Error::runtime(SPAWN_QUEUE_NOT_INIT))?;
    queue.tx.send(task).ok();
    Ok(())
}

/// A spawn belongs to {plugin}, not to its caller, so it takes nothing of
/// the caller's lifetime. Only the command depth carries over, or a command
/// cycle could hop through a spawn and start counting from zero again. The
/// session stays behind. A loop started from one tool call would open every
/// later window in that call's tab otherwise, and lose them all when the
/// session leaves.
///
/// It never goes to `inline_spawn`. A restore runs those tasks to the end
/// before it snapshots, and a spawned loop has no end.
pub(crate) fn enqueue_spawned_task(
    lua: &Lua,
    plugin: Arc<str>,
    work_fn: RegistryKey,
) -> Result<SpawnedTask, mlua::Error> {
    let queue = lua
        .app_data_ref::<SpawnQueue>()
        .ok_or_else(|| mlua::Error::runtime(SPAWN_QUEUE_NOT_INIT))?;
    let spawned = lua
        .app_data_ref::<SpawnedTasks>()
        .ok_or_else(|| mlua::Error::runtime(NO_PLUGIN_HOST))?;
    let (trigger, cancel) = CancelToken::new();
    let slot = spawned.0.insert(Arc::clone(&plugin), trigger);
    if cancel.is_cancelled() {
        tracing::debug!(plugin = %plugin, "maki.async.spawn from an unloaded plugin, dropped");
    }
    let ceiling = Ceiling::current_for(lua, &plugin);
    let handle = SpawnedTask { plugin, slot };
    queue
        .tx
        .send(PendingAsyncTask {
            work_fn,
            cancel,
            deadline: None,
            exts: TaskExts::default(),
            lineage: Lineage {
                session: None,
                ..Lineage::current(lua)
            },
            ceiling,
            lifetime: TaskLifetime::Spawned(handle.clone()),
        })
        .ok();
    Ok(handle)
}

/// Decides when the shared VM collects garbage between ops.
///
/// Luau never collects when an allocation fails, and its own pacer waits for
/// the heap to double before the next cycle. After a big read or a plugin that
/// ran out of memory, that point lies past the memory limit. The garbage then
/// stays forever and every plugin fails to allocate, even for `echo hi`.
/// We run a full collect once the heap gets halfway from what the last collect
/// kept to the limit. That always lands before the wall, and a big heap that
/// is all live data is not rescanned on every op.
struct GcPacer {
    lua: Lua,
    limit: usize,
    collect_above: Cell<usize>,
    ops_since_step: Cell<usize>,
}

impl GcPacer {
    fn new(lua: Lua, limit: usize) -> Self {
        Self {
            lua,
            limit,
            collect_above: Cell::new(halfway_to(0, limit)),
            ops_since_step: Cell::new(0),
        }
    }

    /// Also runs before each op, so no op starts on garbage that work outside
    /// the gate left behind. Returns whether it collected.
    fn relieve_pressure(&self) -> bool {
        if self.lua.used_memory() <= self.collect_above.get() {
            return false;
        }
        self.lua.gc_collect().ok();
        self.collect_above
            .set(halfway_to(self.lua.used_memory(), self.limit));
        true
    }

    fn op_finished(&self) {
        if self.relieve_pressure() {
            return;
        }
        let ops = self.ops_since_step.get() + 1;
        if ops >= GC_STEP_INTERVAL {
            self.ops_since_step.set(0);
            self.lua.gc_step().ok();
        } else {
            self.ops_since_step.set(ops);
        }
    }
}

fn halfway_to(live: usize, limit: usize) -> usize {
    live + limit.saturating_sub(live) / 2
}

/// Caps concurrent coroutines to avoid blowing the Lua stack.
/// Also serves as a drain barrier for load/clear ops.
pub struct InflightGate {
    gc: GcPacer,
    count: Cell<usize>,
    /// What [`Self::drain`] waits on. Unlike `count`, it leaves out work under
    /// a `maki.async.spawn` task and includes work still waiting for a slot.
    held: Cell<usize>,
    owner_holds: RefCell<Vec<Arc<dyn DrainOwner>>>,
    event: Event,
}

impl InflightGate {
    fn new(lua: Lua) -> Self {
        Self {
            gc: GcPacer::new(lua, LUA_MEMORY_LIMIT),
            count: Cell::new(0),
            held: Cell::new(0),
            owner_holds: RefCell::new(Vec::new()),
            event: Event::new(),
        }
    }

    fn increment(&self) {
        self.count.set(self.count.get() + 1);
        self.gc.relieve_pressure();
    }

    fn decrement(&self) {
        self.count.set(self.count.get().saturating_sub(1));
        self.event.notify(usize::MAX);
        self.gc.op_finished();
    }

    async fn wait_until(&self, ready: impl Fn() -> bool) {
        loop {
            if ready() {
                return;
            }
            let listener = self.event.listen();
            if ready() {
                return;
            }
            listener.await;
        }
    }

    async fn wait_below(&self, limit: usize) {
        self.wait_until(|| self.count.get() < limit).await;
    }

    /// Guards are taken on a task's first poll (`acquire`), so one yield
    /// lets just-spawned tasks register before the barrier reads the count;
    /// a `drain` queued right behind a spawn cannot slip past it.
    async fn drain(&self) {
        tokio::task::yield_now().await;
        loop {
            let released = self.event.listen();
            if self.held.get() > 0 {
                released.await;
                continue;
            }
            let owner = self
                .owner_holds
                .borrow()
                .iter()
                .find(|owner| !owner.released())
                .cloned();
            let Some(owner) = owner else { return };
            let changed = owner.changed();
            if !owner.released() {
                futures_lite::future::or(released, changed).await;
            }
        }
    }

    /// Admission and accounting in one step, on the task's own poll: the
    /// dispatcher can spawn a whole backlog in one go without ever parking,
    /// and the cap still holds because no coroutine is created before its
    /// guard exists.
    pub async fn acquire(self: &Rc<Self>) -> GateGuard {
        self.admit(Some(DrainHold::new(self))).await
    }

    /// The hold is taken before waiting for a slot. Otherwise held work stuck
    /// behind a cap full of detached slots would let the barrier through.
    async fn admit(self: &Rc<Self>, hold: Option<DrainHold>) -> GateGuard {
        self.wait_below(MAX_INFLIGHT_TOOLS).await;
        GateGuard::with_hold(self, hold)
    }

    /// [`Self::acquire`] for a call the host cannot interrupt yet. A call
    /// still queued here has no [`TaskCell`], so the watchdog and
    /// [`until_abandoned`] have nothing to end. The wait ends itself once
    /// nobody is left waiting for the reply, and reports what the handler
    /// would have reported.
    pub async fn acquire_before_abandoned(
        self: &Rc<Self>,
        cancel: &CancelToken,
        deadline: Option<Instant>,
    ) -> Result<GateGuard, &'static str> {
        let lapsed = async {
            match deadline {
                Some(at) => _ = tokio::time::sleep_until(at.into()).await,
                None => std::future::pending::<()>().await,
            }
            HANDLER_TIMEOUT_MSG
        };
        let cancelled = async {
            cancel.cancelled().await;
            CANCELLED_MSG
        };
        futures_lite::future::or(async { Ok(self.acquire().await) }, async {
            Err(futures_lite::future::or(cancelled, lapsed).await)
        })
        .await
    }
}

thread_local! {
    static SLOT_COVERED: Cell<bool> = const { Cell::new(false) };
}

/// Whether the caller already runs under an in-flight slot.
///
/// A tool call made from there cannot finish before its caller does, and the
/// caller holds its slot all that time. Charge the child a slot of its own and
/// the gate deadlocks against itself: park as many callers as there are slots
/// and no child is ever admitted, so no caller ever returns. The child rides
/// the caller's slot instead, which also keeps the drain barrier honest, since
/// that slot outlives the whole subtree.
pub fn under_inflight_slot() -> bool {
    SLOT_COVERED.get()
}

pin_project_lite::pin_project! {
    /// `slot` is `None` when an ancestor holds the slot for this work. The
    /// flag goes up on every poll and back down after, because other tasks on
    /// the same executor run in between and must not inherit the cover.
    pub struct SlotCovered<F> {
        _slot: Option<GateGuard>,
        #[pin]
        inner: F,
    }
}

impl<F: Future> Future for SlotCovered<F> {
    type Output = F::Output;
    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let prev = SLOT_COVERED.replace(true);
        let result = self.project().inner.poll(cx);
        SLOT_COVERED.set(prev);
        result
    }
}

pub fn covered<F: Future>(slot: Option<GateGuard>, inner: F) -> SlotCovered<F> {
    SlotCovered { _slot: slot, inner }
}

/// Keeps [`InflightGate::drain`] waiting from the moment work queues for a
/// slot until it gives the slot back. A nested call riding its caller's slot
/// takes one of its own, since a detached caller's slot holds none.
pub struct DrainHold {
    gate: Rc<InflightGate>,
    owner: Option<Arc<dyn DrainOwner>>,
}

impl DrainHold {
    pub fn new(gate: &Rc<InflightGate>) -> Self {
        Self::under(gate, None)
    }

    pub fn under(gate: &Rc<InflightGate>, owner: Option<Arc<dyn DrainOwner>>) -> Self {
        if let Some(owner) = &owner {
            gate.owner_holds.borrow_mut().push(Arc::clone(owner));
        } else {
            gate.held.set(gate.held.get() + 1);
        }
        Self {
            gate: Rc::clone(gate),
            owner,
        }
    }
}

impl Drop for DrainHold {
    fn drop(&mut self) {
        if let Some(owner) = &self.owner {
            let mut holds = self.gate.owner_holds.borrow_mut();
            if let Some(index) = holds.iter().position(|held| Arc::ptr_eq(held, owner)) {
                holds.swap_remove(index);
            }
        } else {
            self.gate.held.set(self.gate.held.get().saturating_sub(1));
        }
        self.gate.event.notify(usize::MAX);
    }
}

pub struct GateGuard {
    gate: Rc<InflightGate>,
    _hold: Option<DrainHold>,
}

impl GateGuard {
    pub fn new(gate: &Rc<InflightGate>) -> Self {
        Self::with_hold(gate, Some(DrainHold::new(gate)))
    }

    fn with_hold(gate: &Rc<InflightGate>, hold: Option<DrainHold>) -> Self {
        gate.increment();
        Self {
            gate: Rc::clone(gate),
            _hold: hold,
        }
    }
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.gate.decrement();
    }
}

pub struct PendingAsyncTask {
    pub work_fn: RegistryKey,
    pub cancel: CancelToken,
    pub deadline: Option<Instant>,
    /// What the queuing task's extensions passed down, see [`TaskExt::inherit`].
    pub exts: TaskExts,
    pub lineage: Lineage,
    /// The [`Ceiling`] up where the task was queued, which it runs under.
    pub ceiling: Option<Ceiling>,
    pub lifetime: TaskLifetime,
}

/// Who ends a queued task, and so whether [`drain_barrier`] waits for it.
pub enum TaskLifetime {
    /// A `maki.async.run`.
    Held,
    /// A `maki.async.run` queued under a spawn. It still counts against the
    /// cap, but the barrier does not wait for it. A spawned loop that queues
    /// a run every tick would otherwise block every load forever.
    Detached,
    /// A `maki.async.spawn`.
    Spawned(SpawnedTask),
}

/// One `maki.async.spawn` task's row in [`SpawnedTasks`], and its Lua handle.
#[derive(Clone)]
pub struct SpawnedTask {
    plugin: Arc<str>,
    slot: CancelSlot,
}

impl SpawnedTask {
    pub(crate) fn cancel(&self, lua: &Lua) {
        if let Some(spawned) = lua.app_data_ref::<SpawnedTasks>() {
            spawned.0.retire(&self.plugin, self.slot);
        }
    }
}

/// Channel of `maki.async.run` tasks. The dispatcher recvs the `rx` side as
/// one arm of its biased select, so a send wakes the loop even while the
/// enqueuing coroutine stays parked.
pub struct SpawnQueue {
    tx: flume::Sender<PendingAsyncTask>,
    rx: flume::Receiver<PendingAsyncTask>,
}

impl SpawnQueue {
    fn new() -> Self {
        let (tx, rx) = flume::unbounded();
        Self { tx, rx }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl SpawnQueue {
    pub fn install(lua: &Lua) {
        lua.set_app_data(Self::new());
    }

    pub fn take(lua: &Lua) -> Option<PendingAsyncTask> {
        lua.app_data_ref::<Self>()?.rx.try_recv().ok()
    }
}

/// Fire-and-forget callback queued from Lua. Runs on the Lua thread's
/// executor after `delay`, outside any TaskScope: no cancel token, no
/// script deadline. Meant for UI intent that must outlive the caller
/// (a toast dismissing itself, a debounced repaint).
///
/// `cancel` is what the `Timer` handed back to Lua flips, and what
/// [`DeferQueue::cancel_plugin`] flips on unload.
pub struct DeferredCallback {
    pub func: RegistryKey,
    pub delay: Duration,
    pub plugin: Arc<str>,
    pub cancel: Arc<AtomicBool>,
    /// The [`Ceiling`] up where the timer was set, which the callback runs
    /// under.
    pub ceiling: Option<Ceiling>,
}

pub(crate) struct DeferQueue {
    tx: flume::Sender<DeferredCallback>,
    pub(crate) rx: flume::Receiver<DeferredCallback>,
    /// Timers that have not fired yet, by owner. A sleeping timer holds no
    /// [`GateGuard`], so [`drain_barrier`] walks straight past it and a reload
    /// would leave the callback to wake up against a torn down env. Cancelling
    /// the doomed plugin's timers in [`LuaRuntime::clear_plugin`] closes that
    /// window: whatever is left is either cancelled or already gated.
    timers: Mutex<Vec<(Arc<str>, Arc<AtomicBool>)>>,
}

/// A spawned task holds no [`GateGuard`], so [`drain_barrier`] never waits
/// for it. [`LuaRuntime::clear_plugin`] cancels it here instead.
///
/// That cancel leaves a mark on the plugin until its next load. Plugin code
/// can still run on the way out, like an `on_cancel` hook or a queued job
/// callback, and the mark stops it from spawning a task nothing would end.
#[derive(Default)]
pub struct SpawnedTasks(CancelMap<Arc<str>>);

impl DeferQueue {
    pub(crate) fn new() -> Self {
        let (tx, rx) = flume::unbounded();
        Self {
            tx,
            rx,
            timers: Mutex::new(Vec::new()),
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Vec<(Arc<str>, Arc<AtomicBool>)>> {
        self.timers.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hands the callback to the dispatcher and remembers it as pending, so
    /// the two can never drift apart.
    pub(crate) fn push(&self, cb: DeferredCallback) -> Result<(), DeferredCallback> {
        let pending = (Arc::clone(&cb.plugin), Arc::clone(&cb.cancel));
        self.tx.try_send(cb).map_err(|e| e.into_inner())?;
        self.locked().push(pending);
        Ok(())
    }

    fn forget(&self, cancel: &Arc<AtomicBool>) {
        self.locked().retain(|(_, c)| !Arc::ptr_eq(c, cancel));
    }

    fn cancel_plugin(&self, plugin: &str) {
        self.locked().retain(|(owner, cancel)| {
            let doomed = &**owner == plugin;
            if doomed {
                cancel.store(true, Ordering::Release);
            }
            !doomed
        });
    }
}

/// Ends `fut` once nobody waits for its reply any more: at `deadline`, or
/// [`CANCEL_ABANDON_AFTER`] past a cancel, answering with why. The watchdog
/// cannot do this on its own, because a task parked in an await runs no Lua
/// to interrupt and renews its grace at every yield. `or`, not `race`: a
/// handler that finished in the same slice deserves to have its result
/// reported, even one whose deadline had already lapsed when it started.
pub async fn until_abandoned<T>(
    fut: impl Future<Output = T>,
    handle: &TaskHandle,
) -> Result<T, &'static str> {
    let cancel = lock_cell(handle).cancel.clone();
    let timed_out = async {
        loop {
            // Listen before reading: a `ctx:set_deadline` landing between the
            // two wakes us instead of leaving us armed on the stale deadline.
            let changed = lock_cell(handle).deadline_changed.listen();
            // Bound before the match: the guard would outlive the await.
            let deadline = lock_cell(handle).live_deadline();
            match deadline {
                Some(dl) if dl <= Instant::now() => break,
                Some(dl) => {
                    futures_lite::future::or(
                        async { _ = tokio::time::sleep_until(dl.into()).await },
                        changed,
                    )
                    .await
                }
                None => changed.await,
            }
        }
        HANDLER_TIMEOUT_MSG
    };
    let cancelled = async {
        cancel.cancelled().await;
        tokio::time::sleep(CANCEL_ABANDON_AFTER).await;
        CANCELLED_MSG
    };
    futures_lite::future::or(async { Ok(fut.await) }, async {
        Err(futures_lite::future::or(timed_out, cancelled).await)
    })
    .await
}

pub async fn run_work_fn(
    lua: &Lua,
    work_fn: &RegistryKey,
    handle: &TaskHandle,
) -> Result<LuaValue, mlua::Error> {
    let func: Function = lua.registry_value(work_fn)?;
    until_abandoned(func.call_async::<LuaValue>(()), handle)
        .await
        .unwrap_or_else(|msg| Err(mlua::Error::runtime(msg)))
}

/// Fire a `maki.defer_fn` callback: sleep, then run the Function once.
///
/// The detached scope gives the callback a task of its own, outliving the
/// one that scheduled it, and pumps job events so a `maki.system` started in
/// the callback still gets its `on_stdout` and `on_exit`.
///
/// The gate guard comes after the sleep: pending timers should pile up
/// cheaply, but the bodies compete for the `MAX_INFLIGHT_TOOLS` budget so
/// `for i=1,10000 do maki.defer_fn(f, 0) end` can't run 10k coroutines at
/// once. Errors are logged and dropped, nobody is awaiting a result.
fn spawn_deferred_callback(
    lua: &Lua,
    ex: &Rc<LocalSet>,
    gate: &Rc<InflightGate>,
    cb: DeferredCallback,
) {
    let lua = lua.clone();
    let gate = Rc::clone(gate);
    ex.spawn_local(async move {
        tokio::time::sleep(cb.delay).await;
        let _guard = gate.acquire().await;
        // Only now stop advertising the timer as pending: up to here a
        // `clear_plugin` can still flip the flag we read below, and from here
        // on the drain barrier waits for the guard instead.
        if let Some(queue) = lua.app_data_ref::<DeferQueue>() {
            queue.forget(&cb.cancel);
        }
        if cb.cancel.load(Ordering::Acquire) {
            let _ = lua.remove_registry_value(cb.func);
            return;
        }
        let run = async {
            let func: Function = lua.registry_value(&cb.func)?;
            let thread = lua.create_thread(func)?;
            thread.into_async::<()>(())?.await
        };
        let run = Narrowed::under(&lua, cb.ceiling.clone(), run);
        if let Err(e) = run_detached(&lua, run).await {
            tracing::warn!(plugin = %cb.plugin, error = %strip_traceback(&e), "defer_fn callback failed");
        }
        let _ = lua.remove_registry_value(cb.func);
    });
}

fn spawn_async_task(lua: &Lua, ex: &Rc<LocalSet>, gate: &Rc<InflightGate>, task: PendingAsyncTask) {
    if task.cancel.is_cancelled() {
        tracing::debug!("async.run: cancelled before spawn");
        lua.remove_registry_value(task.work_fn).ok();
        return;
    }
    let detached = match &task.lifetime {
        TaskLifetime::Held => false,
        TaskLifetime::Detached => true,
        TaskLifetime::Spawned(spawned) => {
            return spawn_plugin_task(lua, ex, spawned.clone(), task);
        }
    };

    let lua = lua.clone();
    let g = Rc::clone(gate);

    ex.spawn_local(async move {
        let slot = Some(
            g.admit((!detached).then(|| DrainHold::under(&g, task.exts.drain_owner())))
                .await,
        );

        let mut cell = TaskCell::new(task.cancel.clone(), task.deadline);
        cell.exts = task.exts;
        cell.lineage = task.lineage.clone();
        cell.detached = detached;
        let scope = TaskScope::new(&lua, cell);
        let handle = Arc::clone(scope.handle());
        let run = run_work_fn(&lua, &task.work_fn, &handle);
        let result = covered(
            slot,
            scope.scope_future(Narrowed::under(&lua, task.ceiling.clone(), run)),
        )
        .await;
        if let Err(e) = &result {
            tracing::debug!(error = %e, "async.run: task failed");
        }

        drop(scope);
        lua.remove_registry_value(task.work_fn).ok();
    });
}

fn spawn_failed_message(plugin: &str, error: &str) -> String {
    format!("plugin {plugin}: spawned task failed: {error}")
}

/// A spawn takes no inflight slot. It can live as long as its plugin, and a
/// slot would keep every load and unload stuck behind [`drain_barrier`].
///
/// The cancel goes first in the race, so once it fires the body never
/// resumes. A cancelled task reports nothing, since the error it may die
/// with is just the cancel.
fn spawn_plugin_task(lua: &Lua, ex: &Rc<LocalSet>, spawned: SpawnedTask, task: PendingAsyncTask) {
    let lua = lua.clone();
    ex.spawn_local(async move {
        let mut cell = TaskCell::new(task.cancel.clone(), None);
        cell.lineage = task.lineage.clone();
        cell.detached = true;
        let scope = TaskScope::new(&lua, cell);
        // On cancel, `call_async` drops the Rust future the task is parked in
        // right away. An `into_async` thread would keep it alive until the next
        // GC, and a cut `conn:write` must close its conn now, not then.
        let run = async {
            let func: Function = lua.registry_value(&task.work_fn)?;
            func.call_async::<()>(()).await
        };
        let cancelled = async {
            task.cancel.cancelled().await;
            Ok(())
        };
        let run = Narrowed::under(&lua, task.ceiling.clone(), run);
        let result = run_scoped(&lua, scope, futures_lite::future::or(cancelled, run)).await;
        if let Err(e) = result
            && !task.cancel.is_cancelled()
        {
            let error = strip_traceback(&e);
            let plugin = &spawned.plugin;
            tracing::warn!(plugin = %plugin, error = %error, "maki.async.spawn task failed");
            // The watchdog already flashed this one when it stopped the loop.
            if !error.contains(&stuck_message(plugin)) {
                host_flash(&lua, spawn_failed_message(plugin, &error));
            }
        }
        spawned.cancel(&lua);
        lua.remove_registry_value(task.work_fn).ok();
    });
}

/// Barrier for load/clear ops: drains queued `maki.async.run` tasks and
/// waits for every in-flight task, looping until both are quiescent. A bare
/// `gate.drain()` is not enough: a click handler that runs during the drain
/// can enqueue an async job into the spawn queue, which only the dispatcher
/// loop would spawn - after the barrier already passed.
///
/// Detached and spawned work is started but neither waited for nor a reason
/// to loop again, see [`TaskLifetime::Detached`].
async fn drain_barrier(
    lua: &Lua,
    ex: &Rc<LocalSet>,
    gate: &Rc<InflightGate>,
    spawn_rx: &flume::Receiver<PendingAsyncTask>,
) {
    loop {
        gate.drain().await;
        let mut held = false;
        while let Ok(task) = spawn_rx.try_recv() {
            held |= matches!(task.lifetime, TaskLifetime::Held);
            spawn_async_task(lua, ex, gate, task);
        }
        if !held {
            return;
        }
    }
}

pub struct PluginOwner {
    revision_guard: Option<Arc<maki_pack::lock::Lock>>,
    /// What this load granted the plugin. Kept past the load so a slot layer
    /// can be weighed against the authority of each call it filters.
    permissions: PluginPermissions,
}

impl PluginOwner {
    pub fn permissions(&self) -> &PluginPermissions {
        &self.permissions
    }
}

pub type PluginMap = Rc<RefCell<HashMap<Arc<str>, PluginOwner>>>;

pub struct LuaRuntime {
    /// Held for its Drop (joins the poker thread). Field order doesn't
    /// matter: the thread keeps its own `Lua` clone alive.
    _watchdog: Watchdog,
    pub lua: Lua,
    pub plugins: PluginMap,
    pub shutdown: Arc<AtomicBool>,
    bundled: BundledModules,
    /// The reference of what the domain installs, which plugins `require`.
    docs: Arc<[&'static ModuleDoc]>,
    codegen_queue: CodegenQueue,
    /// Shared with the [`HostHook`] consumer, which runs their session end,
    /// and reached by each [`TaskScope`] that ends, see [`TaskDomain`].
    domain: Arc<dyn Domain>,
}

/// The running VM's domain, for the task scopes that end on it. Weak, so
/// the domain goes with the runtime and not with the last `Lua` clone.
struct TaskDomain(Weak<dyn Domain>);

impl TaskDomain {
    fn of(lua: &Lua) -> Option<Arc<dyn Domain>> {
        lua.app_data_ref::<Self>()?.0.upgrade()
    }
}

/// Lets each task scope on {lua} call `Domain::end_task` on {domain}.
pub fn install_task_domain(lua: &Lua, domain: &Arc<dyn Domain>) {
    lua.set_app_data(TaskDomain(Arc::downgrade(domain)));
}

impl LuaRuntime {
    fn new(
        shutdown: Arc<AtomicBool>,
        bundled: &'static Bundled,
        jit: bool,
        layered: Arc<LayeredTools>,
        mut domain: Box<dyn Domain>,
    ) -> Result<Self, PluginError> {
        let lua = Lua::new();
        let compiler = install_compiler(&lua, jit);
        lua.set_memory_limit(LUA_MEMORY_LIMIT)
            .map_err(|e| PluginError::Lua {
                plugin: "<init>".to_owned(),
                source: e,
            })?;
        let watchdog = Watchdog::spawn(&lua, Arc::clone(&shutdown));

        let globals = lua.globals();
        for name in &["require", "io", "package"] {
            globals
                .set(*name, LuaValue::Nil)
                .map_err(|e| PluginError::Lua {
                    plugin: "<init>".to_owned(),
                    source: e,
                })?;
        }
        drop(globals);
        lua.sandbox(true).map_err(|e| PluginError::Lua {
            plugin: "<init>".to_owned(),
            source: e,
        })?;

        lua.set_app_data(JobStore::new());
        lua.set_app_data(SpawnQueue::new());
        lua.set_app_data(DeferQueue::new());
        lua.set_app_data(SpawnedTasks::default());
        lua.set_app_data(crate::api::top::NotifyHandler::default());
        lua.set_app_data(PluginEnvs::default());
        lua.set_app_data(PluginOptionSpecs::default());
        lua.set_app_data(crate::api::pack::PackStore::default());
        lua.set_app_data(bundled);
        lua.set_app_data(AutocmdStore::default());
        lua.set_app_data(SlotStore::new(layered));
        domain.install(&lua).map_err(|e| PluginError::Lua {
            plugin: format!("<{}>", domain.name()),
            source: e,
        })?;
        let docs = compose_docs([domain.docs(), crate::facade::DOCS]).into();
        let domain: Arc<dyn Domain> = domain.into();
        install_task_domain(&lua, &domain);

        let plugins: PluginMap = Rc::new(RefCell::new(HashMap::new()));
        {
            let lua = lua.clone();
            let plugins = Rc::clone(&plugins);
            crate::api::slot::set_layer_delegation(move |layer, cost, slot| {
                layer_delegation(&lua, &plugins, cost)(layer, slot)
            });
        }

        Ok(Self {
            _watchdog: watchdog,
            lua,
            plugins,
            shutdown,
            bundled: BundledModules {
                source: bundled,
                compiler,
                bytecode: Arc::default(),
            },
            docs,
            codegen_queue: jit.then(Arc::default),
            domain,
        })
    }

    /// Returns false when there is nothing left to compile, so the caller can
    /// stop polling.
    fn codegen_step(&self) -> bool {
        let Some(queue) = self.codegen_queue.as_ref() else {
            return false;
        };
        let Some(func) = queue.lock().expect("codegen queue").pop() else {
            return false;
        };
        let compiled = unsafe {
            self.lua
                .exec_raw::<()>(func, |state| ffi::luau_codegen_compile(state, -1))
        };
        if let Err(e) = compiled {
            tracing::debug!(error = %e, "native codegen failed");
        }
        true
    }

    fn drop_plugin_keys(&self, name: &str) -> Option<Arc<maki_pack::lock::Lock>> {
        with_jobs(&self.lua, |store| {
            store.kill_owner(&self.lua, &JobOwner::Plugin(Arc::from(name)));
            store.detach_plugin_callbacks(&self.lua, name);
        });
        if let Some(mut store) = self.lua.app_data_mut::<PluginOptionSpecs>() {
            store.remove(name);
        }
        if let Some(mut store) = self.lua.app_data_mut::<AutocmdStore>() {
            store.clear_plugin(name);
        }
        if let Some(mut store) = self.lua.app_data_mut::<SlotStore>() {
            store.clear_plugin(name);
        }
        let owner = self.plugins.borrow_mut().remove(name)?;
        host_unloaded(&self.lua, name);
        owner.revision_guard
    }

    /// The env {plugin}'s chunks would run in, built without loading it: no
    /// `require` root and no options.
    pub fn plugin_env(
        &self,
        plugin: Arc<str>,
        reach: &PluginReach,
        grant: &dyn Any,
    ) -> Result<mlua::Table, mlua::Error> {
        let maki = create_maki_global(
            &self.lua,
            self.domain.as_ref(),
            plugin,
            reach,
            PluginOpts::default(),
            grant,
            LoadKind::Plugin,
        )?;
        self.build_env(maki, None)
    }

    fn build_env(
        &self,
        maki: mlua::Table,
        require_root: Option<RequireRoot>,
    ) -> Result<mlua::Table, mlua::Error> {
        let env = self.lua.create_table()?;
        env.set("maki", maki)?;

        if require_root.is_some() || !self.bundled.source.plugins().is_empty() {
            let require_fn = self.create_require_fn(&env, require_root)?;
            env.set("require", require_fn)?;
        }

        let meta = self.lua.create_table()?;
        meta.set("__index", self.lua.globals())?;
        env.set_metatable(Some(meta))?;
        env.set_safeenv(true);
        Ok(env)
    }

    /// Bundled dirs go first so plugins can `require()` shared modules
    /// (like `maki.truncate`) without touching the filesystem.
    fn create_require_fn(
        &self,
        env: &mlua::Table,
        require_root: Option<RequireRoot>,
    ) -> Result<Function, mlua::Error> {
        let loader = ModuleLoader {
            bundled: self.bundled.clone(),
            docs: Arc::clone(&self.docs),
            require_root,
            env: env.clone(),
            codegen: self.codegen_queue.clone(),
            loaded: self.lua.create_table()?,
            loading: self.lua.create_table()?,
        };

        self.lua
            .create_function(move |lua, modname: String| loader.require(lua, &modname))
    }

    fn check_opts_consumed(&self, name: &str, opts: &PluginOpts) -> Result<(), mlua::Error> {
        if opts.is_empty() {
            return Ok(());
        }
        let store = self.lua.app_data_ref::<PluginOptionSpecs>();
        crate::api::options::check_unknown_opts(
            name,
            opts,
            store.as_ref().and_then(|s| s.get(name)),
        )
    }

    /// The function and argument of the custom loader {declared} names for
    /// {component}, called with the package's spec, that component's data,
    /// and {plugin_dir}.
    fn pack_loader(
        &self,
        declared: &crate::api::pack::Declared,
        component: Component,
        plugin_dir: Option<&Path>,
    ) -> Result<(Function, Table), mlua::Error> {
        let Some(crate::api::pack::Policy {
            load: crate::api::pack::LoadMode::Custom(loader),
            data,
            ..
        }) = declared.policy(component)
        else {
            return Err(mlua::Error::runtime(NOT_A_CUSTOM_LOAD));
        };
        let function = self.lua.registry_value::<Function>(loader.as_ref())?;
        let argument = self.lua.create_table()?;
        argument.set(
            "spec",
            crate::api::pack::spec_to_lua(&self.lua, &declared.spec, data.as_ref())?,
        )?;
        // The same directory the chunks would load from, read back rather
        // than passed twice.
        argument.set("path", plugin_dir.map(|dir| dir.display().to_string()))?;
        Ok((function, argument))
    }

    /// Runs one step against {env}: a chunk's top level, or a custom loader
    /// with the env as its own.
    async fn run_step(
        &self,
        step: &LoadStep,
        env: &Table,
        plugin_dir: Option<&Path>,
    ) -> Result<(), mlua::Error> {
        let (function, argument) = match step {
            LoadStep::Chunk(chunk) => {
                let function = match chunk.origin {
                    ChunkOrigin::Bundled => self
                        .lua
                        .load(chunk.source.as_str())
                        .set_name(chunk.name.as_str()),
                    ChunkOrigin::User => load_user_source(&self.lua, &chunk.name, &chunk.source),
                }
                .set_environment(env.clone())
                .into_function()?;
                (function, None)
            }
            LoadStep::Loader { declared } => {
                let (function, argument) =
                    self.pack_loader(declared, self.domain.component(), plugin_dir)?;
                function.set_environment(env.clone())?;
                (function, Some(argument))
            }
        };
        queue_codegen(&self.codegen_queue, &function);
        let arguments: MultiValue = argument.into_iter().map(LuaValue::Table).collect();
        function.call_async::<()>(arguments).await
    }

    async fn load_source(
        &mut self,
        name: Arc<str>,
        steps: &[LoadStep],
        context: LoadContext,
        config: Option<ConfigLoad<'_>>,
    ) -> LoadResult {
        let LoadContext {
            plugin_dir,
            permissions,
            opts,
            revision_guard,
            package,
            grant,
        } = context;
        let map_err = |e: mlua::Error| PluginError::Lua {
            plugin: name.to_string(),
            source: e,
        };

        let pack_ops_checkpoint = with_packs(&self.lua, |packs| packs.pending.len());

        let require_root = plugin_dir.as_ref().and_then(|dir| match config {
            Some(_) => Some(RequireRoot::trusted(dir.join("lua"))),
            None => RequireRoot::sandboxed(dir),
        });
        let reach = PluginReach::new(permissions.clone());
        let maki = create_maki_global(
            &self.lua,
            self.domain.as_ref(),
            Arc::clone(&name),
            &reach,
            opts.clone(),
            grant.as_ref(),
            match config {
                Some(_) => LoadKind::Config,
                None if package => LoadKind::Package,
                None => LoadKind::Plugin,
            },
        )
        .map_err(map_err)?;

        if let Some(config) = config {
            let setup_fn = crate::api::util::setup::create_setup_fn(
                &self.lua,
                Arc::clone(config.store),
                config.scope.component() == Component::Ui,
            )
            .map_err(map_err)?;
            maki.set("setup", setup_fn).map_err(map_err)?;

            let pack = if config.scope.declares_packages() {
                crate::api::pack::create_pack_table(&self.lua, config.scope.component())
            } else {
                crate::api::pack::create_pack_read_table(&self.lua)
            }
            .map_err(map_err)?;
            maki.set("pack", pack).map_err(map_err)?;
        }
        let env = self.build_env(maki, require_root).map_err(map_err)?;
        if let Some(mut envs) = self.lua.app_data_mut::<PluginEnvs>() {
            envs.0.insert(Arc::clone(&name), env.to_pointer() as usize);
        }

        drop(self.drop_plugin_keys(&name));
        if let Some(spawned) = self.lua.app_data_ref::<SpawnedTasks>() {
            spawned.0.revive(&name);
        }
        self.domain.reload(&self.lua, &name);

        // Package files share an environment and commit together.
        let exec_result = as_plugin_load(async {
            for step in steps {
                self.run_step(step, &env, plugin_dir.as_deref()).await?;
            }
            Ok(())
        })
        .await;

        // Checked once, after the last step: an option that a later step
        // reads must not be reported as unused by an earlier one.
        let exec_result = exec_result.and_then(|()| self.check_opts_consumed(&name, &opts));
        if let Err(e) = exec_result {
            self.rollback_load(&name, pack_ops_checkpoint);
            return Err(map_err(e));
        }

        let scope = LoadScope {
            plugin: &name,
            reach: &reach.owned_by(Arc::clone(&name)),
            grant: grant.as_ref(),
        };
        // Publish the plugin only after its binding commits.
        let committed = self.domain.commit_load(&self.lua, &scope);
        if let Err(e) = committed {
            self.rollback_load(&name, pack_ops_checkpoint);
            return Err(e);
        }
        self.plugins.borrow_mut().insert(
            Arc::clone(&name),
            PluginOwner {
                revision_guard,
                permissions,
            },
        );
        if package {
            with_packs(&self.lua, |packs| packs.active.insert(name.to_string()));
        }

        Ok(())
    }

    /// Undoes a load that failed after its chunks ran: the tools it was about
    /// to register, the package operations it queued, the providers it staged,
    /// and the commands, keymaps, hints and slots it published on the way.
    ///
    /// The provider grant is kept: a package whose load failed still owns its
    /// slugs, so the catalog client cannot take them over. Only the explicit
    /// unload releases a grant.
    fn rollback_load(&self, plugin: &str, pack_ops: usize) {
        with_packs(&self.lua, |packs| packs.pending.truncate(pack_ops));
        self.clear_plugin(plugin);
        // It may have pushed marks before it failed.
        host_unloaded(&self.lua, plugin);
        self.domain.rollback_load(&self.lua, plugin);
    }

    fn clear_plugin(&self, plugin: &str) {
        crate::api::fs::clear_plugin_files(plugin);
        if let Some(queue) = self.lua.app_data_ref::<DeferQueue>() {
            queue.cancel_plugin(plugin);
        }
        if let Some(spawned) = self.lua.app_data_ref::<SpawnedTasks>() {
            spawned.0.cancel(Arc::from(plugin));
        }
        crate::api::top::clear_notify_handler(&self.lua, plugin);
        if let Some(mut envs) = self.lua.app_data_mut::<PluginEnvs>() {
            envs.0.remove(plugin);
        }
        crate::api::net::close_plugin_conns(plugin);
        let revision_guard = self.drop_plugin_keys(plugin);
        with_packs(&self.lua, |packs| packs.active.remove(plugin));
        self.domain.clear_plugin(&self.lua, plugin);
        drop(revision_guard);
    }

    /// Runs {source} as the config file of {scope}, as an entry of its
    /// component, granted the config dir's manifest table for it.
    async fn run_init_lua(
        &mut self,
        source: &str,
        scope: ConfigScope,
        plugin_dir: Option<PathBuf>,
        grant: DomainGrant,
    ) -> Result<Setup, PluginError> {
        let config_store: ConfigStore = Arc::default();
        let component = scope.component();
        let permissions = load_plugin_permissions(plugin_dir.as_deref(), component);
        let chunk = LoadChunk::new(scope.label(), source);
        self.load_source(
            Arc::from(scope.label()),
            &[chunk.into()],
            LoadContext::plain(plugin_dir, permissions, grant),
            Some(ConfigLoad {
                store: &config_store,
                scope: &scope,
            }),
        )
        .await?;
        Ok(std::mem::take(&mut *config_store.lock().unwrap()))
    }
}

pub fn strip_traceback(err: &mlua::Error) -> String {
    match err {
        mlua::Error::CallbackError { cause, .. } => {
            let mut inner = cause;
            while let mlua::Error::CallbackError { cause, .. } = inner.as_ref() {
                inner = cause;
            }
            inner.to_string()
        }
        other => other.to_string(),
    }
}

/// Whether a plugin may layer this slot at all.
///
/// A layer on `tool.<name>.input` rewrites the call the tool then makes, so it
/// borrows that call's authority and has to already hold it. Without this, a
/// plugin denied `run` could turn any bash command into its own. It also sees
/// conversation content, and at the output stage rewrites it, which
/// [`LayerCost::Host`] prices on top. A layer on a slot another plugin
/// declared arrives here as the price that slot's owner named at
/// `declare_slot`, or as [`LayerCost::TOTAL`] when it named none.
///
/// Decided when the chain fires rather than when the layer is registered,
/// because a layer may legitimately be set before its target tool exists, and
/// permissions a reload narrows take effect on the very next call. Host slots
/// reach it through [`run_host_chain`], plugin slots through the gate the
/// runtime installs for them, so both kinds gate in one place.
///
/// A skipped layer is a plugin silently not doing its job, so the user hears
/// about it once per plugin per slot and gap, naming every key to grant. A
/// gap that changes, say after granting half of it, is news and warns again.
///
/// A free layer passes before the plugin is looked up. A plugin joins the map
/// only once its load finishes, so a layer its top-level code set before an
/// await would otherwise be dropped from every chain that fires meanwhile.
///
/// A layer set under a [`Ceiling`] has to fit under that too. Granting more
/// would not help it, so it is skipped without a warning.
///
/// [`run_host_chain`]: crate::api::slot::run_host_chain
pub fn layer_delegation<'a>(
    lua: &'a Lua,
    plugins: &'a PluginMap,
    cost: LayerCost<'a>,
) -> impl Fn(&SlotLayer, &str) -> bool + 'a {
    let free = cost.is_free();
    move |layer, slot| {
        if free {
            return true;
        }
        let plugin = &*layer.plugin;
        let missing: Vec<Permission> = match plugins.borrow().get(plugin) {
            Some(owner) => {
                let mut missing = cost.missing(&owner.permissions).peekable();
                if missing.peek().is_none() {
                    let capped = layer
                        .ceiling
                        .as_ref()
                        .is_some_and(|ceiling| cost.missing(&ceiling.0).next().is_some());
                    if capped {
                        tracing::debug!(
                            plugin,
                            slot,
                            ?cost,
                            "slot layer skipped: outside its ceiling"
                        );
                    }
                    return !capped;
                }
                missing.collect()
            }
            None => {
                tracing::debug!(plugin, slot, ?cost, "slot layer skipped: not loaded");
                return false;
            }
        };
        let gap = format!("{slot} {}", quoted_list(&missing));
        warn_plugin_once(lua, plugin, &gap, || {
            layer_skipped_message(plugin, slot, &missing)
        });
        false
    }
}

/// The core's handle on a running VM thread. The domain' handles live
/// beside it in the host.
pub struct LuaThread {
    pub tx: flume::Sender<Request>,
    pub prio_tx: flume::Sender<Request>,
    pub join: Option<JoinHandle<()>>,
    pub shutdown: Arc<AtomicBool>,
}

/// The two request lanes a VM thread reads, made before its domain so a
/// domain can hold their senders.
pub struct RequestLanes {
    pub tx: flume::Sender<Request>,
    pub prio_tx: flume::Sender<Request>,
    rx: flume::Receiver<Request>,
    prio_rx: flume::Receiver<Request>,
}

impl Default for RequestLanes {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestLanes {
    pub fn new() -> Self {
        let (tx, rx) = flume::unbounded();
        let (prio_tx, prio_rx) = flume::unbounded();
        Self {
            tx,
            prio_tx,
            rx,
            prio_rx,
        }
    }
}

/// Lua lives on its own OS thread (no Send needed). Tokio
/// drives async, load/clear requests wait for in-flight tools. The slot
/// mechanism publishes what is layered to {layered}.
pub fn spawn(
    lanes: RequestLanes,
    domain: Box<dyn Domain>,
    layered: Arc<LayeredTools>,
    bundled: &'static Bundled,
    jit: bool,
) -> Result<LuaThread, PluginError> {
    let RequestLanes {
        tx,
        prio_tx,
        rx,
        prio_rx,
    } = lanes;
    let shutdown: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let shutdown_thread = Arc::clone(&shutdown);
    let (init_tx, init_rx) = flume::bounded::<Result<(), PluginError>>(1);

    let runtime = Handle::current();
    let handle = thread::Builder::new()
        .name("maki-lua".to_owned())
        .spawn(move || {
            let _entered = runtime.enter();
            let mut rt = match LuaRuntime::new(shutdown_thread, bundled, jit, layered, domain) {
                Ok(r) => {
                    let _ = init_tx.send(Ok(()));
                    r
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            let ex = Rc::new(LocalSet::new());
            {
                let lua = rt.lua.clone();
                // Weak, or the executor would own a task that owns it back.
                let host = Rc::downgrade(&ex);
                ex.spawn_local(async move {
                    // One task per job, so a callback parked on a prompt or a
                    // request holds back only its own job's events.
                    let delivering = Rc::new(RefCell::new(HashSet::new()));
                    loop {
                        if let Some(host) = host.upgrade() {
                            for job_id in with_jobs(&lua, |s| s.plugin_jobs_with_events()) {
                                if delivering.borrow_mut().insert(job_id) {
                                    let delivering = Rc::clone(&delivering);
                                    host.spawn_local(deliver_job(lua.clone(), job_id, delivering));
                                }
                            }
                        }
                        tokio::time::sleep(DISPATCH_POLL_INTERVAL).await;
                    }
                });
            }
            let gate = Rc::new(InflightGate::new(rt.lua.clone()));
            let (hook_tx, hook_rx) = flume::unbounded::<HostHook>();
            {
                let lua = rt.lua.clone();
                let domain = Arc::clone(&rt.domain);
                let gate = Rc::clone(&gate);
                ex.spawn_local(async move {
                    while let Ok(hook) = hook_rx.recv_async().await {
                        // Counted as in-flight so a plugin reload waiting on
                        // `drain_barrier` cannot land mid-handler.
                        let _guard = GateGuard::new(&gate);
                        run_host_hook(&lua, domain.as_ref(), hook).await;
                    }
                });
            }
            let spawn_rx = rt
                .lua
                .app_data_ref::<SpawnQueue>()
                .expect("spawn queue installed at init")
                .rx
                .clone();
            let defer_rx = rt
                .lua
                .app_data_ref::<DeferQueue>()
                .expect("defer queue installed at init")
                .rx
                .clone();

            let mut codegen_armed = false;

            runtime.block_on(ex.run_until(async {
                rt.domain.start(&rt.lua);
                loop {
                    while let Ok(task) = spawn_rx.try_recv() {
                        spawn_async_task(&rt.lua, &ex, &gate, task);
                    }
                    while let Ok(cb) = defer_rx.try_recv() {
                        spawn_deferred_callback(&rt.lua, &ex, &gate, cb);
                    }
                    // Nothing to serve, so spend the lull on native codegen.
                    // One chunk per pass with a yield in between, so no request
                    // or spawned task ever waits for more than a single chunk.
                    if codegen_armed
                        && prio_rx.is_empty()
                        && rx.is_empty()
                        && spawn_rx.is_empty()
                        && rt.codegen_step()
                    {
                        tokio::task::yield_now().await;
                        continue;
                    }
                    // Biased: user-initiated requests (commands, keybinds) jump
                    // ahead of bulk work like session restores so the UI stays
                    // snappy, and queued `maki.async.run` tasks jump ahead of
                    // plain requests. `defer_rx` is selected on so a queued
                    // `maki.defer_fn` wakes the loop instead of stalling
                    // behind the next unrelated request.
                    let next = futures_lite::future::or(
                        async { prio_rx.recv_async().await.map(Some) },
                        futures_lite::future::or(
                            async {
                                let task = spawn_rx.recv_async().await?;
                                spawn_async_task(&rt.lua, &ex, &gate, task);
                                Ok(None)
                            },
                            futures_lite::future::or(
                                async {
                                    let cb = defer_rx.recv_async().await?;
                                    spawn_deferred_callback(&rt.lua, &ex, &gate, cb);
                                    Ok(None)
                                },
                                async { rx.recv_async().await.map(Some) },
                            ),
                        ),
                    )
                    .await;
                    let msg = match next {
                        Ok(Some(m)) => m,
                        Ok(None) => {
                            tokio::task::yield_now().await;
                            continue;
                        }
                        Err(_) => break,
                    };
                    match msg {
                        Request::Shutdown => break,
                        Request::WarmJit => codegen_armed = true,
                        Request::LoadSource {
                            name,
                            steps,
                            context,
                            reply,
                        } => {
                            drain_barrier(&rt.lua, &ex, &gate, &spawn_rx).await;
                            let res = rt
                                .load_source(Arc::clone(&name), &steps, context, None)
                                .await;
                            let _ = reply.send(res);
                        }
                        Request::TakePackOps { reply } => {
                            let ops =
                                with_packs(&rt.lua, |packs| std::mem::take(&mut packs.pending));
                            let _ = reply.send(ops);
                        }
                        Request::SealPackOps { reply } => {
                            let ops = with_packs(&rt.lua, |packs| {
                                packs.drained = true;
                                std::mem::take(&mut packs.pending)
                            });
                            let _ = reply.send(ops);
                        }
                        Request::ClearPlugin { plugin, reply } => {
                            drain_barrier(&rt.lua, &ex, &gate, &spawn_rx).await;
                            rt.clear_plugin(&plugin);
                            rt.domain.unload(&rt.lua, &plugin);
                            let _ = reply.send(());
                        }
                        Request::RunInitLua {
                            source,
                            scope,
                            plugin_dir,
                            grant,
                            reply,
                        } => {
                            drain_barrier(&rt.lua, &ex, &gate, &spawn_rx).await;
                            let res = rt.run_init_lua(&source, scope, plugin_dir, grant).await;
                            let _ = reply.send(res);
                        }
                        Request::CollectPluginOptions { reply } => {
                            let _ = reply.send(collect_plugin_options(&rt.lua));
                        }
                        Request::CollectPackages { reply } => {
                            let declared = with_packs(&rt.lua, |packs| packs.specs.clone());
                            let _ = reply.send(declared);
                        }
                        Request::CollectPackageContext { reply } => {
                            let context = with_packs(&rt.lua, |packs| {
                                (packs.specs.clone(), packs.active.clone())
                            });
                            let _ = reply.send(context);
                        }
                        Request::FireAutocmd { event, data } => {
                            let _ = hook_tx.send(HostHook::Autocmd { event, data });
                        }
                        Request::EndSession(end) => {
                            let _ = hook_tx.send(HostHook::EndSession(end));
                        }
                        Request::Domain(request) => {
                            let vm = Vm {
                                rt: &rt,
                                ex: &ex,
                                gate: &gate,
                            };
                            request.serve(&vm).await;
                        }
                    }
                }
            }));
            // A host's clones of the senders can still be live, so
            // dropping the receivers alone does not free
            // queued requests. Drain them so their reply channels drop and
            // no caller blocks on a dead host.
            for _ in rx.drain() {}
            for _ in prio_rx.drain() {}
            // Each pooled client owns a curl thread and its connection cache.
            // Nothing else hands them back, so a host that fetched once would
            // keep its sockets open for the rest of the process.
            crate::api::net::clear_client_pool();
        })
        .map_err(|e| PluginError::Io {
            path: PathBuf::from("lua-thread"),
            source: e,
        })?;

    init_rx.recv().map_err(|_| PluginError::Lua {
        plugin: "<init>".to_owned(),
        source: mlua::Error::runtime("lua thread exited before init completed"),
    })??;

    Ok(LuaThread {
        tx,
        prio_tx,
        join: Some(handle),
        shutdown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::r#fn::JobSpec;
    use crate::host_sink::RecordingSink;
    use futures_lite::future::poll_once;
    use maki_types::cancel::CancelTrigger;
    use std::task::Poll;
    use test_case::test_case;

    /// A job printing faster than we deliver never runs its channel dry, so the
    /// last pass a scope makes before reaping it has to stop on its own.
    #[test]
    fn the_bounded_drain_stops_on_an_endless_stream() {
        let lua = Lua::new();
        let mut offered = 0usize;
        block_on(deliver_pending(&lua, FINAL_DRAIN_BUDGET, || {
            offered += 1;
            (offered <= FINAL_DRAIN_BUDGET * 2).then(|| (1, JobEvent::Stdout("spam".into())))
        }));
        assert_eq!(offered, FINAL_DRAIN_BUDGET);
    }

    #[test]
    fn task_scope_clears_only_its_jobs_on_drop() {
        let lua = Lua::new();
        let scope = TaskScope::new(&lua, task_cell());
        let task_owner = JobOwner::Task(lock_cell(scope.handle()).id);
        let plugin_owner = JobOwner::Plugin(Arc::from("test-plugin"));
        with_jobs(&lua, |store| {
            store
                .start(JobSpec::new(task_owner.clone(), "exit 0"))
                .unwrap();
            store
                .start(JobSpec::new(plugin_owner.clone(), "exit 0"))
                .unwrap();
        });

        drop(scope);

        with_jobs(&lua, |store| {
            assert!(store.is_empty(&task_owner));
            assert!(!store.is_empty(&plugin_owner));
            store.kill_owner(&lua, &plugin_owner);
        });
    }

    fn task_cell() -> TaskCell {
        TaskCell::new(CancelToken::none(), None)
    }

    #[test]
    fn defer_queue_cancels_only_the_doomed_plugins_timers() {
        let lua = Lua::new();
        let queue = DeferQueue::new();
        let timer = |plugin: &str| {
            let cancel = Arc::new(AtomicBool::new(false));
            let queued = queue.push(DeferredCallback {
                func: lua.create_registry_value(true).unwrap(),
                delay: Duration::ZERO,
                plugin: Arc::from(plugin),
                cancel: Arc::clone(&cancel),
                ceiling: None,
            });
            assert!(queued.is_ok(), "the queue is unbounded");
            cancel
        };
        let doomed = timer("memory");
        let bystander = timer("other");

        queue.cancel_plugin("memory");
        assert!(doomed.load(Ordering::Acquire));
        assert!(!bystander.load(Ordering::Acquire));

        queue.forget(&bystander);
        queue.cancel_plugin("other");
        assert!(
            !bystander.load(Ordering::Acquire),
            "a timer past the gate is running, not pending"
        );
    }

    fn gate() -> InflightGate {
        InflightGate::new(Lua::new())
    }

    const MIB: usize = 1024 * 1024;
    const PACER_HEADROOM: usize = 4 * MIB;
    const PACER_LIVE: usize = 3 * MIB;
    const PACER_GARBAGE: usize = MIB;

    #[test]
    fn gc_pacer_rescans_a_big_live_heap_only_once_it_grows() {
        let lua = Lua::new();
        // Luau's own GC would free memory behind the pacer's back. A full
        // collect turns it back on, so it gets stopped again after each one.
        lua.gc_stop();
        let pacer = GcPacer::new(lua.clone(), lua.used_memory() + PACER_HEADROOM);
        let alloc = |global: &str, bytes: usize| {
            lua.load(format!("{global} = string.rep('x', {bytes})"))
                .exec()
                .unwrap()
        };

        alloc("live", PACER_LIVE);
        assert!(pacer.relieve_pressure(), "past halfway to the limit");
        lua.gc_stop();
        let live = lua.used_memory();
        assert!(!pacer.relieve_pressure(), "nothing grew since the collect");

        alloc("garbage", PACER_GARBAGE);
        lua.load("garbage = nil").exec().unwrap();
        assert!(pacer.relieve_pressure(), "garbage pushed past the new mark");
        assert!(
            lua.used_memory() < live + PACER_GARBAGE,
            "the garbage is gone, the live heap kept"
        );
    }

    #[test]
    fn inflight_gate_drain_requires_all_guards_dropped() {
        let ex = LocalSet::new();
        block_on_or_fail(ex.run_until(async {
            let g = Rc::new(gate());
            let first = GateGuard::new(&g);
            let second = GateGuard::new(&g);
            let g2 = Rc::clone(&g);
            let waiter = ex.spawn_local(async move { g2.drain().await });
            tokio::task::yield_now().await;
            assert!(!waiter.is_finished());
            drop(first);
            tokio::task::yield_now().await;
            assert!(!waiter.is_finished());
            drop(second);
            waiter.await.unwrap();
        }));
    }

    /// Enough turns for anything not parked on a real wait to finish.
    const SETTLE_YIELDS: usize = 8;

    #[test]
    fn drain_waits_for_held_work_queued_behind_detached_slots() {
        let ex = LocalSet::new();
        block_on_or_fail(ex.run_until(async {
            let g = Rc::new(gate());
            let mut detached = Vec::new();
            for _ in 0..MAX_INFLIGHT_TOOLS {
                detached.push(g.admit(None).await);
            }
            g.drain().await;

            let g2 = Rc::clone(&g);
            let queued = ex.spawn_local(async move { drop(g2.acquire().await) });
            let g3 = Rc::clone(&g);
            let waiter = ex.spawn_local(async move { g3.drain().await });
            for _ in 0..SETTLE_YIELDS {
                tokio::task::yield_now().await;
            }
            assert!(!waiter.is_finished());

            drop(detached);
            queued.await.unwrap();
            waiter.await.unwrap();
        }));
    }

    #[test]
    fn inflight_gate_blocks_at_max_capacity() {
        let ex = LocalSet::new();
        block_on(ex.run_until(async {
            let g = Rc::new(gate());
            for _ in 0..MAX_INFLIGHT_TOOLS {
                g.increment();
            }
            let g2 = Rc::clone(&g);
            let waiter = ex.spawn_local(async move { g2.wait_below(MAX_INFLIGHT_TOOLS).await });
            tokio::task::yield_now().await;
            assert!(!waiter.is_finished());
            g.decrement();
            waiter.await.unwrap();
        }));
    }

    #[test]
    fn acquire_caps_concurrent_holders_even_when_spawned_in_bulk() {
        let ex = LocalSet::new();
        block_on(ex.run_until(async {
            let g = Rc::new(gate());
            let (release_tx, release_rx) = flume::unbounded::<()>();
            let tasks: Vec<_> = (0..MAX_INFLIGHT_TOOLS + 1)
                .map(|_| {
                    let g = Rc::clone(&g);
                    let release_rx = release_rx.clone();
                    ex.spawn_local(async move {
                        let _guard = g.acquire().await;
                        release_rx.recv_async().await.ok();
                    })
                })
                .collect();
            for _ in 0..MAX_INFLIGHT_TOOLS + 2 {
                tokio::task::yield_now().await;
            }
            assert_eq!(g.count.get(), MAX_INFLIGHT_TOOLS);
            drop(release_tx);
            for t in tasks {
                t.await.unwrap();
            }
            assert_eq!(g.count.get(), 0);
        }));
    }

    /// The cover belongs to the polls of the covered work, not to the thread.
    /// A sibling that runs while that work is parked is a call of its own and
    /// still owes a slot.
    #[test]
    fn slot_cover_does_not_leak_into_a_sibling_task() {
        let ex = LocalSet::new();
        block_on(ex.run_until(async {
            let (release_tx, release_rx) = flume::unbounded::<()>();
            let holder = ex.spawn_local(covered(None, async move {
                let entered = under_inflight_slot();
                release_rx.recv_async().await.ok();
                (entered, under_inflight_slot())
            }));
            tokio::task::yield_now().await;
            assert!(
                !ex.spawn_local(async { under_inflight_slot() })
                    .await
                    .unwrap()
            );
            drop(release_tx);
            assert_eq!(holder.await.unwrap(), (true, true));
            assert!(!under_inflight_slot());
        }));
    }

    #[test_case(true, false, HANDLER_TIMEOUT_MSG ; "lapsed_deadline")]
    #[test_case(false, true, CANCELLED_MSG ; "cancelled")]
    #[test_case(true, true, CANCELLED_MSG ; "cancel_outranks_deadline")]
    fn admission_gives_up_once_nobody_waits_for_the_reply(
        lapsed: bool,
        cancelled: bool,
        expected: &str,
    ) {
        let ex = LocalSet::new();
        block_on(ex.run_until(async {
            let g = Rc::new(gate());
            for _ in 0..MAX_INFLIGHT_TOOLS {
                g.increment();
            }
            let (trigger, token) = CancelToken::new();
            if cancelled {
                trigger.cancel();
            }
            let deadline = lapsed.then(|| Instant::now() - Duration::from_secs(1));
            let admitted = g.acquire_before_abandoned(&token, deadline).await;
            assert_eq!(admitted.err(), Some(expected));
            assert_eq!(g.count.get(), MAX_INFLIGHT_TOOLS);
        }));
    }

    fn enqueue_test_lua() -> Lua {
        let lua = Lua::new();
        lua.set_app_data(SpawnQueue::new());
        lua
    }

    fn enqueue_dummy(lua: &Lua) -> RegistryKey {
        let func = lua.create_function(|_, _: ()| Ok(())).unwrap();
        lua.create_registry_value(func).unwrap()
    }

    #[test]
    fn gate_guard_tracks_count_via_raii() {
        let g = Rc::new(gate());
        let g1 = GateGuard::new(&g);
        let g2 = GateGuard::new(&g);
        assert_eq!(g.count.get(), 2);
        drop(g1);
        assert_eq!(g.count.get(), 1);
        drop(g2);
        assert_eq!(g.count.get(), 0);
    }

    #[test]
    fn enqueue_async_task_missing_spawn_queue_errors() {
        let lua = Lua::new();
        let key = lua
            .create_registry_value(lua.create_function(|_, _: ()| Ok(())).unwrap())
            .unwrap();
        let err = enqueue_async_task(&lua, SPAWN_PLUGIN, key).unwrap_err();
        assert!(err.to_string().contains(SPAWN_QUEUE_NOT_INIT));
    }

    #[test]
    fn enqueue_async_task_routes_to_inline_spawn_when_set() {
        let lua = enqueue_test_lua();
        let scope = TaskScope::new(&lua, TaskCell::new(CancelToken::none(), None));
        let _active = scope.enter();
        lock_cell(scope.handle()).inline_spawn = Some(Vec::new());

        enqueue_async_task(&lua, SPAWN_PLUGIN, enqueue_dummy(&lua)).unwrap();

        assert!(
            lua.app_data_ref::<SpawnQueue>().unwrap().rx.is_empty(),
            "task must not reach the global queue"
        );
        let cell = lock_cell(scope.handle());
        assert_eq!(cell.inline_spawn.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn enqueue_async_task_works_without_task_ctx() {
        let lua = enqueue_test_lua();
        enqueue_async_task(&lua, SPAWN_PLUGIN, enqueue_dummy(&lua)).unwrap();

        let queue = lua.app_data_ref::<SpawnQueue>().unwrap();
        let queued = queue.rx.try_recv().unwrap();
        assert!(queued.exts.is_empty());
    }

    #[test]
    fn enqueue_async_task_inherits_cancel_token() {
        let lua = enqueue_test_lua();
        let (trigger, token) = CancelToken::new();
        let scope = TaskScope::new(&lua, TaskCell::new(token, None));
        let _active = scope.enter();
        enqueue_async_task(&lua, SPAWN_PLUGIN, enqueue_dummy(&lua)).unwrap();

        let queue = lua.app_data_ref::<SpawnQueue>().unwrap();
        let queued = queue.rx.try_recv().unwrap();
        assert!(!queued.cancel.is_cancelled());
        trigger.cancel();
        assert!(
            queued.cancel.is_cancelled(),
            "async task should inherit parent cancel"
        );
    }

    /// Without this a `run_command` cycle could hop through `maki.async.run`
    /// and start over at depth 0, and a window opened from it would land in
    /// the wrong tab.
    #[test]
    fn enqueue_async_task_inherits_lineage() {
        let lua = enqueue_test_lua();
        let lineage = Lineage {
            command_depth: 3,
            session: Some(MakiId::generate()),
            working_dir: Some(Arc::from(Path::new("/session"))),
        };
        let mut cell = TaskCell::new(CancelToken::none(), None);
        cell.lineage = lineage.clone();
        let scope = TaskScope::new(&lua, cell);
        let _active = scope.enter();
        enqueue_async_task(&lua, SPAWN_PLUGIN, enqueue_dummy(&lua)).unwrap();

        let queue = lua.app_data_ref::<SpawnQueue>().unwrap();
        assert_eq!(queue.rx.try_recv().unwrap().lineage, lineage);
    }

    /// A task the owner queues while its action holds only the price runs
    /// with that price too, or the callback could hand its work to a task and
    /// get its other grants back. Another plugin's task never had them.
    #[test_case(SPAWN_PLUGIN, true ; "the_owner")]
    #[test_case(OTHER_PLUGIN, false ; "another_plugin")]
    fn a_queued_task_runs_under_the_ceiling_of_its_plugin(queued_by: &str, narrowed: bool) {
        const SAW: &str = "saw_ceiling";
        let lua = enqueue_test_lua();
        let work = lua
            .create_function(|lua, ()| {
                lua.globals()
                    .set(SAW, lua.app_data_ref::<Ceiling>().is_some())
            })
            .unwrap();
        let work = lua.create_registry_value(work).unwrap();
        let price = PluginPermissions::denied().owned_by(SPAWN_PLUGIN.into());
        block_on(Narrowed::to(&lua, &price, async {
            enqueue_async_task(&lua, queued_by, work).unwrap();
        }));
        assert!(
            lua.app_data_ref::<Ceiling>().is_none(),
            "the ceiling ends with the poll"
        );
        let task = lua
            .app_data_ref::<SpawnQueue>()
            .unwrap()
            .rx
            .try_recv()
            .unwrap();

        let ex = Rc::new(LocalSet::new());
        block_on_or_fail(ex.run_until(async {
            spawn_async_task(&lua, &ex, &Rc::new(gate()), task);
            while lua.globals().get::<Option<bool>>(SAW).unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        }));
        assert_eq!(lua.globals().get::<bool>(SAW).unwrap(), narrowed);
    }

    #[test]
    fn enqueue_async_task_uses_fresh_deadline_regardless_of_parent() {
        let lua = enqueue_test_lua();
        let parent_deadline = Instant::now() - Duration::from_secs(10);
        let scope = TaskScope::new(
            &lua,
            TaskCell::new(CancelToken::none(), Some(parent_deadline)),
        );
        let _active = scope.enter();

        let before = Instant::now();
        enqueue_async_task(&lua, SPAWN_PLUGIN, enqueue_dummy(&lua)).unwrap();

        let queue = lua.app_data_ref::<SpawnQueue>().unwrap();
        let task_deadline = queue.rx.try_recv().unwrap().deadline.unwrap();
        assert!(
            task_deadline > before,
            "async task should get a fresh deadline, not inherit expired parent"
        );
    }

    fn pending_task(lua: &Lua, cancel: CancelToken, deadline: Option<Instant>) -> PendingAsyncTask {
        PendingAsyncTask {
            work_fn: enqueue_dummy(lua),
            cancel,
            deadline,
            exts: TaskExts::default(),
            lineage: Lineage::default(),
            ceiling: None,
            lifetime: TaskLifetime::Held,
        }
    }

    #[test]
    fn spawn_async_task_skips_cancelled_tasks() {
        let ex = Rc::new(LocalSet::new());
        block_on(ex.run_until(async {
            let lua = enqueue_test_lua();
            let (trigger, token) = CancelToken::new();
            trigger.cancel();

            let g = Rc::new(gate());
            spawn_async_task(&lua, &ex, &g, pending_task(&lua, token, None));
            tokio::task::yield_now().await;
            assert_eq!(g.count.get(), 0);
        }));
    }

    const SPAWN_PLUGIN: &str = "spawner";
    const OTHER_PLUGIN: &str = "bystander";
    const SPAWN_ERROR: &str = "spawned boom";
    const PARK_AND_HOOK_SRC: &str =
        "on_cancel(function() hooked = true end) parked = true park() reached = true";
    const QUEUE_RUN_AND_PARK_SRC: &str = "queue_run() park()";
    const SELF_CANCEL_SRC: &str = "task:cancel() before = true yield_once() reached = true";
    const SPAWN_COMMAND_DEPTH: u8 = 2;

    fn spawn_test_lua() -> Lua {
        let lua = enqueue_test_lua();
        lua.set_app_data(SpawnedTasks::default());
        let park = lua
            .create_async_function(|_, ()| std::future::pending::<mlua::Result<()>>())
            .unwrap();
        let on_cancel = lua
            .create_function(|lua, hook: Function| register_cancel_hook(lua, hook))
            .unwrap();
        let globals = lua.globals();
        globals.set("park", park).unwrap();
        globals.set("on_cancel", on_cancel).unwrap();
        lua
    }

    fn spawn_src(lua: &Lua, plugin: &str, src: &str) -> (SpawnedTask, PendingAsyncTask) {
        let work = lua
            .create_registry_value(lua.load(src).into_function().unwrap())
            .unwrap();
        let handle = enqueue_spawned_task(lua, Arc::from(plugin), work).unwrap();
        let task = lua
            .app_data_ref::<SpawnQueue>()
            .unwrap()
            .rx
            .try_recv()
            .expect("a spawn goes to the global queue, never inline");
        (handle, task)
    }

    fn run_spawned_to_idle(lua: &Lua, task: PendingAsyncTask) {
        let ex = Rc::new(LocalSet::new());
        block_on(ex.run_until(async {
            spawn_async_task(lua, &ex, &Rc::new(gate()), task);
        }));
        block_on_or_fail(Rc::try_unwrap(ex).ok().unwrap());
    }

    /// Passed down to every task queued under it, so a task that takes
    /// nothing of its caller's shows it by an empty map.
    struct Inherited;

    impl TaskExt for Inherited {
        fn inherit(&self, _inline: bool) -> Option<Box<dyn TaskExt>> {
            Some(Box::new(Self))
        }
    }

    #[test]
    fn spawned_task_lives_with_its_plugin_not_its_caller() {
        let lua = spawn_test_lua();
        let (trigger, token) = CancelToken::new();
        let mut cell = TaskCell::new(token, Some(Instant::now()));
        cell.exts.insert(Inherited);
        cell.lineage = Lineage {
            command_depth: SPAWN_COMMAND_DEPTH,
            session: Some(MakiId::generate()),
            working_dir: Some(Arc::from(Path::new("/session"))),
        };
        cell.inline_spawn = Some(Vec::new());
        let scope = TaskScope::new(&lua, cell);
        let _active = scope.enter();

        let (_, task) = spawn_src(&lua, SPAWN_PLUGIN, "");
        trigger.cancel();

        assert!(
            !task.cancel.is_cancelled(),
            "the caller's cancel must not reach it"
        );
        assert_eq!(task.deadline, None);
        assert!(
            task.exts.is_empty(),
            "it must take no share of the caller's, so it pins none of its buffers"
        );
        assert_eq!(
            task.lineage,
            Lineage {
                command_depth: SPAWN_COMMAND_DEPTH,
                session: None,
                working_dir: Some(Arc::from(Path::new("/session"))),
            }
        );
        assert!(matches!(&task.lifetime, TaskLifetime::Spawned(s) if &*s.plugin == SPAWN_PLUGIN));
    }

    #[test]
    fn cancel_ends_a_spawned_task_parked_in_an_await() {
        let lua = spawn_test_lua();
        let (handle, task) = spawn_src(&lua, SPAWN_PLUGIN, PARK_AND_HOOK_SRC);
        let ex = Rc::new(LocalSet::new());
        block_on_or_fail(ex.run_until(async {
            spawn_async_task(&lua, &ex, &Rc::new(gate()), task);
            while !lua.globals().get::<bool>("parked").unwrap() {
                tokio::task::yield_now().await;
            }
            handle.cancel(&lua);
        }));
        block_on_or_fail(Rc::try_unwrap(ex).ok().unwrap());
        let globals = lua.globals();
        assert!(globals.get::<bool>("hooked").unwrap(), "on_cancel must run");
        assert!(!globals.get::<bool>("reached").unwrap());
    }

    #[test]
    fn a_spawned_task_that_cancels_itself_stops_at_its_next_yield() {
        let lua = spawn_test_lua();
        let yield_once = lua
            .create_async_function(|_, ()| async {
                tokio::task::yield_now().await;
                Ok(())
            })
            .unwrap();
        lua.globals().set("yield_once", yield_once).unwrap();
        let (handle, task) = spawn_src(&lua, SPAWN_PLUGIN, SELF_CANCEL_SRC);
        lua.globals().set("task", handle).unwrap();

        run_spawned_to_idle(&lua, task);

        let globals = lua.globals();
        assert!(globals.get::<bool>("before").unwrap());
        assert!(!globals.get::<bool>("reached").unwrap());
    }

    /// Without the retire, a plugin that spawns a short task per event would
    /// leak one cancel trigger per event for as long as it stays loaded.
    #[test_case(false, false ; "returning")]
    #[test_case(false, true ; "raising")]
    #[test_case(true, true ; "raising_after_cancelling_itself")]
    fn a_finished_spawned_task_releases_its_slot_and_reports_only_failures(
        cancels_itself: bool,
        raises: bool,
    ) {
        let lua = spawn_test_lua();
        let sink = RecordingSink::install(&lua);
        let cancel = if cancels_itself { "task:cancel()" } else { "" };
        let raise = if raises {
            format!("error('{SPAWN_ERROR}')")
        } else {
            String::new()
        };
        let (handle, task) = spawn_src(&lua, SPAWN_PLUGIN, &format!("{cancel} {raise}"));
        lua.globals().set("task", handle).unwrap();
        let token = task.cancel.clone();

        run_spawned_to_idle(&lua, task);

        assert!(token.is_cancelled(), "its trigger must leave SpawnedTasks");
        let flashes = sink.take_flashes();
        if raises && !cancels_itself {
            let [msg] = flashes.as_slice() else {
                panic!("the user must be told once, got: {flashes:?}");
            };
            assert!(
                msg.starts_with(&spawn_failed_message(SPAWN_PLUGIN, "")),
                "got: {msg}"
            );
            assert!(msg.contains(SPAWN_ERROR), "got: {msg}");
        } else {
            assert!(flashes.is_empty(), "got: {flashes:?}");
        }
    }

    /// The runs a spawned loop queues must die with it, or an unload would
    /// leave them behind.
    #[test]
    fn spawn_plugin_task_marks_its_runs_detached_and_cancellable_with_it() {
        let lua = spawn_test_lua();
        let queue_run = lua
            .create_function(|lua, ()| enqueue_async_task(lua, SPAWN_PLUGIN, enqueue_dummy(lua)))
            .unwrap();
        lua.globals().set("queue_run", queue_run).unwrap();
        let (_, task) = {
            let mut cell = TaskCell::new(CancelToken::none(), None);
            cell.lineage.command_depth = SPAWN_COMMAND_DEPTH;
            let scope = TaskScope::new(&lua, cell);
            let _active = scope.enter();
            spawn_src(&lua, SPAWN_PLUGIN, QUEUE_RUN_AND_PARK_SRC)
        };
        let spawn_rx = lua.app_data_ref::<SpawnQueue>().unwrap().rx.clone();

        let ex = Rc::new(LocalSet::new());
        let run = block_on_or_fail(ex.run_until(async {
            spawn_async_task(&lua, &ex, &Rc::new(gate()), task);
            spawn_rx.recv_async().await.unwrap()
        }));

        assert!(matches!(run.lifetime, TaskLifetime::Detached));
        assert_eq!(run.lineage.command_depth, SPAWN_COMMAND_DEPTH);
        assert!(!run.cancel.is_cancelled());
        lua.app_data_ref::<SpawnedTasks>()
            .unwrap()
            .0
            .cancel(Arc::from(SPAWN_PLUGIN));
        assert!(run.cancel.is_cancelled(), "the unload must reach the run");
    }

    fn watchdog_lua(shutdown: bool) -> (Lua, Watchdog) {
        let lua = Lua::new();
        let watchdog = Watchdog::spawn(&lua, Arc::new(AtomicBool::new(shutdown)));
        (lua, watchdog)
    }

    /// Generous vs the expected kill (one poll plus [`KILL_GRACE`]); only a
    /// broken watchdog gets here.
    const WATCHDOG_TEST_TIMEOUT: Duration = Duration::from_secs(10);

    const JIT_DEADLINE: Duration = Duration::from_millis(20);

    /// `while true do end` only stops if the watchdog kills it, so run it on
    /// a helper thread with a bounded wait: a broken watchdog then fails the
    /// test with a reason instead of hanging the harness. The clock starts
    /// before the loop can, so the reported time is a lower bound.
    fn hot_loop_expecting_kill(lua: &Lua) -> (mlua::Error, Duration) {
        let f = lua.load("while true do end").into_function().unwrap();
        let (tx, rx) = flume::bounded(1);
        let start = Instant::now();
        thread::spawn(move || drop(tx.send(f.call::<bool>(()))));
        let err = rx
            .recv_timeout(WATCHDOG_TEST_TIMEOUT)
            .expect("watchdog never killed the hot loop")
            .unwrap_err();
        (err, start.elapsed())
    }

    /// Runs long enough (50ms) to guarantee several watchdog pokes.
    fn timed_loop(lua: &Lua) -> Function {
        lua.load("local t = os.clock() while os.clock() - t < 0.05 do end return true")
            .into_function()
            .unwrap()
    }

    fn cancelled_token() -> CancelToken {
        let (trigger, token) = CancelToken::new();
        trigger.cancel();
        token
    }

    fn cancelled_handle() -> TaskHandle {
        Arc::new(Mutex::new(TaskCell::new(cancelled_token(), None)))
    }

    /// Killing at the first armed safepoint is what froze batch's children
    /// as in-progress after esc; never killing at all would leave a core
    /// spinning, which [`WATCHDOG_TEST_TIMEOUT`] catches.
    #[test]
    fn stale_cancelled_handle_aborts_callback_after_the_grace() {
        let (lua, _watchdog) = watchdog_lua(false);
        lua.set_app_data::<TaskHandle>(cancelled_handle());

        let (err, elapsed) = hot_loop_expecting_kill(&lua);

        assert!(err.to_string().contains(INTERRUPT_CANCELLED_MSG));
        assert!(elapsed >= KILL_GRACE, "kill skipped the cleanup grace");
    }

    /// A healthy poke must leave nothing armed, or the first poke after esc
    /// kills instantly instead of granting the grace.
    #[test]
    fn kill_grace_is_armed_once_then_renewed_per_execution_slice() {
        let (trigger, token) = CancelToken::new();
        let cell = TaskCell::new(token, None);
        let start = Instant::now();
        assert_eq!(cell.kill_due(start), None);
        assert!(cell.kill_at.get().is_none(), "healthy poke must not arm");

        trigger.cancel();
        assert_eq!(cell.kill_due(start), None, "first doomed poke only arms");
        assert_eq!(
            cell.kill_due(start + KILL_GRACE),
            None,
            "cleanup must get a full grace counted from the cancel"
        );
        assert_eq!(
            cell.kill_due(start + KILL_GRACE * 2),
            Some(Kill::Doomed(KillReason::Cancelled))
        );
        assert_eq!(
            cell.kill_due(start + KILL_GRACE * 2),
            None,
            "a raise must refill the grace for whoever catches it"
        );

        cell.renew_kill_grace();
        assert_eq!(
            cell.kill_due(start + KILL_GRACE * 2),
            None,
            "yielding must hand the task a fresh budget"
        );
    }

    /// A doom that lifts (a handler extending its own deadline) must take
    /// its stamp with it, or the next cancel kills with no grace at all.
    #[test]
    fn a_lifted_doom_disarms_the_grace() {
        let start = Instant::now();
        let cell = TaskCell::new(CancelToken::none(), Some(start));
        assert_eq!(
            cell.kill_due(start + KILL_GRACE),
            None,
            "first doomed poke only arms"
        );

        cell.deadline.set(Some(start + KILL_GRACE * 4));

        assert_eq!(cell.kill_due(start + KILL_GRACE * 2), None);
        assert!(cell.kill_at.get().is_none(), "healthy poke must disarm");
    }

    /// A stuck slice has had its seconds already, so the first raise lands at
    /// once. Only code that catches it gets a grace before the next one.
    #[test]
    fn stuck_slice_is_raised_at_once_then_again_a_grace_later() {
        let cell = TaskCell::new(CancelToken::none(), None);
        let start = Instant::now();
        cell.begin_slice(start);
        assert_eq!(cell.kill_due(start + SLICE_WARN), None);

        let stuck = start + SLICE_KILL + WATCHDOG_POLL_INTERVAL;
        assert_eq!(cell.kill_due(stuck), Some(Kill::Stuck));
        assert_eq!(cell.kill_due(stuck), None, "a caught raise gets a grace");
        assert_eq!(cell.kill_due(stuck + KILL_GRACE * 2), Some(Kill::Stuck));
    }

    #[test]
    fn yielding_task_outlives_the_slice_budget() {
        let cell = TaskCell::new(CancelToken::none(), None);
        let start = Instant::now();
        cell.end_slice(cell.begin_slice(start));
        let resumed = start + SLICE_KILL;
        cell.begin_slice(resumed);
        assert_eq!(cell.kill_due(resumed + SLICE_WARN), None);
    }

    #[test]
    fn nested_poll_of_the_same_cell_keeps_the_outer_slice() {
        let cell = TaskCell::new(CancelToken::none(), None);
        let start = Instant::now();
        let stuck = start + SLICE_KILL + WATCHDOG_POLL_INTERVAL;
        let outermost = cell.begin_slice(start);
        cell.end_slice(cell.begin_slice(start + SLICE_WARN));
        assert_eq!(cell.kill_due(stuck), Some(Kill::Stuck));
        cell.end_slice(outermost);
        assert_eq!(cell.kill_due(stuck), None);
    }

    #[test]
    fn slow_slice_warns_once_per_slice() {
        let cell = TaskCell::new(CancelToken::none(), None);
        let start = Instant::now();
        let late = start + SLICE_WARN * 2;
        let outer = cell.begin_slice(start);
        assert_eq!(cell.slice_warning(start + SLICE_WARN / 2), None);
        assert_eq!(cell.slice_warning(late), Some(SLICE_WARN * 2));
        assert_eq!(cell.slice_warning(late), None);
        cell.end_slice(outer);

        cell.begin_slice(late);
        assert_eq!(
            cell.slice_warning(late + SLICE_WARN * 2),
            Some(SLICE_WARN * 2)
        );
    }

    const STUCK_PLUGIN: &str = "spinner";

    /// An autocmd or keymap has no deadline, and a tool handler's is far off.
    /// Neither may save a slice that holds the thread.
    #[test_case(None ; "detached")]
    #[test_case(Some(Duration::from_secs(3600)) ; "before_deadline")]
    fn stuck_slice_is_killed_naming_the_plugin(deadline: Option<Duration>) {
        let (lua, _watchdog) = watchdog_lua(false);
        // The hot loop runs in globals, so globals stand in for a plugin env.
        let mut envs = PluginEnvs::default();
        envs.0
            .insert(Arc::from(STUCK_PLUGIN), lua.globals().to_pointer() as usize);
        lua.set_app_data(envs);
        let sink = RecordingSink::install(&lua);
        let cell = TaskCell::new(CancelToken::none(), deadline.map(|d| Instant::now() + d));
        cell.begin_slice(Instant::now() - SLICE_KILL);
        lua.set_app_data::<TaskHandle>(cell.into_handle());

        let (err, _) = hot_loop_expecting_kill(&lua);

        let expected = stuck_message(STUCK_PLUGIN);
        assert!(err.to_string().contains(&expected), "got: {err}");
        assert_eq!(sink.take_flashes(), [expected], "the user must be told");
    }

    fn task_handle(cancel: CancelToken, deadline: Option<Instant>) -> TaskHandle {
        TaskCell::new(cancel, deadline).into_handle()
    }

    /// The watchdog never reaches a handler parked in an await, so this
    /// race is what ends it - but not before its cleanup window, and never
    /// ahead of a result the handler already produced.
    #[test]
    fn until_abandoned_ends_a_parked_handler_only_after_its_window() {
        let parked = std::future::pending::<()>;
        block_on(async {
            let early = futures_lite::future::poll_once(until_abandoned(
                parked(),
                &task_handle(cancelled_token(), None),
            ))
            .await;
            assert!(
                early.is_none(),
                "a cancel must not abandon the handler before its window"
            );

            assert_eq!(
                until_abandoned(
                    parked(),
                    &task_handle(CancelToken::none(), Some(Instant::now())),
                )
                .await
                .expect_err("a lapsed deadline must end a parked handler"),
                HANDLER_TIMEOUT_MSG
            );

            until_abandoned(
                std::future::ready(()),
                &task_handle(cancelled_token(), Some(Instant::now())),
            )
            .await
            .expect("a finished handler outranks a doom in the same slice");
        });
    }

    /// `ctx:set_deadline` lands after the race is already armed, so the
    /// arming must be revisited or a parked handler outlives its deadline.
    #[test]
    fn until_abandoned_re_arms_on_a_deadline_set_after_it_started() {
        let handle = task_handle(CancelToken::none(), None);
        let cell = Arc::clone(&handle);
        block_on(async {
            let set = async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                let cell = lock_cell(&cell);
                cell.deadline.set(Some(Instant::now()));
                cell.deadline_changed.notify(usize::MAX);
            };
            let wait = until_abandoned(std::future::pending::<()>(), &handle);
            let (_, err) = futures_lite::future::zip(set, wait).await;
            assert_eq!(
                err.expect_err("the new deadline must end the handler"),
                HANDLER_TIMEOUT_MSG
            );
        });
    }

    /// [`ScopedFuture`] is the one place that observes a task yielding, so
    /// it is what renews the grace.
    #[test]
    fn scoped_future_poll_renews_kill_grace() {
        let lua = Lua::new();
        let scope = TaskScope::new(&lua, TaskCell::new(cancelled_token(), None));
        let handle = Arc::clone(scope.handle());
        lock_cell(&handle).kill_due(Instant::now());
        assert!(lock_cell(&handle).kill_at.get().is_some());

        block_on(scope.scope_future(std::future::ready(())));

        assert!(lock_cell(&handle).kill_at.get().is_none());
    }

    /// What the executor runs between two polls must not find the parked task
    /// still published. A plugin loaded beside a spawned loop used to run its
    /// top level inside that loop, and its runs died with the other plugin.
    #[test]
    fn a_parked_task_leaves_no_task_published() {
        let lua = Lua::new();
        let scope = TaskScope::detached(&lua);
        let mut parked = scope.scope_future(std::future::pending::<()>());
        assert!(block_on(poll_once(&mut parked)).is_none());
        assert!(lua.app_data_ref::<TaskHandle>().is_none());
    }

    const HOOK_MARKS: [&str; 3] = ["first", "second", "third"];
    const HOOK_GOOD_MARK: &str = "good";
    const HOOK_NESTED_MARK: &str = "nested";
    const HOOK_RAISES: &str = "error('hook blew up')";
    const HOOK_YIELDS: &str = "coroutine.yield()";
    const HOOK_POLL_ROUNDS: usize = 3;
    const HOOK_INNER_OUTPUT: u8 = 7;
    const HOOK_NEVER_FIRED: &str = "cancel hook never fired";
    const HOOK_SKIPPED_MSG: &str = "a hook registered after the bad one never fired";

    fn recording_hook(lua: &Lua, tx: &flume::Sender<&'static str>, mark: &'static str) {
        let tx = tx.clone();
        let hook = lua
            .create_function(move |_, ()| {
                tx.send(mark).ok();
                Ok(())
            })
            .unwrap();
        register_cancel_hook(lua, hook).unwrap();
    }

    fn live_scope(lua: &Lua) -> (CancelTrigger, TaskScope) {
        let (trigger, token) = CancelToken::new();
        (trigger, TaskScope::new(lua, TaskCell::new(token, None)))
    }

    /// The token is already tripped, so the cancel branch is ready on the
    /// first poll and no waker round trip is needed.
    fn poll_cancelled_scope_once(scope: &TaskScope) {
        let mut fut = scope.scope_future(std::future::pending::<()>());
        assert!(block_on(poll_once(&mut fut)).is_none());
    }

    /// A hook armed after the token tripped has no transition left to ride, so
    /// it fires inline, whether a plugin or another hook armed it. The nested
    /// case also pins the lock discipline: `fire_cancel_hooks` must not hold
    /// the cell across a hook.
    #[test]
    fn cancel_hook_registered_after_the_cancel_fires_inline() {
        let lua = Lua::new();
        let scope = TaskScope::new(&lua, TaskCell::new(cancelled_token(), None));
        let _active = scope.enter();
        let (tx, fired_rx) = flume::unbounded();
        let outer = lua
            .create_function(move |lua, ()| {
                tx.send(HOOK_GOOD_MARK).ok();
                let tx = tx.clone();
                let nested = lua.create_function(move |_, ()| {
                    tx.send(HOOK_NESTED_MARK).ok();
                    Ok(())
                })?;
                register_cancel_hook(lua, nested)
            })
            .unwrap();

        register_cancel_hook(&lua, outer).unwrap();

        assert_eq!(
            fired_rx.try_iter().collect::<Vec<_>>(),
            [HOOK_GOOD_MARK, HOOK_NESTED_MARK],
            "{HOOK_NEVER_FIRED}"
        );
        assert!(
            lock_cell(scope.handle()).cancel_hooks.is_empty(),
            "a hook that already fired must not be kept for the next poll"
        );
    }

    /// Hooks are cleanup steps stacked as the handler nests deeper, so every
    /// one runs, in registration order. A hook that raises, or yields from
    /// outside its coroutine, is one plugin's bug and must not cost the later
    /// plugins their cleanup.
    #[test_case(HOOK_RAISES ; "raising")]
    #[test_case(HOOK_YIELDS ; "yielding")]
    fn cancel_hooks_all_fire_in_order_despite_a_bad_one(bad_body: &str) {
        let lua = Lua::new();
        let (trigger, scope) = live_scope(&lua);
        let _active = scope.enter();
        let bad = lua.load(bad_body).into_function().unwrap();
        register_cancel_hook(&lua, bad).unwrap();
        let (fired_tx, fired_rx) = flume::unbounded();
        for mark in HOOK_MARKS {
            recording_hook(&lua, &fired_tx, mark);
        }
        trigger.cancel();

        poll_cancelled_scope_once(&scope);

        assert_eq!(
            fired_rx.try_iter().collect::<Vec<_>>(),
            HOOK_MARKS,
            "{HOOK_SKIPPED_MSG}"
        );
    }

    /// `scope_future` nests, a handler's scope wrapping the `gather` that
    /// parks it, so several levels see the token go ready in one poll and
    /// every later poll sees it ready again. Cleanup that runs twice repaints
    /// over what the abandon path already put on screen.
    #[test]
    fn cancel_hook_fires_once_across_nested_scopes_and_repeated_polls() {
        let lua = Lua::new();
        let (trigger, scope) = live_scope(&lua);
        let _active = scope.enter();
        let (fired_tx, fired_rx) = flume::unbounded();
        recording_hook(&lua, &fired_tx, HOOK_GOOD_MARK);
        trigger.cancel();

        let mut nested = scope.scope_future(scope.scope_future(std::future::pending::<()>()));
        for _ in 0..HOOK_POLL_ROUNDS {
            assert!(block_on(poll_once(&mut nested)).is_none());
        }

        assert_eq!(fired_rx.try_iter().count(), 1);
    }

    /// The `RegistryKey` is the last reference to a hook, so holding it pins
    /// the closure and its captures for the VM's whole life. Both endings must
    /// let go: fired, or dropped with a task that was never cancelled, which
    /// mlua only reclaims on some later registry op.
    #[test_case(true ; "after firing")]
    #[test_case(false ; "when an uncancelled scope ends")]
    fn cancel_hook_registry_value_is_released(cancel: bool) {
        let lua = Lua::new();
        let captured = Arc::new(());
        let held = Arc::clone(&captured);
        let (trigger, scope) = live_scope(&lua);
        let hook = lua
            .create_function(move |_, ()| {
                let _ = &held;
                Ok(())
            })
            .unwrap();
        {
            let _active = scope.enter();
            register_cancel_hook(&lua, hook).unwrap();
        }

        if cancel {
            trigger.cancel();
            poll_cancelled_scope_once(&scope);
        } else {
            drop(scope);
        }
        lua.gc_collect().unwrap();
        lua.gc_collect().unwrap();

        assert_eq!(Arc::strong_count(&captured), 1);
    }

    /// With a sibling's handle in app_data the hook would repaint the
    /// sibling's buffers. The sibling has to get app_data back once the poll is
    /// over, and its own hooks must not ride someone else's cancel.
    #[test]
    fn cancel_hook_runs_under_its_own_task_handle() {
        let lua = Lua::new();
        let (trigger, scope) = live_scope(&lua);
        let (seen_tx, seen_rx) = flume::bounded(1);
        let hook = lua
            .create_function(move |lua, ()| {
                seen_tx.send(active_task(lua)).ok();
                Ok(())
            })
            .unwrap();
        {
            let _active = scope.enter();
            register_cancel_hook(&lua, hook).unwrap();
        }
        let (_sibling_trigger, sibling) = live_scope(&lua);
        let _sibling_active = sibling.enter();
        let (sibling_tx, sibling_rx) = flume::bounded(1);
        recording_hook(&lua, &sibling_tx, HOOK_GOOD_MARK);
        trigger.cancel();

        poll_cancelled_scope_once(&scope);

        let seen = seen_rx.try_recv().expect(HOOK_NEVER_FIRED);
        assert!(Arc::ptr_eq(&seen, scope.handle()));
        assert!(Arc::ptr_eq(&active_task(&lua), sibling.handle()));
        assert!(
            sibling_rx.try_recv().is_err(),
            "an uncancelled task's hook must not fire with a sibling's cancel"
        );
    }

    /// The whole point of the hook: a handler parked in an await runs no Lua
    /// of its own, so only the token's waker can get its cleanup on screen
    /// before the abandon window. The cancel branch and the inner future share
    /// that one waker, so if firing swallowed the poll, a handler finishing
    /// right after the cancel would hang instead of returning its output.
    #[test]
    fn cancel_hook_fires_on_a_parked_task_and_leaves_it_wakeable() {
        let lua = Lua::new();
        let (trigger, scope) = live_scope(&lua);
        let _active = scope.enter();
        let (fired_tx, fired_rx) = flume::bounded(1);
        recording_hook(&lua, &fired_tx, HOOK_GOOD_MARK);
        let (item_tx, item_rx) = flume::bounded(1);

        let out = block_on_or_fail(futures_lite::future::or(
            scope.scope_future(async move { item_rx.recv_async().await.unwrap() }),
            async move {
                trigger.cancel();
                assert_eq!(fired_rx.recv_async().await.ok(), Some(HOOK_GOOD_MARK));
                item_tx.send(HOOK_INNER_OUTPUT).unwrap();
                std::future::pending().await
            },
        ));

        assert_eq!(out, HOOK_INNER_OUTPUT);
        assert!(
            lock_cell(scope.handle()).cancel_hooks.is_empty(),
            "a fired hook must not be kept for a second cancel"
        );
    }

    /// Order is the whole guarantee: a handler whose last await resolves on
    /// the very poll the cancel lands returns straight away, and the scope
    /// dropping behind it clears its hooks unfired. Firing after the inner
    /// poll would skip the cleanup silently, with nothing on screen.
    #[test]
    fn cancel_hooks_fire_before_an_inner_future_that_is_ready_at_once() {
        let lua = Lua::new();
        let (trigger, scope) = live_scope(&lua);
        let _active = scope.enter();
        let inner_ran = Arc::new(AtomicBool::new(false));
        let seen_by_hook = Arc::clone(&inner_ran);
        let (fired_tx, fired_rx) = flume::bounded(1);
        let hook = lua
            .create_function(move |_, ()| {
                fired_tx.send(seen_by_hook.load(Ordering::SeqCst)).ok();
                Ok(())
            })
            .unwrap();
        register_cancel_hook(&lua, hook).unwrap();
        trigger.cancel();

        let out = block_on(scope.scope_future(poll_fn(move |_| {
            inner_ran.store(true, Ordering::SeqCst);
            Poll::Ready(HOOK_INNER_OUTPUT)
        })));

        assert_eq!(
            out, HOOK_INNER_OUTPUT,
            "the scope must still yield the handler's result"
        );
        assert!(
            !fired_rx.try_recv().expect(HOOK_NEVER_FIRED),
            "the hook must run before the inner future gets its poll"
        );
    }

    /// The fire takes the whole list first, so a hook that arms another one
    /// re-enters `fire_cancel_hooks` from inside itself. That must terminate
    /// and leave nothing unfired; where the new hook lands among the queued
    /// ones is not a promise anyone can lean on.
    #[test]
    fn cancel_hook_registered_mid_fire_still_fires_every_hook_once() {
        let lua = Lua::new();
        let (trigger, scope) = live_scope(&lua);
        let _active = scope.enter();
        let (fired_tx, fired_rx) = flume::unbounded();
        let tx = fired_tx.clone();
        let arming = lua
            .create_function(move |lua, ()| {
                tx.send(HOOK_MARKS[0]).ok();
                recording_hook(lua, &tx, HOOK_NESTED_MARK);
                Ok(())
            })
            .unwrap();
        register_cancel_hook(&lua, arming).unwrap();
        for mark in HOOK_MARKS.into_iter().skip(1) {
            recording_hook(&lua, &fired_tx, mark);
        }
        trigger.cancel();

        poll_cancelled_scope_once(&scope);

        let mut fired = fired_rx.try_iter().collect::<Vec<_>>();
        fired.sort_unstable();
        let mut expected = HOOK_MARKS.to_vec();
        expected.push(HOOK_NESTED_MARK);
        expected.sort_unstable();
        assert_eq!(fired, expected, "{HOOK_SKIPPED_MSG}");
        assert!(
            lock_cell(scope.handle()).cancel_hooks.is_empty(),
            "a re-entrant fire must not leave a hook behind"
        );
    }

    const HOOK_PARENT_MARK: &str = "parent";
    const HOOK_CHILD_MARK: &str = "child";
    const HOOK_CHILD_WORK: &str = "arm(); park()";
    const HOOK_ARM_FN: &str = "arm";
    const HOOK_PARK_FN: &str = "park";

    /// A `maki.async.run` task gets its own cell but inherits the parent's
    /// token, so a hook armed inside it fires on the parent's cancel even
    /// though nothing else polls that task. Only its own hooks: parent and
    /// siblings share that token, and reading the wrong cell would clean up
    /// buffers it never owned.
    #[test]
    fn cancel_hook_in_a_spawned_task_fires_on_the_inherited_cancel() {
        let lua = Lua::new();
        let (trigger, parent) = live_scope(&lua);
        let (fired_tx, fired_rx) = flume::unbounded();
        {
            let _active = parent.enter();
            recording_hook(&lua, &fired_tx, HOOK_PARENT_MARK);
        }

        let (armed_tx, armed_rx) = flume::bounded(1);
        let tx = fired_tx.clone();
        let arm = lua
            .create_function(move |lua, ()| {
                recording_hook(lua, &tx, HOOK_CHILD_MARK);
                armed_tx.send(()).ok();
                Ok(())
            })
            .unwrap();
        let park = lua
            .create_async_function(|_, ()| std::future::pending::<Result<(), mlua::Error>>())
            .unwrap();
        lua.globals().set(HOOK_ARM_FN, arm).unwrap();
        lua.globals().set(HOOK_PARK_FN, park).unwrap();
        let work_fn = lua
            .create_registry_value(lua.load(HOOK_CHILD_WORK).into_function().unwrap())
            .unwrap();
        let task = PendingAsyncTask {
            work_fn,
            cancel: lock_cell(parent.handle()).cancel.clone(),
            deadline: None,
            exts: TaskExts::default(),
            lineage: Lineage::default(),
            ceiling: None,
            lifetime: TaskLifetime::Held,
        };

        let ex = Rc::new(LocalSet::new());
        block_on_or_fail(ex.run_until(async {
            spawn_async_task(&lua, &ex, &Rc::new(gate()), task);
            armed_rx.recv_async().await.unwrap();
            trigger.cancel();
            assert_eq!(fired_rx.recv_async().await.ok(), Some(HOOK_CHILD_MARK));
        }));

        assert!(
            fired_rx.is_empty(),
            "the parent's hook must not ride the child's fire"
        );

        poll_cancelled_scope_once(&parent);

        assert_eq!(
            fired_rx.try_iter().collect::<Vec<_>>(),
            [HOOK_PARENT_MARK],
            "the parent must fire its own hook, once, and not the child's"
        );
    }

    #[test]
    fn delivery_scope_refuses_job_ownership() {
        let lua = Lua::new();

        let delivery = TaskScope::delivery(&lua);
        {
            let _active = delivery.enter();
            assert!(active_task_id(&lua).is_some());
            assert!(job_task_id(&lua).is_none());
        }

        let detached = TaskScope::detached(&lua);
        let _active = detached.enter();
        assert_eq!(job_task_id(&lua), active_task_id(&lua));
    }

    #[test]
    fn fresh_task_scope_shields_callback_from_stale_cancelled_handle() {
        let (lua, _watchdog) = watchdog_lua(false);
        lua.set_app_data::<TaskHandle>(cancelled_handle());

        let scope = TaskScope::detached(&lua);
        let _active = scope.enter();
        let result = timed_loop(&lua).call::<bool>(());

        assert!(result.unwrap());
    }

    #[test]
    fn shutdown_flag_aborts_callback_even_with_fresh_scope() {
        let (lua, _watchdog) = watchdog_lua(true);

        let scope = TaskScope::detached(&lua);
        let _active = scope.enter();
        let (err, _) = hot_loop_expecting_kill(&lua);

        assert!(err.to_string().contains(INTERRUPT_SHUTDOWN_MSG));
    }

    /// Shutdown is checked before the task cell, so a reload or a quit
    /// never queues behind the cleanup grace of every cancelled runaway.
    #[test]
    fn shutdown_outranks_the_cleanup_grace() {
        let (lua, _watchdog) = watchdog_lua(true);
        lua.set_app_data::<TaskHandle>(cancelled_handle());

        let (err, elapsed) = hot_loop_expecting_kill(&lua);

        assert!(err.to_string().contains(INTERRUPT_SHUTDOWN_MSG));
        assert!(
            elapsed < KILL_GRACE,
            "shutdown waited out the cleanup grace"
        );
    }

    /// JIT-compiled code must still hit the interrupt, and the timeout
    /// path gets the same cleanup grace as the cancel path: a timed-out
    /// handler also has children to settle and a buf to rerender.
    #[test]
    fn jit_busy_loop_killed_a_grace_after_the_deadline() {
        let (lua, _watchdog) = watchdog_lua(false);
        install_compiler(&lua, true);

        let deadline = Instant::now() + JIT_DEADLINE;
        let cell = TaskCell::new(CancelToken::none(), Some(deadline));
        lua.set_app_data::<TaskHandle>(Arc::new(Mutex::new(cell)));

        let (err, elapsed) = hot_loop_expecting_kill(&lua);

        assert!(err.to_string().contains(INTERRUPT_DEADLINE_MSG));
        assert!(
            elapsed >= JIT_DEADLINE + KILL_GRACE,
            "kill skipped the cleanup grace"
        );
    }

    #[test]
    fn spawn_async_task_runs_and_decrements_gate() {
        let ex = Rc::new(LocalSet::new());
        block_on(ex.run_until(async {
            let lua = enqueue_test_lua();
            let task = pending_task(
                &lua,
                CancelToken::none(),
                Some(Instant::now() + Duration::from_secs(5)),
            );

            let g = Rc::new(gate());
            spawn_async_task(&lua, &ex, &g, task);

            for _ in 0..10 {
                tokio::task::yield_now().await;
                if g.count.get() == 0 {
                    return;
                }
            }
            panic!("gate count never reached 0 after draining");
        }));
    }

    const WARNED_PLUGIN: &str = "gapped";
    const SECOND_WARNED_PLUGIN: &str = "other";
    const WARNED_SUBJECT: &str = "TurnStart";
    const OTHER_SUBJECT: &str = "tool.*.input";

    /// Re-decided on every event and call, so without the dedup the user would
    /// see the same gap for as long as it lasts.
    #[test]
    fn a_permission_gap_is_shown_once_per_plugin_per_subject() {
        let lua = Lua::new();
        let sink = RecordingSink::install(&lua);
        let warn = |plugin: &str, subject: &str| {
            warn_plugin_once(&lua, plugin, subject, || format!("{plugin}/{subject}"));
        };

        warn(WARNED_PLUGIN, WARNED_SUBJECT);
        warn(WARNED_PLUGIN, WARNED_SUBJECT);
        warn(WARNED_PLUGIN, OTHER_SUBJECT);
        warn(SECOND_WARNED_PLUGIN, WARNED_SUBJECT);

        assert_eq!(
            sink.take_flashes(),
            [
                format!("{WARNED_PLUGIN}/{WARNED_SUBJECT}"),
                format!("{WARNED_PLUGIN}/{OTHER_SUBJECT}"),
                format!("{SECOND_WARNED_PLUGIN}/{WARNED_SUBJECT}"),
            ]
        );
    }

    const LOADING_PLUGIN: &str = "loading";
    const LAYERED_SLOT: &str = "loading.complete";

    /// A plugin is not in the map until its load finishes, so only a layer
    /// that costs nothing may steer a chain before then.
    #[test_case(LayerCost::Named(&[]), true ; "a_free_layer_steers")]
    #[test_case(LayerCost::Named(&[Permission::Net]), false ; "a_priced_layer_waits")]
    #[test_case(LayerCost::TOTAL, false ; "a_total_price_waits")]
    fn a_layer_from_a_plugin_still_loading(cost: LayerCost<'static>, allowed: bool) {
        let lua = Lua::new();
        let plugins = PluginMap::default();
        let layer = SlotLayer {
            plugin: LOADING_PLUGIN.into(),
            func: lua.create_function(|_, ()| Ok(())).unwrap(),
            ceiling: None,
        };
        assert_eq!(
            layer_delegation(&lua, &plugins, cost)(&layer, LAYERED_SLOT),
            allowed
        );
    }
}
