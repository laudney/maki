//! The event loop of a client, whose one tab's agent runs in another
//! process: what a local agent would read is filled with values a remote one
//! never reads.

use std::sync::Arc;

use crate::agent::remote_models::ModelMenu;
use agent_client_protocol_schema::v1::SessionConfigOption;
use maki_config::project::TrustQuestion;
use maki_config::{AgentConfig, ProjectConfig};
use maki_lua_ui::UiStart;
use maki_storage::StateDir;

use crate::{EventHandle, EventLoopParams, OpenSession, RemoteAgent};

/// Drawn when the agent names a model this build does not know. The client
/// never calls it.
const REMOTE_MODEL_PLACEHOLDER: &str = "anthropic/claude-sonnet-4-20250514";

/// The client's own project, from the directory it started in.
pub struct ClientProject {
    pub config: ProjectConfig,
    /// What `/trust` grants, and the user's own store it records the grant
    /// in; `None` when nothing is left to answer or no store resolves.
    pub trust: Option<(TrustQuestion, StateDir)>,
}

impl EventLoopParams {
    /// {config_options} come with the session the agent opened: they name the
    /// model drawn and seed the settings. {ui} is the client's UI host with
    /// what its `ui.lua` set. {storage} backs only the tab's local identity,
    /// and no plugin of a client adds a permission rule.
    pub fn remote(
        remote: RemoteAgent,
        config_options: &[SessionConfigOption],
        ui: &UiStart,
        cwd: &str,
        storage: StateDir,
        project: ClientProject,
        exit_on_done: bool,
    ) -> Self {
        let model = display_model(config_options);
        let open = OpenSession::fresh(&model, cwd, &storage);
        let host = &ui.host;
        let (trust_question, trust_store) = match project.trust {
            Some((question, store)) => (Some(question), store),
            None => (None, storage.clone()),
        };
        let project_config = project.config;
        Self {
            agent_factory: None,
            prepare_pack: Arc::new(maki_lua_ui::prepare_pack_command),
            needs_login: false,
            commands: Vec::new(),
            sessions: vec![open],
            focused: 0,
            startup_warnings: ui.warnings.clone(),
            startup_notice: None,
            startup_alert: None,
            storage,
            config: AgentConfig::default(),
            ui_config: ui.ui_config.clone(),
            input_history_size: ui.input_history_size,
            yolo: false,
            exit_on_done,
            background_wait: None,
            lua_command_reader: host.command_reader(),
            keymap_reader: host.keymap_reader(),
            hint_reader: host.hint_reader(),
            input_rules_reader: host.input_rules_reader(),
            ui_action_rx: host.ui_action_rx(),
            ui_wake_rx: host.ui_wake_rx(),
            ui_attachment: host.ui_attachment(),
            lua_event_handle: EventHandle::from_ui_host(host),
            login_catalog: None,
            project_config,
            trust_question,
            trust_store,
            remote: Some(remote),
        }
    }
}

/// The status bar names the agent's current model. Only the names come from
/// the agent: resolving its spec here could read this machine's provider
/// configuration, which has nothing to do with a remote agent.
fn display_model(options: &[SessionConfigOption]) -> String {
    ModelMenu::from_options(options)
        .and_then(|menu| menu.current)
        .unwrap_or_else(|| REMOTE_MODEL_PLACEHOLDER.into())
}

#[cfg(test)]
mod tests {
    use crate::RemoteSettings;
    use maki_lua_core::pack::{Interaction, PackRoots};
    use maki_lua_ui::{UiDirs, UiFiles, UiHost};
    use maki_wire::client::{AcpClient, Link};
    use maki_wire::extensions::Agreed;
    use maki_wire::methods::MODEL_CONFIG_ID;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const REMOTE_SPEC: &str = "runinfra/some-model-the-client-cannot-resolve";
    const CWD: &str = "/agent/workspace";
    const THINKING_COMMAND: &str = "/thinking";

    /// Never driven: the event loop is not run.
    fn remote_agent() -> RemoteAgent {
        let (client_tx, _) = flume::unbounded();
        let (_, client_rx) = flume::unbounded();
        let (client, events) = AcpClient::new(Link::new(client_tx, client_rx));
        RemoteAgent {
            client: Arc::new(client),
            events,
            extensions: Agreed::default(),
            images: false,
            plan_modes: false,
            mode: None,
            session_seq: 0,
            replay: Vec::new(),
            unreadable: 0,
            settings: RemoteSettings::from_options(&[]),
            login: None,
            input: None,
            reload: false,
            early: Vec::new(),
        }
    }

    /// What `maki-acp-client` starts with and without `--no-plugins`, with
    /// no file of the user's.
    #[test_case(true ; "bundled_plugins")]
    #[test_case(false ; "no_plugins")]
    fn the_event_loop_reads_the_ui_hosts_commands(bundled_plugins: bool) {
        let _runtime = crate::test_runtime().enter();
        let scratch = TempDir::new().unwrap();
        let files = if bundled_plugins {
            UiFiles::Global
        } else {
            UiFiles::Disabled
        };
        let dirs = UiDirs {
            config: Vec::new(),
            packs: Ok(PackRoots {
                site: scratch.path().to_path_buf(),
                lockfile: None,
            }),
        };
        let ui = UiHost::start(Interaction::Tty, true, files, dirs).unwrap();
        let params = EventLoopParams::remote(
            remote_agent(),
            &[],
            &ui,
            CWD,
            StateDir::from_path(scratch.path().to_path_buf()),
            ClientProject {
                config: ProjectConfig::discover(scratch.path()),
                trust: None,
            },
            false,
        );
        let commands = params.lua_command_reader.load();
        assert_eq!(commands.commands.is_empty(), !bundled_plugins);
        assert_eq!(
            commands
                .commands
                .iter()
                .any(|command| &*command.name == THINKING_COMMAND),
            bundled_plugins
        );
    }

    fn options(config_options: Value) -> Vec<SessionConfigOption> {
        serde_json::from_value(config_options).unwrap()
    }

    fn model_option(current: &str) -> Value {
        json!([{
            "id": MODEL_CONFIG_ID,
            "name": "Model",
            "type": "select",
            "currentValue": current,
            "options": [{ "value": current, "name": current }],
        }])
    }

    #[test]
    fn display_model_names_the_remote_model_without_resolving_it() {
        let model = display_model(&options(model_option(REMOTE_SPEC)));
        assert_eq!(model, REMOTE_SPEC);
    }

    #[test_case(json!([]) ; "no_options")]
    fn display_model_falls_back_to_the_placeholder(config_options: Value) {
        let model = display_model(&options(config_options));
        assert_eq!(model, REMOTE_MODEL_PLACEHOLDER);
    }
}
