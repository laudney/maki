use clap::Parser;
use maki_host::{AcpArgs, HostFlags};

#[derive(Parser)]
#[command(
    name = "maki-acp-agent",
    version,
    about = "The maki agent as an ACP (Agent Client Protocol) server over stdio"
)]
struct Cli {
    #[command(flatten)]
    acp: AcpArgs,
    #[command(flatten)]
    host: HostFlags,
}

#[tokio::main]
async fn main() {
    color_eyre::install().ok();
    let cli = Cli::parse();
    let result = tokio::task::spawn_blocking(move || maki_host::acp(cli.acp, &cli.host))
        .await
        .unwrap_or_else(|error| Err(error.into()));
    maki_host::exit(result)
}
