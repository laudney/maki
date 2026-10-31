use clap::Parser;
use maki_acp_client::ConnectArgs;

#[derive(Parser)]
#[command(
    name = "maki-acp-client",
    version,
    about = "The maki terminal UI against an ACP agent, over WebSocket or a child process"
)]
struct Cli {
    #[command(flatten)]
    connect: ConnectArgs,
    /// Run plugin Lua on the interpreter with full debug info (no native codegen)
    #[arg(long)]
    no_jit: bool,
    /// Run no `ui.lua`, no UI package and no bundled UI plugin, such as
    /// `/thinking` or `/sessions`: the client has no builtin it must keep.
    /// Use this to rule out a plugin when the UI misbehaves.
    #[arg(long)]
    no_plugins: bool,
}

fn main() {
    let runtime = tokio::runtime::Runtime::new().expect("cannot start Tokio runtime");
    let _runtime = runtime.enter();
    color_eyre::install().ok();
    let cli = Cli::parse();
    maki_otel::exit(maki_acp_client::connect(
        cli.connect,
        cli.no_jit,
        cli.no_plugins,
    ))
}
