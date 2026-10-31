//! The host's way into a running core from outside the VM thread: the two
//! request lanes, and the core requests sent on them.

use std::fmt::Display;
use std::time::{Duration, Instant};

use flume::{Receiver, Sender};
use maki_storage::id::MakiId;
use maki_types::event::{Envelope, SessionEndReason};

use crate::agent_autocmd::{HostEvent, host_event_for};
use crate::pack::{PackContext, installed_names};
use crate::runtime::{EndSession, Request};

/// How long the host waits for the Lua thread at a teardown: the
/// `SessionEnd` handlers of a process exit, and the join that follows.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const PACK_STATE_UNAVAILABLE: &str = "could not read package state: plugin host stopped";
const LOCKFILE_UNREADABLE: &str = "could not read the package lockfile";

#[derive(Clone)]
pub struct CoreHandle {
    tx: Sender<Request>,
    /// User-initiated requests bypass queued bulk work (session restores).
    prio_tx: Sender<Request>,
}

impl CoreHandle {
    pub fn new(tx: Sender<Request>, prio_tx: Sender<Request>) -> Self {
        Self { tx, prio_tx }
    }

    #[doc(hidden)]
    pub fn disconnected_for_test() -> Self {
        Self::new(flume::unbounded().0, flume::unbounded().0)
    }

    /// Queues {request} behind the bulk work. False when no runtime drains
    /// the lane.
    pub fn send(&self, request: Request) -> bool {
        self.tx.try_send(request).is_ok()
    }

    /// Queues {request} ahead of the bulk work. False when no runtime drains
    /// the lane.
    pub fn send_prio(&self, request: Request) -> bool {
        self.prio_tx.try_send(request).is_ok()
    }

    /// The one way host code fires an event, so every field it sends is one
    /// [`HostEventKind::fields`](crate::agent_autocmd::HostEventKind::fields)
    /// classifies. {session_id} is the session the event is about.
    pub fn fire(&self, session_id: Option<&dyn Display>, event: HostEvent<'_>) {
        self.send(Request::FireAutocmd {
            event: event.name().to_owned(),
            data: event.payload(session_id),
        });
    }

    pub fn dispatch(&self, session_id: &dyn Display, envelope: &Envelope, subagent_bounded: bool) {
        if let Some(event) = host_event_for(&envelope.event, subagent_bounded) {
            self.fire(Some(session_id), event);
        }
    }

    pub fn fire_autocmd(&self, event: &str, data: serde_json::Value) {
        self.send(Request::FireAutocmd {
            event: event.to_owned(),
            data,
        });
    }

    pub fn package_context(&self) -> Result<PackContext, String> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        if !self.send(Request::CollectPackageContext { reply: reply_tx }) {
            return Err(PACK_STATE_UNAVAILABLE.to_owned());
        }
        let (declared, active) = reply_rx
            .recv()
            .map_err(|_| PACK_STATE_UNAVAILABLE.to_owned())?;
        let installed = installed_names().ok_or_else(|| LOCKFILE_UNREADABLE.to_owned())?;

        Ok(PackContext::new(declared, installed, active))
    }

    /// Queue the kill of session-owned jobs and the `SessionEnd` dispatch,
    /// then return. Call from every session-end path so a Lua monitor can
    /// stay a plugin. Process exit wants [`Self::end_sessions_blocking`].
    ///
    /// Nothing waits here, so handlers get no deadline and the UI is still
    /// there to answer them.
    pub fn end_session(&self, session: MakiId, reason: SessionEndReason) {
        self.send(Request::EndSession(EndSession {
            session,
            reason,
            wait: None,
        }));
    }

    /// [`Self::end_session`] for process exit: block until the handlers ran
    /// and the jobs were reaped, so the `Shutdown` that follows on the
    /// priority lane cannot skip ahead of them.
    ///
    /// Every session is queued first and the deadline is shared, so quitting
    /// with many tabs open costs one `SHUTDOWN_TIMEOUT`, not one per tab.
    pub fn end_sessions_blocking(
        &self,
        sessions: impl IntoIterator<Item = MakiId>,
        reason: SessionEndReason,
    ) {
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let waits: Vec<_> = sessions
            .into_iter()
            .filter_map(|session| {
                Some((session, self.send_end_session(session, reason, deadline)?))
            })
            .collect();
        for (session, reply_rx) in waits {
            let left = deadline.saturating_duration_since(Instant::now());
            if reply_rx.recv_timeout(left).is_err() {
                tracing::warn!(
                    session = %session,
                    "SessionEnd did not finish within timeout, continuing teardown"
                );
            }
        }
    }

    /// [`Self::end_sessions_blocking`] parked on a spare thread. ACP calls
    /// this from its executor, where blocking would freeze stdin for the
    /// whole grace period.
    pub async fn end_session_async(&self, session: MakiId, reason: SessionEndReason) {
        let handle = self.clone();
        tokio::task::spawn_blocking(move || handle.end_sessions_blocking([session], reason))
            .await
            .expect("end_sessions_blocking panicked");
    }

    fn send_end_session(
        &self,
        session: MakiId,
        reason: SessionEndReason,
        deadline: Instant,
    ) -> Option<Receiver<()>> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.send(Request::EndSession(EndSession {
            session,
            reason,
            wait: Some((deadline, reply_tx)),
        }))
        .then_some(reply_rx)
    }
}
