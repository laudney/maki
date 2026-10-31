# Remote agents over ACP: implementation, step 6k

Status: proposed. Step 6k of the [design](acp-remote-agent-design.md)
turns `/login` on for a remote tab whose agent `maki connect` spawned,
with ACP's standard terminal authentication, so no credential crosses the
link and the agent keeps owning its credentials.

## What the local TUI does

- `/login` opens the login picker: pick a provider (a plan, a URL or a
  custom provider where needed), then type the API key, or follow a
  browser OAuth flow. The picker stores the credentials in the local state
  dir and config.
- When it is done, `Action::RefreshProvider { slug }` rebuilds the model
  slots of every tab on that provider, or switches the focused tab to the
  provider's default model when no tab uses it.
- `maki auth login [provider]` is the same flow on the command line.

## Today on a remote tab

`/login` flashes `UNAVAILABLE_REMOTE`: the picker would write this
machine's credentials, which a remote agent does not read.

## Terminal authentication

ACP's `AuthMethod::Terminal` lets a client run "the configured agent
invocation" with extra `args` and `env` as a separate interactive process,
without passing the method to `authenticate`. A client may only be offered
one when it sets `clientCapabilities.auth.terminal`, meaning it can
reproduce that invocation in an interactive terminal.

### Agent

- `maki acp` gains `--login`: it runs the interactive provider login of
  `maki auth login`, with the same plugin and trust setup, on the agent's
  own state dir, then exits instead of serving.
- When the client advertised `auth.terminal` and agreed on the login
  extension below, the `initialize` answer lists one method: `Terminal {
  id: "maki-login", name: "Log in to a provider", args: ["--login"] }`.
  Otherwise `authMethods` stays empty, as today: a client that could run
  the login but would never say it ran would leave the agent on its old
  credentials.

### Picking up new credentials

A login can add or replace a key, a plan, an endpoint or a custom
provider, so the agent rebuilds rather than reloads. The login extension
(key `login`, version 1, in `Agreed.login`) adds one client notification,
`_maki/auth/changed`, params `{}`, sent after the terminal login process
exits. On it `maki acp`:

- tells the running session's driver to rebuild through a coalescing
  channel on `InteractiveHandle` (`bounded(1)`, as `settings_saved` is).
  At the start of its next input the driver resolves its model afresh from
  its spec (`Model::from_spec`, keeping the old model when the spec no
  longer resolves) and connects it with `from_model_async`, so nothing a
  provider adjusted under the old credentials survives. A run already out
  keeps its provider to its end (see Limits).
- resolves the session's model afresh the same way (`resolved`), so the
  options it offers (thinking, fast) follow what the new configuration
  allows, and sends the full `config_option_update`.
- starts a new model discovery, as a local login's `RefreshModels`
  does. Each discovery has a generation: the model list resets to the
  offline list as it reads now, and only the latest generation's batches
  merge, so a late batch of an older one is ignored and a model a replaced
  account no longer reaches drops out. The reset and every batch that
  changes the list send the full `config_option_update`.
- when the latest discovery completes, rebuilds once more, as the local
  loop rebuilds its model slots when a discovery lands: the driver is
  asked to rebuild before its next input, the session model is resolved
  afresh and its options are sent, even when the model ids did not change,
  since the new listing can change what a model supports (fast, for one).

**A driver whose first connection failed.** Today the interactive driver
returns when its first `connect` fails, leaving a session that answers no
prompt, so a login could not recover it. The driver instead keeps the
session and holds no provider: every input first connects when it has
none, and a failed connect reports its error for that input, as a model
switch's does, and waits for the next. The transcript and the session
are kept.

Side requests (`/btw`, the quota) connect a provider of their own each
time, so they read the new configuration without more.

### Client

- `maki connect` sets `auth.terminal` only for an agent it spawned over
  stdio, and keeps that invocation: program, arguments and environment.
  Over WebSocket it does not, since the agent program is not on this
  machine.
- `RemoteAgent` and the remote `AgentHandles` carry `RemoteLogin {
  program, args, env }` when the agent offered a terminal method: the
  spawn's arguments with the method's `args` appended, and its `env` over
  the spawn's.
- `AgentBackend::Remote` gains `login`, which `Feature::Login` follows;
  without it `/login` still flashes `UNAVAILABLE_REMOTE`.
- On a remote tab, `/login` yields `Action::RemoteLogin`. The event loop
  suspends the TUI as it does for `$EDITOR` (pausing input, tearing the
  terminal down), runs the login invocation with inherited stdio, waits
  for it, and resumes. It then sends `_maki/auth/changed` when the
  extension is agreed, and flashes the outcome: the exit status, or why
  the process could not start; the TUI is restored on every one of them.
- **Ctrl+C during the login.** The terminal is in cooked mode while the
  child runs, so Ctrl+C signals the whole foreground process group. The
  client installs a no-op `SIGINT` handler for the wait and restores the
  previous action after: the login child, whose handlers `exec` resets to
  the default, is interrupted, and `maki connect` is not. An ignored
  signal would be inherited across `exec`, so the handler is a real one.
  The running ACP agent is spawned in a process group of its own
  (`Link::spawn`), so no terminal signal reaches it, during a login or
  ever: the client ends it through its pipes, as today.

## Limits

- Discovery publishes provider metadata to the shared registry from
  detached workers in `fetch_all_models`, and a worker of an older
  discovery that completes last can still overwrite a newer one's. The
  local login runs the same discovery and shares this; the fix belongs in
  `maki-providers` and is not part of this step.

- New credentials apply from the next prompt. Locally, the agent reads
  its model slot between turns, so a rebuilt provider reaches a run
  already out at its next turn; a remote run keeps its provider to its
  end, also for an automatic compaction within it.

- After a local login the TUI switches to the new provider's default
  model when no tab used it. The terminal login runs inside the agent's
  own process, so the client does not learn which provider it was; the
  new models appear in `/model` instead.
- A WebSocket agent offers no terminal login, since its program is not on
  this machine; `/login` stays unavailable there.

## Tests

- Agent: `initialize` lists the terminal method only when the client set
  `auth.terminal` and agreed on `login`; `_maki/auth/changed` asks the
  driver to rebuild, re-resolves the session model and sends its options,
  and starts a discovery generation; a batch of an older generation is
  ignored, the reset drops a model the new listing lacks, and the latest
  discovery's end rebuilds and sends the options again, also when the
  model ids are unchanged.
- Driver: a failed first connect leaves the session up, and the next
  input connects and runs; after a rebuild request the next input resolves
  and connects afresh, also for several requests.
- Extensions: `login` agrees alone and with the others.
- Connect: the login invocation is the spawn's with the method's `args`
  and `env`; a WebSocket agent has none; a spawned agent runs in its own
  process group.
- App: `/login` on a remote tab with a login yields `Action::RemoteLogin`,
  without one flashes `UNAVAILABLE_REMOTE`.
- Event loop: the TUI is restored after a login that exits nonzero and
  after one that cannot start; a `SIGINT` during the wait ends the child
  and not the client (a test that signals itself while a child sleeps).

## Checks

Full `just ci`. A live run of `maki connect -- maki acp` runs `/login`,
returns to the TUI, and lists the new provider's models in `/model`.
