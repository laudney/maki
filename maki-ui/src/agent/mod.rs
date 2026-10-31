pub(crate) mod capabilities;
pub(crate) mod remote_facade;
pub(crate) mod remote_models;
pub(crate) mod remote_plan;
pub(crate) mod remote_settings;
pub use self::remote_settings::RemoteSettings;
mod remote_loop;
pub(crate) mod remote_replay;
pub(crate) mod shared_queue;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::EventHandle;
use crate::components::permission_prompt::AnsweredRequest;
use agent_client_protocol_schema::v1::{SessionId, SessionUpdate};
use arc_swap::ArcSwapOption;
use maki_storage::remote_drafts::RemoteStore;
use maki_types::event::Envelope;
use maki_types::session::StoredSession;
use maki_wire::client::{AcpClient, ClientEvent};
use maki_wire::extensions::Agreed;
use maki_wire::session::SessionOperation;
use maki_wire::usage::SessionUsage;
use serde::de::DeserializeOwned;

use self::remote_plan::{PlanWrites, RemotePlan};

use maki_types::cancel::RunCancels;
use tokio::runtime::Handle;
use tokio_util::task::AbortOnDropHandle;
use tracing::{info, warn};

use crate::app::App;

pub(crate) use self::remote_loop::MALFORMED_UPDATE;
/// A fake agent on the other end of a remote tab's link.
#[cfg(test)]
pub(crate) use self::remote_loop::tests as fake_agent;
pub use self::remote_loop::{BUILD_MODE, PLAN_MODE};
use self::remote_loop::{Rebind, RemoteAgentLoop, RewindRequest};
pub(crate) use self::shared_queue::{QueueSender, QueuedMessage};

/// A remote session's spending, published by its loop and drawn by the App.
pub(crate) type UsageSlot = Arc<ArcSwapOption<SessionUsage>>;

/// Which `/new` the client is bound to. The App asks for a replacement at
/// once, the loop installs it later, and until then the client still names
/// the old session, which a side question must not ask.
#[derive(Default)]
pub(crate) struct SessionBinding {
    requested: AtomicU64,
    bound: AtomicU64,
}

impl SessionBinding {
    pub(crate) fn request(&self) -> u64 {
        self.requested.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn bind(&self, generation: u64) {
        self.bound.store(generation, Ordering::SeqCst);
    }

    pub(crate) fn is_current(&self) -> bool {
        self.bound.load(Ordering::SeqCst) == self.requested.load(Ordering::SeqCst)
    }
}

/// A spawned agent's own invocation with its terminal login's arguments
/// and environment, run in this terminal for `/login`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginInvocation {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// A remote tab's `/login`: what to run, and whom to tell it ran.
#[derive(Clone)]
pub(crate) struct RemoteLogin {
    pub(crate) invocation: LoginInvocation,
    pub(crate) client: Arc<AcpClient>,
}

/// What a remote tab's side requests (`/btw`, the `/usage` quota) ask
/// through, when the agent agreed on them.
#[derive(Clone)]
pub(crate) struct RemoteSession {
    pub(crate) client: Arc<AcpClient>,
    pub(crate) binding: Arc<SessionBinding>,
    pub(crate) rewind_tx: Option<flume::Sender<RewindRequest>>,
}

impl RemoteSession {
    /// The binding first: only the UI thread asks for a `/new`, so once it
    /// is current the session read next is the installed one.
    pub(crate) fn session(&self) -> Option<SessionId> {
        match self.binding.is_current() {
            true => self.client.bound_session().ok(),
            false => None,
        }
    }

    pub(crate) fn request<T: DeserializeOwned>(
        &self,
        operation: impl FnOnce(SessionId) -> SessionOperation,
    ) -> Result<T, String> {
        if self.rewind_tx.is_none() {
            return Err(capabilities::UNAVAILABLE_REMOTE.into());
        }
        let id = self.session().ok_or(capabilities::SESSION_OPENING)?;
        let result = Handle::current()
            .block_on(self.client.session_operation(operation(id.clone())))
            .map_err(|error| error.to_string())?;
        if self.session().as_ref() != Some(&id) {
            return Err(remote_settings::SESSION_REPLACED.into());
        }
        serde_json::from_value(result).map_err(|error| error.to_string())
    }

    pub(crate) fn rewind(&self, index: usize, prompt: String) -> Result<StoredSession, String> {
        let (reply, result) = flume::bounded(1);
        self.rewind_tx
            .as_ref()
            .ok_or(capabilities::UNAVAILABLE_REMOTE)?
            .send(RewindRequest {
                session: self.session().ok_or(capabilities::SESSION_OPENING)?,
                index,
                prompt,
                reply,
            })
            .map_err(|_| capabilities::CONNECTION_LOST.to_owned())?;
        result
            .recv()
            .map_err(|_| capabilities::CONNECTION_LOST.to_owned())?
    }

