//! `/usage` on a remote tab: the ledger arrives with the usage updates, so
//! only the quota is asked for, of the agent's own provider.

use std::sync::Arc;

use maki_wire::usage::QuotaState;

use crate::agent::RemoteSession;
use crate::agent::capabilities::SESSION_OPENING;
use crate::components::usage_modal::{UsageFetch, UsageFetchState};

use super::App;

/// Until the agent answers, it has not said which provider it asks.
const REMOTE_PROVIDER: &str = "Remote agent";

impl App {
    pub(crate) fn refresh_remote_quota(&mut self, remote: RemoteSession) {
        let Some(session) = remote.session() else {
            self.flash(SESSION_OPENING.into());
            return;
        };
        let slot = self.fresh_usage_slot(REMOTE_PROVIDER.into());
        tokio::spawn(async move {
            let fetch = match remote.client.usage_quota(session).await {
                Ok(answer) => UsageFetch {
                    provider: answer.provider,
                    state: match answer.state {
                        QuotaState::Ready(quota) => UsageFetchState::Ready(quota),
                        QuotaState::Unsupported {} => UsageFetchState::Unsupported,
                        QuotaState::Error(message) => UsageFetchState::Error(message),
                    },
                },
                Err(error) => UsageFetch {
                    provider: REMOTE_PROVIDER.into(),
                    state: UsageFetchState::Error(error.to_string()),
                },
            };
            slot.store(Some(Arc::new(fetch)));
        });
    }
}
