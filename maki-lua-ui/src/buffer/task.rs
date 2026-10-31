//! What the render domain keeps on each task: the buffers its Lua made and
//! the shares that keep the buffers' handlers alive.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use mlua::Lua;

use crate::buffer::buf::BufferStore;
use crate::buffer::runtime::{TaskExt, TaskHandle, lock_cell, try_active_task};

pub(crate) struct TaskBufs {
    pub store: BufferStore,
    /// Set when the task's scope ends. A task with no scope never clears
    /// its store; the store goes with the task.
    ended: bool,
    claim: Weak<BufsClaim>,
}

impl TaskBufs {}

/// A share of {bufs}: the live one, or a new one while the scope runs.
fn claim(bufs: &Arc<Mutex<TaskBufs>>) -> Option<Arc<BufsClaim>> {
    let mut locked = lock(bufs);
    if let Some(claim) = locked.claim.upgrade() {
        return Some(claim);
    }
    if locked.ended {
        return None;
    }
    let claim = Arc::new(BufsClaim(Arc::clone(bufs)));
    locked.claim = Arc::downgrade(&claim);
    Some(claim)
}

fn lock(bufs: &Mutex<TaskBufs>) -> MutexGuard<'_, TaskBufs> {
    bufs.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Shared ownership of a task's buffers past its scope: each queued
/// `maki.async.run` task holds one clone until it ends, and a warm tool one
/// until it is evicted. The store clears once the scope has ended and no
/// clone is left, breaking Lua GC watcher/click cycles. The root buf is
/// resolved lazily because it may not exist at enqueue time.
pub struct BufsClaim(Arc<Mutex<TaskBufs>>);

impl BufsClaim {}

impl Drop for BufsClaim {
    fn drop(&mut self) {
        let mut bufs = lock(&self.0);
        if bufs.ended {
            bufs.store.clear();
        }
    }
}

/// The render domain's [`TaskExt`]. Every scope gets one at its start; a
/// cell with no scope gets one on its first buf.
pub(crate) struct BufsView {
    bufs: Arc<Mutex<TaskBufs>>,
    /// The share of the queuing task's buffers, held until this task ends.
    owner: Option<Arc<BufsClaim>>,
}

impl Default for BufsView {
    fn default() -> Self {
        Self {
            bufs: Arc::new(Mutex::new(TaskBufs {
                store: BufferStore::new(),
                ended: false,
                claim: Weak::new(),
            })),
            owner: None,
        }
    }
}

impl TaskExt for BufsView {
    /// An inline task takes no claim: its queue lives in the parent's cell,
    /// so a claim would make a cycle that keeps the buffers and their Lua
    /// callbacks.
    fn inherit(&self, inline: bool) -> Option<Box<dyn TaskExt>> {
        if inline {
            return None;
        }
        Some(Box::new(Self {
            owner: Some(claim(&self.bufs)?),
            ..Self::default()
        }))
    }

    /// This task's buffers clear before the queuing task's share goes.
    fn end(&mut self) {
        {
            let mut bufs = lock(&self.bufs);
            bufs.ended = true;
            if bufs.claim.strong_count() == 0 {
                bufs.store.clear();
            }
        }
        self.owner = None;
    }
}

/// Runs {f} on {task}'s buffers, made on first use.
pub(crate) fn with_bufs<R>(task: &TaskHandle, f: impl FnOnce(&mut TaskBufs) -> R) -> R {
    let bufs = Arc::clone(
        &lock_cell(task)
            .exts
            .get_or_insert_with(BufsView::default)
            .bufs,
    );
    f(&mut lock(&bufs))
}

pub(crate) fn with_task_bufs<R>(
    lua: &Lua,
    f: impl FnOnce(&mut BufferStore) -> R,
) -> mlua::Result<R> {
    Ok(with_bufs(&try_active_task(lua)?, |bufs| f(&mut bufs.store)))
}

#[cfg(test)]
mod tests {
    use maki_types::snapshot::SharedBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    use maki_lua_core::domain::Domain;
    use maki_types::cancel::CancelToken;
    use maki_types::snapshot::SnapshotLine;
    use mlua::RegistryKey;

