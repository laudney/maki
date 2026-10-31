//! The exit every maki binary takes, here because it drains telemetry before
//! `process::exit` skips every destructor.

use std::process;
use std::time::Duration;

use color_eyre::{Report, Result};

/// How long a final telemetry export may take before maki stops waiting.
pub const TELEMETRY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub fn exit(result: Result<()>) -> ! {
    // Detached export tasks die with the process; drain them here, before
    // the `exit` below skips every destructor.
    crate::shutdown(TELEMETRY_SHUTDOWN_TIMEOUT);
    if let Err(e) = result {
        print_error(&e);
        process::exit(1);
    }
    process::exit(0)
}

fn print_error(e: &Report) {
    const RED: &str = "\x1b[31m";
    const BOLD_RED: &str = "\x1b[1;31m";
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";

    eprintln!();
    eprintln!("{BOLD_RED}✖ {e}{RESET}");
    let causes: Vec<_> = e.chain().skip(1).collect();
    let last = causes.len().saturating_sub(1);
    for (i, cause) in causes.iter().enumerate() {
        let branch = if i == last { "└─" } else { "├─" };
        eprintln!("{DIM}{branch}{RESET} {RED}{cause}{RESET}");
    }
    eprintln!();
}
