use clap::{Args, value_parser};

const MAX_BACKGROUND_TIMEOUT_SECS: u64 = 7 * 24 * 60 * 60;

#[derive(Args)]
pub struct HostFlags {
    /// Skip user `init.lua` and `ui.lua` files (global and project) and
    /// installed packages but keep the Lua host and every builtin plugin
    /// running, so tools and the default keymap still load. Use this to
    /// recover from a broken `init.lua` or keymap override. Only Lua
    /// config and packages are affected; `permissions.toml`, custom
    /// commands, and env files load as usual.
    #[arg(long)]
    pub no_plugins: bool,

    /// Run plugin Lua on the interpreter with full debug info (no native codegen)
    #[arg(long)]
    pub no_jit: bool,

    /// Load shared project `.maki` config for this run without asking and
    /// without recording an answer. For containers and CI, where the state
    /// directory is thrown away anyway. Only use it on a project you trust.
    #[arg(long)]
    pub trust: bool,
}

#[derive(Args)]
pub struct AcpArgs {
    /// Model spec (provider/model-id)
    #[arg(short, long)]
    pub model: Option<String>,
    /// Skip all permission prompts
    #[arg(long)]
    pub yolo: bool,
    /// Log in to a provider interactively and exit, as `maki auth login`;
    /// the terminal auth method an ACP client runs for `/login`
    #[arg(long)]
    pub login: bool,
    /// Set to zero to keep tool calls in the foreground. ACP keeps
    /// background results until the next prompt and stops them on close.
    #[arg(long, value_name = "SECS", value_parser = value_parser!(u64).range(..=MAX_BACKGROUND_TIMEOUT_SECS))]
    pub background_timeout: Option<u64>,
}
