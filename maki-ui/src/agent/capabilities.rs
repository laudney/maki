//! Capabilities advertised by the ACP agent behind a tab.

use maki_lua_ui::BuiltinAction;

pub(crate) const UNAVAILABLE_REMOTE: &str = "Not available for a remote agent";
pub(crate) const MODEL_NEXT_PROMPT: &str = "The agent switches to this model with the next prompt";
pub(crate) const CONNECTION_LOST: &str = "The connection to the remote agent was lost";
pub(crate) const SESSION_OPENING: &str = "The new session is still opening";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum AgentBackend {
    /// `images` is what the agent advertised for prompts, `plan` that it
    /// offers the `build` and `plan` modes, `settings`, `btw` and
    /// `usage_ledger` that it agreed on those extensions, `login` that it
    /// offered a terminal login this client runs, `plan_file` that it
    /// agreed on the plan extension, and `reload` that it loads sessions, so
    /// the client can restart its own plugins and reopen this one. Its model
    /// menu and settings can come and go, so each command checks them when
    /// it runs.
    Remote {
        native: bool,
        host: bool,
        images: bool,
        plan: bool,
        settings: bool,
        btw: bool,
        usage_ledger: bool,
        login: bool,
        plan_file: bool,
        reload: bool,
    },
    /// The link is gone. Nothing reaches the agent any more.
    #[default]
    Disconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Feature {
    ModelChange,
    ModeChange,
    Settings,
    Compaction,
    /// `/new` in place. `session/new` is a required ACP method.
    NewSession,
    Sessions,
    Directory,
    Mcp,
    Reload,
    Trust,
    Packages,
    Login,
    Usage,
    Btw,
    LocalFiles,
    PlanFiles,
}

impl AgentBackend {
    pub(crate) fn supports(self, feature: Feature) -> bool {
        match self {
            Self::Remote {
                native,
                host,
                plan,
                btw,
                usage_ledger,
                login,
                plan_file,
                reload,
                ..
            } => match feature {
                Feature::Directory | Feature::Mcp => host,
                Feature::Sessions | Feature::LocalFiles => native,
                Feature::ModeChange => plan,
                Feature::PlanFiles => plan_file,
                Feature::Reload | Feature::Trust | Feature::Packages => reload,
                Feature::Btw => btw,
                Feature::Usage => usage_ledger,
                Feature::Login => login || native,
                Feature::ModelChange
                | Feature::NewSession
                | Feature::Settings
                | Feature::Compaction => true,
            },
            Self::Disconnected => false,
        }
    }

    /// Whether a prompt carries thinking, fast and workflow to the agent.
    pub(crate) fn takes_settings(self) -> bool {
        matches!(self, Self::Remote { settings: true, .. })
    }

    pub(crate) fn is_native(self) -> bool {
        matches!(self, Self::Remote { native: true, .. })
    }

    pub(crate) fn accepts_prompts(self) -> bool {
        self != Self::Disconnected
    }

    /// A remote agent's own word on images replaces the local model's.
    pub(crate) fn takes_images(self) -> bool {
        match self {
            Self::Remote { images, .. } => images,
            Self::Disconnected => false,
        }
    }

    /// `None` when the command needs nothing the agent may lack.
    pub(crate) fn command_feature(name: &str) -> Option<Feature> {
        Some(match name {
            "/compact" => Feature::Compaction,
            "/usage" => Feature::Usage,
            "/btw" => Feature::Btw,
            "/new" => Feature::NewSession,
            "/fork" => Feature::Sessions,
            "/model" => Feature::ModelChange,
            "/mcp" => Feature::Mcp,
            "/login" => Feature::Login,
            "/cd" => Feature::Directory,
            "/yolo" | "/fast" | "/workflow" => Feature::Settings,
            "/reload" => Feature::Reload,
            "/trust" => Feature::Trust,
            "/packupdate" | "/packdel" => Feature::Packages,
            _ => return None,
        })
    }

    pub(crate) fn action_feature(action: BuiltinAction) -> Option<Feature> {
        match action {
            BuiltinAction::Background => Some(Feature::Sessions),
            BuiltinAction::FilePicker => Some(Feature::LocalFiles),
            BuiltinAction::PlanToggle | BuiltinAction::PlanEditor => Some(Feature::PlanFiles),
            BuiltinAction::ModelPicker => Some(Feature::ModelChange),
            BuiltinAction::Search
            | BuiltinAction::Help
            | BuiltinAction::EditInput
            | BuiltinAction::PopQueue
            | BuiltinAction::PrevChat
            | BuiltinAction::NextChat
            | BuiltinAction::InputRaw
            | BuiltinAction::Undo
            | BuiltinAction::Redo
            | BuiltinAction::HistoryPrev
            | BuiltinAction::HistoryNext
            | BuiltinAction::Newline => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("/fork" ; "fork")]
    #[test_case("/cd" ; "cd")]
    #[test_case("/reload" ; "reload")]
    #[test_case("/packdel" ; "packdel")]
    fn commands_require_the_agents_capabilities(name: &str) {
        let feature = AgentBackend::command_feature(name).unwrap();
        assert!(
            !AgentBackend::Remote {
                native: false,
                host: false,
                images: true,
                plan: true,
                settings: true,
                btw: true,
                usage_ledger: true,
                login: true,
                plan_file: false,
                reload: false,
            }
            .supports(feature)
        );
    }

    /// A client restarts its own plugins and reopens the session, which
    /// needs an agent that loads sessions.
    #[test_case("/reload" ; "reload")]
    #[test_case("/trust" ; "trust")]
    #[test_case("/packupdate" ; "packupdate")]
    #[test_case("/packdel" ; "packdel")]
    fn a_client_runs_its_own_commands_when_the_agent_loads_sessions(name: &str) {
        let feature = AgentBackend::command_feature(name).unwrap();
        let remote = |reload| AgentBackend::Remote {
            native: false,
            host: false,
            images: false,
            plan: false,
            settings: false,
            btw: false,
            usage_ledger: false,
            login: false,
            plan_file: false,
            reload,
        };
        assert!(remote(true).supports(feature));
        assert!(!remote(false).supports(feature));
        assert!(!AgentBackend::Disconnected.supports(feature));
    }

    #[test_case("/help" ; "help")]
    #[test_case("/theme" ; "theme")]
    #[test_case("/queue" ; "queue")]
    #[test_case("/exit" ; "exit")]
    fn ui_commands_need_no_feature(name: &str) {
        assert_eq!(AgentBackend::command_feature(name), None);
    }

    #[test_case(AgentBackend::Remote { native: false, host: false, images: true, plan: true, settings: true, btw: true, usage_ledger: true, login: true, plan_file: false, reload: false }, true ; "remote_agent_takes_images")]
    #[test_case(AgentBackend::Remote { native: false, host: false, images: false, plan: true, settings: true, btw: true, usage_ledger: true, login: true, plan_file: false, reload: false }, false ; "remote_agent_refuses_images")]
    #[test_case(AgentBackend::Disconnected, false ; "disconnected")]
    fn images_follow_the_agent(backend: AgentBackend, expected: bool) {
        assert_eq!(backend.takes_images(), expected);
    }
}
