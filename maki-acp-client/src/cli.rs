use clap::Args;

#[derive(Args)]
pub struct ConnectArgs {
    /// Agent endpoint: http, https, ws or wss (WebSocket upgrade)
    pub url: Option<String>,
    /// Initial prompt to submit once the session is open
    #[arg(short, long)]
    pub prompt: Option<String>,
    /// Environment variable holding the bearer token
    #[arg(long, value_name = "NAME")]
    pub token_env: Option<String>,
    /// Workspace directory sent to the agent, default the current one
    #[arg(long)]
    pub cwd: Option<String>,
    /// Exit after the initial prompt's run ends
    #[arg(long)]
    pub exit_on_done: bool,
    /// Resume the agent's most recent session in the workspace directory
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    pub continue_session: bool,
    /// Resume the agent's session with this ID
    #[arg(short = 'r', long, visible_alias = "session")]
    pub resume: Option<String>,
    /// Agent command to run over stdio instead of a URL, e.g. `-- maki acp`
    #[arg(last = true)]
    pub agent: Vec<String>,
}
