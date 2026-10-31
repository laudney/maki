//! The core and UI handles of the UI plugin host.

use std::sync::Arc;

use maki_lua_core::handle::CoreHandle;
use maki_lua_ui::{UiHandle, UiHost};

#[derive(Clone)]
pub struct EventHandle {
    pub core: CoreHandle,
    pub ui: UiHandle,
}

impl EventHandle {
    /// A client's: its agent runs in another process.
    pub fn from_ui_host(host: &UiHost) -> Self {
        Self {
            core: host.core_handle(),
            ui: host.ui_handle(),
        }
    }

    #[doc(hidden)]
    pub fn disconnected_for_test() -> Self {
        Self::over(CoreHandle::disconnected_for_test())
    }

    fn over(core: CoreHandle) -> Self {
        Self {
            ui: UiHandle::new(core.clone(), Arc::default()),
            core,
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;

    use maki_lua_core::api::slot::{LayeredTools, PLAN_FORM_SLOT};
    use maki_lua_core::handle::CoreHandle;
    use maki_lua_core::runtime::Request;
    use maki_lua_ui::test_support::{dispatched_command, keybind_ticket};
    use maki_lua_ui::{Key, KeybindTicket, UiHandle};
    use maki_storage::id::MakiId;
    use maki_types::event::SessionEndReason;

    use super::EventHandle;

    /// Observes which requests an [`EventHandle`] sends, without a running
    /// plugin host.
    pub(crate) struct RequestProbe(flume::Receiver<Request>);

    impl RequestProbe {
        /// Next dispatched slash command as `(command, args, depth)`, skipping
        /// other requests.
        pub(crate) fn try_recv_command(&self) -> Option<(String, String, u8)> {
            self.0.try_iter().find_map(dispatched_command)
        }

        /// Next key handed to a plugin binding, skipping other requests. The
        /// chat input fires an autocmd of its own on every keystroke, so a
        /// test that types cannot tell a dispatched binding from an announced
        /// edit without this.
        pub(crate) fn try_recv_keybind(&self) -> Option<Key> {
            self.try_recv_ticket().map(|ticket| ticket.key())
        }

        /// [`Self::try_recv_keybind`] with the whole ticket, for a test that
        /// plays the Lua thread and reports the handler done.
        pub(crate) fn try_recv_ticket(&self) -> Option<KeybindTicket> {
            self.0.try_iter().find_map(keybind_ticket)
        }

        /// Next fired autocmd as `(event, data)`, skipping other requests.
        pub(crate) fn try_recv_autocmd(&self) -> Option<(String, serde_json::Value)> {
            self.0.try_iter().find_map(|req| match req {
                Request::FireAutocmd { event, data } => Some((event, data)),
                _ => None,
            })
        }

        /// Next `SessionEnd` request as the session being left behind and why.
        pub(crate) fn try_recv_end_session(&self) -> Option<(MakiId, SessionEndReason)> {
            self.0.try_iter().find_map(|req| match req {
                Request::EndSession(end) => Some((end.session, end.reason)),
                _ => None,
            })
        }
    }

    /// Both lanes on one channel, so a [`RequestProbe`] sees every request,
    /// including the priority-lane commands and keybind callbacks.
    pub(crate) fn probed_event_handle() -> (EventHandle, RequestProbe) {
        let (tx, rx) = flume::unbounded();
        (
            EventHandle::over(CoreHandle::new(tx.clone(), tx)),
            RequestProbe(rx),
        )
    }

    /// [`probed_event_handle`] for a host with a plugin layering the plan
    /// form, the one case the UI has to ask a chain before it draws.
    pub(crate) fn probed_event_handle_layering_plan_form() -> (EventHandle, RequestProbe) {
        let (mut handle, probe) = probed_event_handle();
        handle.ui = UiHandle::new(
            handle.core.clone(),
            Arc::new(LayeredTools::with_surfaces(&[PLAN_FORM_SLOT])),
        );
        (handle, probe)
    }
}
