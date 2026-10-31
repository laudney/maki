use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use agent_client_protocol_schema::v1::Implementation;
use maki_acp::AcpParams;
#[cfg(test)]
use maki_agent::CancelToken;
use maki_providers::Model;
use maki_ui::{AgentFactory, RemoteAgent, RemoteSettings};
use maki_wire::client::AcpClient;
use maki_wire::extensions;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

type Running = (Weak<AcpClient>, JoinHandle<()>);

#[derive(Default)]
pub struct NativeAgents(Mutex<Vec<Running>>);

impl NativeAgents {
    pub fn factory(self: &Arc<Self>, params: AcpParams) -> AgentFactory {
        let agents = Arc::clone(self);
        Arc::new(move |open| {
            let mut params = params.clone();
            if let Ok(model) = Model::from_spec(&open.session.model) {
                params.model = model;
            }
            params.initial = Some((open.session.clone(), open.claim.clone()));
            let (link, server) = maki_acp::server::memory_link();
            let task = tokio::spawn(maki_acp::server::serve_channels(
                params,
                server.input,
                server.output,
            ));
            let (client, events) = AcpClient::new(link);
            let client = Arc::new(client);
            let opened = Handle::current().block_on(async {
                let initialized = client
                    .initialize(
                        Implementation::new("maki", env!("CARGO_PKG_VERSION")),
                        false,
                    )
                    .await?;
                let loaded = client
                    .load_session(
                        &events,
                        open.session.id.to_string().into(),
                        open.session.cwd.clone().into(),
                    )
                    .await?;
                let options = loaded.response.value.config_options.unwrap_or_default();
                let configuration = RemoteSettings::from_options(&options);
                Ok::<_, color_eyre::Report>(RemoteAgent {
                    client: Arc::clone(&client),
                    events,
                    extensions: extensions::agreed(initialized.meta.as_ref()),
                    images: initialized.agent_capabilities.prompt_capabilities.image,
                    plan_modes: true,
                    mode: loaded
                        .response
                        .value
                        .modes
                        .map(|modes| modes.current_mode_id.to_string()),
                    session_seq: loaded.response.seq,
                    replay: loaded.replay,
                    unreadable: loaded.unreadable,
                    settings: configuration,
                    login: None,
                    input: None,
                    reload: true,
                    early: loaded.early,
                })
            });
            let mut running = agents.0.lock().unwrap_or_else(|error| error.into_inner());
            running.retain(|(_, task)| !task.is_finished());
            running.push((Arc::downgrade(&client), task));
            opened
        })
    }

    pub fn shutdown(&self) {
        let tasks = std::mem::take(&mut *self.0.lock().unwrap_or_else(|error| error.into_inner()));
        if tasks.is_empty() {
            return;
        }
        Handle::current().block_on(async {
            for (client, task) in tasks {
                if let Some(client) = client.upgrade() {
                    client.shutdown(SHUTDOWN_GRACE).await;
                }
                let _ = task.await;
            }
        });
    }
}

impl Drop for NativeAgents {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_config::project::TrustMode;
    use maki_storage::StateDir;
    use maki_ui::AppSession as StoredSession;
    use maki_ui::OpenSession;
    use maki_wire::session::SessionOperation;
    use tempfile::TempDir;

    const MODEL: &str = "anthropic/claude-sonnet-4-20250514";
    const STORED_MODEL: &str = "zai/glm-4.6";
    const TITLE: &str = "host changed the title";

    #[test]
    fn native_reload_keeps_the_live_session_and_its_claim() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _guard = runtime.enter();
        let root = TempDir::new().unwrap();
        let storage = StateDir::from_path(root.path().join("state"));
        let cwd = root.path().to_string_lossy().into_owned();
        let open = OpenSession::fresh(STORED_MODEL, &cwd, &storage);
        let id = open.session.id;
        let params = AcpParams {
            initial: None,
            administration: None,
            plugins: None,
            model: Model::from_spec(MODEL).unwrap(),
            config: Default::default(),
            timeouts: Default::default(),
            initial_wd: root.path().into(),
            prompt_slots: Arc::default(),
            yolo: false,
            defaults: Default::default(),
            model_policy: Arc::default(),
            plugin_rules: Arc::default(),
            trust_mode: TrustMode::Consult,
            trust_policy: Arc::default(),
            on_session_end: None,
            background: true,
            stop: CancelToken::none(),
            storage: storage.clone(),
        };
        let agents = Arc::new(NativeAgents::default());
        let factory = agents.factory(params);
        let remote = factory(&open).unwrap();
        let snapshot = Handle::current().block_on(async {
            remote
                .client
                .session_operation(SessionOperation::SetTitle {
                    session_id: id.to_string().into(),
                    title: TITLE.into(),
                })
                .await
                .unwrap();
            remote
                .client
                .session_operation(SessionOperation::Close {
                    reason: Some(maki_lua::SessionEndReason::Reload),
                    session_id: id.to_string().into(),
                })
                .await
                .unwrap()
        });
        let session: StoredSession = serde_json::from_value(snapshot).unwrap();
        assert_eq!(session.title, TITLE);
        assert_eq!(session.model, STORED_MODEL);
        drop(remote);
        let (_, server) = agents.0.lock().unwrap().pop().unwrap();
        Handle::current().block_on(server).unwrap();
        assert!(maki_storage::sessions::SessionClaim::acquire(id, &storage).is_err());
        let resumed = factory(&OpenSession {
            session,
            claim: open.claim.clone(),
        })
        .unwrap();
        let snapshot = Handle::current()
            .block_on(
                resumed
                    .client
                    .session_operation(SessionOperation::Snapshot {
                        session_id: id.to_string().into(),
                    }),
            )
            .unwrap();
        assert_eq!(snapshot["title"], TITLE);
        drop(resumed);
        agents.shutdown();
    }
}
