mod cli;
mod cmd;
mod native;
use maki_host::interrupt;
mod print;
mod resume;
mod sdk_mode;
mod terminal_prompt;
mod update;

use clap::Parser;
use tokio::runtime::{Handle, Runtime};

use cli::Cli;

pub use cli::{AcpArgs, HostFlags};
pub use maki_host::exit;

pub fn run() -> ! {
    color_eyre::install().ok();
    let runtime = BackgroundShutdown::start();
    let handle = runtime.handle();
    let guard = handle.enter();
    maki_lua::set_terminal_prompt(terminal_prompt::read_answer);
    let result = cmd::dispatch(Cli::parse());
    maki_otel::shutdown(maki_host::TELEMETRY_SHUTDOWN_TIMEOUT);
    drop(guard);
    drop(runtime);
    exit(result)
}

/// Owns the runtime so that every way out of `main`, a panic included, shuts
/// it down without waiting for a blocking task still parked on stdin.
struct BackgroundShutdown(Option<Runtime>);

impl BackgroundShutdown {
    fn start() -> Self {
        Self(Some(
            Runtime::new().expect("failed to start the async runtime"),
        ))
    }

    fn handle(&self) -> Handle {
        self.0
            .as_ref()
            .expect("the runtime lives until drop")
            .handle()
            .clone()
    }
}

impl Drop for BackgroundShutdown {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}
