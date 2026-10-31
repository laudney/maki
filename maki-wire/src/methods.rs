use agent_client_protocol_schema::ProtocolVersion;
use agent_client_protocol_schema::v1::{
    AgentCapabilities, AuthMethod, AuthMethodTerminal, Implementation, InitializeResponse,
    LoadSessionResponse, McpCapabilities, NewSessionResponse, PromptCapabilities,
    SessionCapabilities, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionListCapabilities, SessionMode, SessionModeId,
    SessionModeState,
};
use maki_types::input::AgentMode;

use crate::login;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const MODE_BUILD: &str = "build";
pub const MODE_PLAN: &str = "plan";

pub const MODEL_CONFIG_ID: &str = "model";

/// `terminal_login` when the client can run the agent's invocation in its
/// terminal and will tell the agent the login ran.
pub fn initialize_response(terminal_login: bool) -> InitializeResponse {
    let login = AuthMethodTerminal::new(login::METHOD_ID, login::METHOD_NAME)
        .args(vec![login::LOGIN_FLAG.to_owned()]);
    let auth_methods = match terminal_login {
        true => vec![AuthMethod::Terminal(login)],
        false => Vec::new(),
    };
    InitializeResponse::new(ProtocolVersion::V1)
        .agent_capabilities(
            AgentCapabilities::new()
                .load_session(true)
                .prompt_capabilities(PromptCapabilities::new().image(true).embedded_context(true))
                .mcp_capabilities(McpCapabilities::new().http(true))
                .session_capabilities(
                    SessionCapabilities::new().list(SessionListCapabilities::new()),
                ),
        )
        .auth_methods(auth_methods)
        .agent_info(Implementation::new("maki", VERSION))
}

pub fn mode_state(current: &str) -> SessionModeState {
    SessionModeState::new(
        SessionModeId::from(current.to_string()),
        vec![
            SessionMode::new(SessionModeId::from(MODE_BUILD.to_string()), "Build"),
            SessionMode::new(SessionModeId::from(MODE_PLAN.to_string()), "Plan"),
        ],
    )
}

pub fn new_session_response(session_id: &str) -> NewSessionResponse {
    NewSessionResponse::new(session_id.to_string()).modes(mode_state(MODE_BUILD))
}

pub fn load_session_response() -> LoadSessionResponse {
    LoadSessionResponse::new().modes(mode_state(MODE_BUILD))
}

pub fn model_config_option(current: &str, specs: &[String]) -> SessionConfigOption {
    let mut options: Vec<SessionConfigSelectOption> = specs
        .iter()
        .map(|spec| SessionConfigSelectOption::new(spec.clone(), spec.clone()))
        .collect();
    if !specs.iter().any(|spec| spec == current) {
        options.insert(
            0,
            SessionConfigSelectOption::new(current.to_string(), current.to_string()),
        );
    }
    SessionConfigOption::select(MODEL_CONFIG_ID, "Model", current.to_string(), options)
        .category(SessionConfigOptionCategory::Model)
}

pub fn mode_id_to_agent_mode(mode_id: &str) -> Option<AgentMode> {
    match mode_id {
        MODE_BUILD => Some(AgentMode::Build),
        MODE_PLAN => {
            let storage = maki_storage::StateDir::resolve().ok()?;
            let plan_path = maki_storage::plans::new_plan_path(&storage).ok()?;
            Some(AgentMode::Plan(plan_path))
        }
        _ => None,
    }
}
