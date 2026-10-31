# Agent and UI split: stage 4, binaries

Status: proposed. Stage 4 of the [implementation
design](agent-ui-split-implementation.md): two new packages,
`maki-acp-agent` and `maki-acp-client`, give `maki acp` and `maki connect`
binaries of their own, and a dependency test per binary lists the edges
the later stages still cut. Behavior does not change: each binary does
exactly what its `maki` subcommand does today.

## Today

- The root package is one binary. `src/main.rs` declares every module
  (`cli`, `cmd`, `setup`, `provider_scripts`, `update`, and the rest), and
  `cmd::dispatch` runs a `Cli`.
- `maki acp` is `Command::Acp { model, yolo, login }`. It reads the root
  flags `--no-plugins`, `--no-jit` and `--trust`. With `--login` it runs the
  provider login (`cli_stack`, then `subcmd::auth_login`), which is the ACP
  terminal auth method: a client runs the agent's own command with
  `--login` (`maki_wire::login::LOGIN_FLAG`). Without it, `cmd::acp::run`
  loads the config and the plugins, resolves the model and serves ACP.
- `maki connect` is `Command::Connect { url, prompt, token_env, cwd,
  exit_on_done, continue_session, resume, agent }`, with the root flag
  `--no-jit`, run by `cmd::connect::run`.
- `main` installs `color_eyre`, drains telemetry with
  `maki_otel::shutdown`, and prints an error chain before exiting 1.
- Both paths use root-only helpers: `cmd::load_plugins`,
  `report_warnings`, `cli_stack`, `setup` and `provider_scripts`, and
  through `provider_scripts` the self-updater `update`.

## Design

- **A library target for the root.** The module declarations move from
  `src/main.rs` to a new `src/lib.rs`, so the root package `maki` has a
  library as well as its binary. `src/main.rs` keeps only `main`. Nothing
  moves to another directory, so upstream changes to `src/` still apply
  where they did.
- **Shared arguments.** The fields of `Command::Acp` become a
  `#[derive(clap::Args)] pub struct AcpArgs`, and those of `Command::Connect`
  a `ConnectArgs`, which the variants flatten. The three root flags that
  the agent side reads, `--no-plugins`, `--no-jit` and `--trust`, become a
  flattened `HostFlags`, so each binary takes them with the same names and
  help text. `Cli` flattens it where `--no-plugins` is today, and `--yolo`,
  which sits between `--no-jit` and `--trust` today, moves to just before
  the group. That one line in `maki --help` is the only change to the `maki`
  command line: every flag keeps its name and help text, and `maki acp
  --help` and `maki connect --help` do not change.
- **Entry points.** The library exposes a small public surface and keeps
  the rest private:
  - `pub fn run() -> !`, today's `main`: it installs `color_eyre`, parses
    `Cli`, dispatches and exits, so `src/main.rs` is `fn main() {
    maki::run() }`;
  - `pub fn acp(args: AcpArgs, flags: &HostFlags) -> Result<()>`, the
    body of today's `Command::Acp` arms, `--login` included;
  - `pub fn connect(args: ConnectArgs, no_jit: bool) -> Result<()>`, the
    body of today's `Command::Connect` arm;
  - `pub fn exit(result: Result<()>) -> !`, today's tail of `main`
    (telemetry drain, error chain, exit code), which all three binaries
    call.

  `cmd::dispatch` calls `acp` and `connect` too, so there is one path per
  command.
- **The packages.** `maki-acp-agent/` and `maki-acp-client/` are workspace
  members. Each has one `src/main.rs` with a `#[derive(Parser)]` command
  (`AcpArgs` and `HostFlags`, or `ConnectArgs` and `--no-jit`), which
  installs `color_eyre`, calls the entry point and passes its result to
  `exit`. Each depends on `maki`, `clap` and `color-eyre`.
  The terminal auth method carries only `--login`, and a client appends
  it to the agent command it spawned (`terminal_login` in
  `src/cmd/connect.rs`), so a client that spawned `maki-acp-agent --model
  M` runs `maki-acp-agent --model M --login`. `AcpArgs` keeps today's
  rule that `--login` runs the login whatever else is given, so the
  binary needs nothing more.
- **Where the code goes later.** The packages wrap the root library for
  now, so they reach everything it links. The code of each side moves into
  its package in the stage that cuts that side's edges: the agent start
  (`load_plugins`, the model, the plugin host) in stage 5, when it becomes
  the agent composition, and the client start in stage 6, when it runs the
  UI runtime. Moving the agent start into a shared crate now would only
  move code that stage 5 rewrites. Until then `maki acp` and `maki
  connect` are not aliases of the packages, as the design has them in the
  end, but the other way round.
- **Dependency tests.** `just deps-check` gives each checked package two
  lists: crates it must not reach, and the subset it still reaches and a
  later stage cuts. It fails when the normal tree has a forbidden crate
  that is not in the pending list, and when a pending crate is no longer
  reached, so the list stays exact as stages cut edges:

  | Package | Must not reach | Still reached |
  | --- | --- | --- |
  | `maki-types`, `maki-wire` | `maki-agent`, `maki-providers` | none |
  | `maki-acp-agent` | `maki-ui`, `maki-lua-ui`, `crossterm`, `ratatui` | `maki-ui`, `crossterm`, `ratatui` |
  | `maki-acp-client` | `maki-agent`, `maki-providers`, `maki-lua`, `maki-acp` | all four |

  The binary column of the design's end state is the first list. The test
  reads crate names from `cargo tree -e normal --prefix none`, so a crate
  counts wherever in the tree it appears.

## Tests

The command-line structs gain no tests of their own: clap's derive is
checked by the build, and a parse test would repeat the attribute. The
existing CLI parse tests keep passing. No generated doc reads the clap
command, so the help output is checked by hand: `maki --help`, `maki acp
--help` and `maki connect --help` are saved before the change and
compared after it, and the only difference is the `--yolo` entry of `maki
--help` moving up above `--no-plugins` and `--no-jit`. The dependency tests are the new check.

## Checks

Full `just ci`. Live: `maki-acp-client -- target/debug/maki-acp-agent`
answers a prompt, `maki connect -- target/debug/maki-acp-agent` does too,
and `/login` in that session runs `maki-acp-agent --login`. `maki acp` and
`maki connect` behave as before.