    /// The session the client is bound to, which names the tab, also while
    /// a `/new` is still opening the next one.
    pub(crate) fn name(&self) -> Option<String> {
        self.client.bound_session().ok().map(|id| id.to_string())
    }
}

/// Channels and task for one tab's ACP connection.
pub(crate) struct AgentHandles {
    pub(crate) agent_rx: flume::Receiver<Envelope>,
    pub(crate) answer_tx: flume::Sender<AnsweredRequest>,
    pub(crate) queue: QueueSender,
    cancels: Arc<RunCancels>,
    task: AbortOnDropHandle<()>,
    remote_usage: UsageSlot,
    remote_settings: RemoteSettings,
    /// A remote tab's loop owns the connection, so `/new` hands it new input
    /// channels instead of respawning it.
    rebind_tx: flume::Sender<Rebind>,
    pub(crate) remote_btw: Option<RemoteSession>,
    pub(crate) remote_quota: Option<RemoteSession>,
    pub(crate) remote_login: Option<RemoteLogin>,
    /// The session a remote tab is bound to, for what it keeps of its own.
    pub(crate) remote_link: RemoteSession,
    pub(crate) remote_plan: Option<RemotePlan>,
    /// Shared with the loop, which binds each replacement it installs.
    binding: Arc<SessionBinding>,
}

impl AgentHandles {
    pub(crate) fn apply_to_app(&self, app: &mut App) {
        app.answer_tx = Some(self.answer_tx.clone());
        app.queue.set_shared(self.queue.clone());
        app.watch_remote_usage(Some(self.remote_usage.clone()));
        app.remote_settings = Some(self.remote_settings.clone());
        app.remote_plan = self.remote_plan.clone();
        app.remote_link = Some(self.remote_link.clone());
        if self.remote_link.rewind_tx.is_some() {
            let reader = app.mcp_picker.set_remote(self.remote_link.clone());
            app.command_palette.set_mcp_reader(reader);
        }
    }

    /// Esc: stops {run_id} and everything queued behind it.
    pub(crate) fn cancel_run(&self, run_id: u64) {
        self.cancels.cancel(run_id);
    }

    pub(crate) fn cancel_subagent(&self, tool_use_id: String) {
        if let Some(session_id) = self.remote_link.session() {
            let client = Arc::clone(&self.remote_link.client);
            tokio::spawn(async move {
                if let Err(error) = client
                    .session_operation(SessionOperation::CancelSubagent {
                        session_id,
                        id: tool_use_id,
                    })
                    .await
                {
                    warn!(%error, "could not cancel subagent");
                }
            });
        }
    }

    /// Respawn or shutdown: this loop is done, whatever it was in the middle of.
    pub(crate) fn cancel_all(&self) {
        self.cancels.cancel_all();
    }

    pub(crate) fn send_mcp(&self, server: String, enabled: bool) -> Result<(), String> {
        self.remote_link
            .request::<maki_types::mcp::McpSnapshot>(|session_id| SessionOperation::Mcp {
                session_id,
                toggle: Some((server, enabled)),
            })?;
        Ok(())
    }

    /// `/new` replaces the queue only after the loop has its successor,
    /// so the loop never reads the closed queue as a shutdown.
    pub(crate) fn rebind(&mut self, app: &mut App) {
        app.run_id += 1;
        self.rebind_queue(app.run_id);
        app.queue.set_shared(self.queue.clone());
        app.watch_remote_usage(Some(self.remote_usage.clone()));
        app.usage_slot = Arc::default();
    }