    use super::*;
    use crate::buffer::buf::HandlerSlot;
    use crate::buffer::runtime::{
        SpawnQueue, TaskCell, TaskScope, enqueue_async_task, install_task_domain,
    };
    use crate::domains::ui::UiDomain;
    use crate::pack::Interaction;

    const PLUGIN: &str = "test-plugin";

    /// The view is made on first use, as a detached scope's.
    fn task_cell() -> TaskCell {
        TaskCell::new(CancelToken::none(), None)
    }

    fn dummy_work(lua: &Lua) -> RegistryKey {
        let func = lua.create_function(|_, _: ()| Ok(())).unwrap();
        lua.create_registry_value(func).unwrap()
    }

    #[test]
    fn task_scope_drop_clears_buf_handler_slots() {
        let lua = Lua::new();
        let scope = TaskScope::new(&lua, task_cell());
        let handle = {
            let _active = scope.enter();
            let handle = with_task_bufs(&lua, |store| store.create()).unwrap();
            lua.globals()
                .set("buf", lua.create_userdata(handle.clone()).unwrap())
                .unwrap();
            lua.load(
                r#"buf:on("click", function() end); buf:on("change", function() hit = true end)"#,
            )
            .exec()
            .unwrap();
            handle
        };
        let shared = Arc::clone(&handle.buf);
        shared.append(SnapshotLine { spans: vec![] });
        assert!(lua.globals().get::<bool>("hit").unwrap());
        assert!(handle.click_fn().is_some());
        drop(scope);
        lua.globals().set("hit", false).unwrap();
        shared.append(SnapshotLine { spans: vec![] });
        assert!(!lua.globals().get::<bool>("hit").unwrap());
        assert!(handle.click_fn().is_none());
    }

    #[test]
    fn scope_drop_defers_watcher_clear_until_owned_tasks_release() {
        let lua = Lua::new();
        SpawnQueue::install(&lua);
        let scope = TaskScope::new(&lua, task_cell());
        let handle = Arc::clone(scope.handle());

        let buf = Arc::new(SharedBuf::new());
        let fired = Arc::new(AtomicBool::new(false));
        let f = Arc::clone(&fired);
        buf.set_on_change(move || f.store(true, Ordering::Release));
        with_bufs(&handle, |bufs| {
            bufs.store.track(HandlerSlot::Change(Arc::clone(&buf)))
        });

        {
            let _active = scope.enter();
            enqueue_async_task(&lua, PLUGIN, dummy_work(&lua)).unwrap();
        }
        drop(scope);

        buf.set_lines(Vec::new());
        assert!(
            fired.load(Ordering::Acquire),
            "watcher must survive scope drop while an owned async task is pending"
        );

        let task = SpawnQueue::take(&lua).unwrap();
        drop(task);
        fired.store(false, Ordering::Release);
        buf.set_lines(Vec::new());
        assert!(
            !fired.load(Ordering::Acquire),
            "dropping the last owned task must clear the deferred watcher"
        );
    }

    #[test]
    fn a_task_queued_before_the_first_buf_keeps_the_later_bufs_alive() {
        let lua = Lua::new();
        SpawnQueue::install(&lua);
        let domains: Arc<dyn Domain> = Arc::new(UiDomain::new(Interaction::Tty).0);
        install_task_domain(&lua, &domains);
        let scope = TaskScope::detached(&lua);
        {
            let _active = scope.enter();
            enqueue_async_task(&lua, PLUGIN, dummy_work(&lua)).unwrap();
        }

        let buf = Arc::new(SharedBuf::new());
        let fired = Arc::new(AtomicBool::new(false));
        let f = Arc::clone(&fired);
        buf.set_on_change(move || f.store(true, Ordering::Release));
        with_bufs(scope.handle(), |bufs| {
            bufs.store.track(HandlerSlot::Change(Arc::clone(&buf)))
        });
        drop(scope);

        buf.set_lines(Vec::new());
        assert!(
            fired.load(Ordering::Acquire),
            "a buf made after the enqueue must survive while the task is pending"
        );

        drop(SpawnQueue::take(&lua).unwrap());
        fired.store(false, Ordering::Release);
        buf.set_lines(Vec::new());
        assert!(
            !fired.load(Ordering::Acquire),
            "the finished task must clear the buf it kept"
        );
    }
}