    fn rebind_queue(&mut self, run_id: u64) {
        let (queue_tx, queue_rx) = shared_queue::queue();
        let usage = UsageSlot::default();
        let _ = self.rebind_tx.send(Rebind {
            queue: Arc::new(queue_rx),
            run_id,
            usage: Arc::clone(&usage),
            generation: self.binding.request(),
        });
        self.queue = queue_tx;
        self.remote_usage = usage;
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Hand back the agent task, dropping every channel so the loop can
    /// wind down. The caller sends `CancelAll` first and then awaits all
    /// tabs at once via [`join_all`] instead of paying a serial timeout
    /// per tab.
    pub(crate) fn into_task(self) -> AbortOnDropHandle<()> {
        self.task
    }
}

/// Wait for every agent task under one shared timeout, not one per task.
pub(crate) fn join_all(tasks: Vec<AbortOnDropHandle<()>>, timeout: Duration) {
    info!(
        count = tasks.len(),
        "waiting for agents to finish (timeout {timeout:?})"
    );
    let joined = Handle::current().block_on(tokio::time::timeout(timeout, async {
        for task in tasks {
            task.await.expect("agent task panicked");
        }
    }));
    if joined.is_err() {
        warn!("agents did not finish within {timeout:?}, forcing shutdown");
    }
}

/// Start a tab's ACP loop and its channels.
/// `seed` is the replay's usage, stored before the loop starts so any live
/// update, which is newer, replaces it.
pub(crate) fn spawn_remote(
    mut remote: RemoteAgent,
    seed: Option<SessionUsage>,
    cwd: PathBuf,
    lua: EventHandle,
) -> AgentHandles {
    let (agent_tx, agent_rx) = flume::unbounded::<Envelope>();
    let (answer_tx, answer_rx) = flume::unbounded::<AnsweredRequest>();
    let (queue_tx, queue_rx) = shared_queue::queue();
    let (rebind_tx, rebind_rx) = flume::unbounded();
    let (rewind_tx, rewind_rx) = flume::unbounded();
    let cancels = RunCancels::new();
    let usage = UsageSlot::new(ArcSwapOption::from_pointee(seed));
    let binding = Arc::<SessionBinding>::default();
    let link = RemoteSession {
        client: Arc::clone(&remote.client),
        binding: Arc::clone(&binding),
        rewind_tx: remote.extensions.sessions.then_some(rewind_tx),
    };
    let side = |agreed: bool| agreed.then(|| link.clone());
    let plan_writes = remote.extensions.plan.then(PlanWrites::default);
    let remote_plan = plan_writes.clone().map(|writes| RemotePlan {
        writes,
        link: link.clone(),
    });
    let remote_btw = side(remote.extensions.btw);
    let remote_quota = side(remote.extensions.usage_ledger);
    let remote_login = remote
        .login
        .clone()
        .filter(|_| remote.extensions.login)
        .map(|invocation| RemoteLogin {
            invocation,
            client: Arc::clone(&remote.client),
        });
    let remote_loop = RemoteAgentLoop::new(
        &mut remote,
        agent_tx.clone(),
        answer_rx,
        Arc::new(queue_rx),
        Arc::clone(&cancels),
        Arc::clone(&usage),
        cwd.clone(),
        rebind_rx,
        rewind_rx,
        Arc::clone(&binding),
        lua,
        plan_writes,
    );
    AgentHandles {
        agent_rx,
        answer_tx,
        queue: queue_tx,
        cancels,
        task: AbortOnDropHandle::new(tokio::spawn(remote_loop.run())),
        remote_usage: usage,
        remote_settings: remote.settings,
        rebind_tx,
        remote_btw,
        remote_quota,
        remote_login,
        remote_link: link,
        remote_plan,
        binding,
    }
}

/// An ACP connection with its session already open, handed to the first tab.
pub struct RemoteAgent {
    pub client: Arc<AcpClient>,
    pub events: flume::Receiver<ClientEvent>,
    /// The Maki extensions both sides agreed on.
    pub extensions: Agreed,
    /// The agent takes images in prompts.
    pub images: bool,
    /// The agent advertised `build` and `plan`, so Tab switches its mode.
    pub plan_modes: bool,
    /// The agent's mode when the session opened.
    pub mode: Option<String>,
    /// The wire position of the `session/new` or `session/load` answer the
    /// mode and menu came from, so only later updates replace them.
    pub session_seq: u64,
    /// The history a loaded session replayed, drawn before the first frame.
    pub replay: Vec<SessionUpdate>,
    /// Replayed lines the client could not read.
    pub unreadable: usize,
    /// Shared model, mode and preference configuration.
    pub settings: RemoteSettings,
    /// The agent offered a terminal login this client can run.
    pub login: Option<LoginInvocation>,
    /// Where the tab keeps what it typed, when the user's store resolves.
    pub input: Option<RemoteInput>,
    /// The agent loads sessions, so a reload of the client's plugins
    /// reopens this one instead of ending the run.
    pub reload: bool,
    /// Events the client read off the link after the session opened and
    /// before this tab's loop started, which the loop handles first, in
    /// order.
    pub early: Vec<ClientEvent>,
}

/// A remote tab's own input, in the user's store: its history for the
/// workspace, and its unsent draft and queue per session.
pub struct RemoteInput {
    pub store: RemoteStore,
    pub history: PathBuf,
    /// The session was loaded, so its record is restored.
    pub resumed: bool,
}
