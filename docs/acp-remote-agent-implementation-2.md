# Remote agents over ACP: implementation, steps 3 to 5

Status: proposed. This document builds steps 3, 4 and 5 of the
[design](acp-remote-agent-design.md) on top of
[steps 1 and 2](acp-remote-agent-implementation.md). Step 3 is the
capability value, step 4 the remote loop and `maki connect`, step 5 the
WebSocket link. The live target is a Pantheon listener at
`http://localhost:7803/acp`, a WebSocket upgrade with bearer
authentication.

## Step 3: capabilities

### Value

`maki-ui/src/agent/capabilities.rs`:

```text
pub(crate) enum AgentBackend { Local, Remote { images: bool }, Disconnected }
pub(crate) enum Feature { ModelChange, ModeChange, Settings, Compaction,
    Sessions, Directory, Mcp, Reload, Trust, Packages, Login, Usage, Btw,
    Export, LocalFiles, PlanFiles }
impl AgentBackend {
    fn supports(self, Feature) -> bool; fn accepts_prompts(self) -> bool;
    fn takes_images(self, local_model_vision: bool) -> bool }
```

`Local` supports every feature, so local behavior cannot change. `Remote`
supports none of the listed features and carries the agent's advertised
image capability. `Disconnected` supports nothing and accepts no prompt.
`App` holds one `backend` field, set at construction and changed only to
`Disconnected`. There is no other remote flag. The type is `AgentBackend`
because `ratatui::backend::Backend` is already in scope in the loop.

### Checks

Each check runs before any state changes and reports the existing flash
`UNAVAILABLE_REMOTE` ("not available for a remote agent").

| Entry point | Check |
| --- | --- |
| `App::execute_command` | one lookup from the built-in command name to a `Feature`, before the `match`: `/compact`, `/usage`, `/btw`, `/new`, `/fork`, `/model`, `/mcp`, `/login`, `/cd`, `/yolo`, `/fast`, `/workflow`, `/reload`, `/trust`, `/packupdate`, `/packdel` |
| `App::run_builtin` | one lookup from `BuiltinAction` to a `Feature`, before the `match`: `FilePicker`, `PlanToggle`, `PlanEditor`, `ModelPicker` |
| `App::toggle_mode` (Tab) | `ModeChange` |
| `App::checkpoint_with` | a remote session returns at once: nothing reaches the local store |
| `EventLoop` new tab, load, fork and respawn | `Sessions`: a remote runtime is never respawned and no second runtime is opened from it |
| image paste and file images | `takes_images`: the agent's prompt capability, not the local model's vision flag |
| status bar | thinking, fast, workflow, yolo and restricted are hidden for a non-local backend |
| `App::submit_prompt`, `submit_or_queue` | a disconnected tab refuses the prompt and puts the text and images back in the input |

Commands without a feature row (`/help`, `/theme`, `/queue`, `/exit`)
work unchanged. `maki connect` loads no plugins, so plugin commands and
plugin keymaps (`/sessions`, `/thinking`, `@` completion) do not exist in
a remote session.

### Tests

- Each command and builtin action in the table, on a remote `App`,
  flashes `UNAVAILABLE_REMOTE` and leaves mode, model, session id, cwd and
  queue unchanged.
- The same commands on a local `App` behave as before; the existing app
  tests pass unchanged.
- A remote `App` checkpoint writes nothing to a temporary state dir.

## Step 4: `RemoteAgentLoop` and `maki connect`

### Launch

The remote branch starts before local services. With
`EventLoopParams::remote` set, `EventLoop::new` skips `mcp::start`, builds
`maki_providers::provider::unconfigured()` (which reads no configuration
and constructs no provider), and starts no model discovery, so no model
slot is ever rebuilt. No MCP server runs and no provider credentials are
read. A remote tab clears `restore_event_tx` on the app
and its chats, so a theme change never asks local Lua to restore a remote
row; remote snapshots keep the colors they arrived with.


`src/cmd/connect.rs` runs before any local provider, plugin or init file
setup:

1. Build a `Link` (stdio command or WebSocket, below).
2. `AcpClient::initialize`, then `new_session(cwd)`. A failure exits with
   the error and a non-zero code.
3. `render::agreed` on the response turns on `DisplayMapper::with_render`.
4. Start a `PluginHost` with no plugins and no init files, for the UI
   handles `EventLoop` needs. No tool, provider or MCP service starts.
5. Build the display model from the shipped placeholder spec
   `REMOTE_MODEL_PLACEHOLDER`, renamed to the agent's current model config
   value (`provider/id`) when it has one. The agent's spec is never
   resolved, because resolving an unknown provider reads this machine's
   provider configuration. The model is only drawn; no provider is built
   from it. The context gauge comes from the agent: for a remote backend
   the App applies `Done`'s context size and window, which the mapper
   fills from `UsageUpdate`, since ACP sends no `TurnComplete`.
6. Open one `OpenSession::fresh` whose claim lives in a temporary state
   dir that is removed on exit, before any non-zero `process::exit`, so
   the user's session store is untouched.
7. Run `maki_ui::run` with `EventLoopParams::remote = Some(RemoteAgent)`.

`RemoteAgent` carries the client, its event receiver and the render flag.
`SpawnCtx::spawn_runtime` builds `AgentHandles::spawn_remote` for the
first runtime when it is present, and sets `Backend::Remote` on the `App`.

### Command line

```text
maki connect <URL> [--token-env NAME] [--cwd PATH] [--exit-on-done] [-p PROMPT]
maki connect [--cwd PATH] [--exit-on-done] [-p PROMPT] -- <COMMAND> [ARGS...]
```

- `URL` with `http`, `https`, `ws` or `wss` is a WebSocket endpoint.
- `--token-env` names the variable that holds the bearer token, default
  `MAKI_ACP_TOKEN`. The value never appears in output, logs or errors. An
  explicitly named variable that is unset is an error; the unset default
  connects without a token.
- `--cwd` is sent to the agent unchanged, default the current directory.
- `-- maki acp` runs a child agent over stdio. This is how local use
  reaches `maki acp`. A URL and a command together are an error.
- The prompt is a flag, so it can never be mistaken for the URL.

### Remote handles

`AgentHandles::spawn_remote` makes the same channels as
`spawn_agent_internal` and spawns `RemoteAgentLoop::run` instead of
`AgentLoop::run`. It binds an empty history mirror, no MCP handle and a
mailbox registered under the local display id, which nothing wakes.

### Loop

`maki-ui/src/agent/remote_loop.rs`. One task owns the client (in an
`Arc`, so the pending prompt future does not borrow the loop), the
mapper, the event receiver, the queue receiver, the answer receiver and
the cancel registry.

- Idle, it races the teardown token, the queue notification, client
  events and answers. Updates outside a run are mapped under the last run
  id.
- A queue run drops cancelled items first (`drop_cancelled`, as the local
  loop). `Compact` ends at once with `Error(UNAVAILABLE_REMOTE)`.
  `Messages` emits `QueueItemConsumed` for each input the UI did not
  display, merges inputs as the local loop does, and sends one prompt. For
  each earlier input in order, then the last: its shell-result preamble as
  text, its message as text, its images as image blocks.
- While the prompt is pending the loop races teardown, the prompt future,
  client events, answers and the run's cancel token. Cancel sends
  `session/cancel` once, then that branch is off, and the loop keeps
  waiting for the response. Teardown returns at once without waiting.
- Ordering: the client's reader puts every update in the event channel
  before it resolves the response that follows. When the response is
  ready, the loop first drains the event channel with `try_recv`, then
  emits `mapper.done(stop_reason)` or `Error`. So no earlier update can
  land after `Done` or in the next run. A `Closed` met during the drain
  came after the response: the drain stops there, the run still ends with
  its own result, and only then is the connection reported lost.
- `QueueDrained` follows through `publish_if_empty`, as the local loop.
- `ClientEvent::Closed`, or `Closed` from the prompt, ends the task with
  no run event: the loss belongs to the session, and a run `Error` would
  let the ordinary error path clear the unsent queue first. When `Closed`
  is picked up on the event branch while a prompt is pending, the loop
  first polls the prompt once: the reader settles the response before it
  queues `Closed`, so a known result still ends the run with `Done`. The
  prompt is never resent. The event loop sees the finished remote task
  with an empty event channel and calls `App::connection_lost`, which is a
  session-level transition that bypasses run-id filtering: it ends the
  in-flight turn, closes permission prompts, takes every unsent queued
  message with its images and puts them back in the input ahead of the
  current draft, sets `Disconnected` and fails an `--exit-on-done` exit.
- `ClientEvent::Malformed` becomes `AgentEvent::Notice`.

### Permissions

The existing prompt stays. A `ClientEvent::Permission` becomes
`AgentEvent::PermissionRequest` with a fresh local request id and the
title as the one scope. The tool key is the name the mapper remembered for
the call when it is a valid wire name, else the fixed key `remote`; a
title is never parsed as a tool key. The App only sends the tagged answer
to the agent and writes no local permission rule. The loop keeps
`request id -> ask id and offered options`. An answer from `answer_rx` is
a `TaggedAnswer`; it maps to an offered option by kind, never broader than
the choice:

| TUI answer | ACP option kind |
| --- | --- |
| `AllowOnce` | `allow_once` |
| `AllowSession` | `allow_always`, only when the agent is Maki (`agentInfo.name == "maki"`), whose `allow_always` is a session grant; else `allow_once` |
| `AllowAlwaysProject`, `AllowAlwaysGlobal` | as `AllowSession` |
| `Deny`, `DenyWithGuidance` | `reject_once` |
| `DenyAlwaysProject`, `DenyAlwaysGlobal` | `reject_always`, else `reject_once` |

With no offered option of the mapped kind the loop replies cancelled. A
run end replies cancelled to every pending ask, and a late answer finds no
ask and is dropped. The App closes a remote tab's permission prompts on
`Done` and `Error` of the main chat, and on `connection_lost`. A local
tab is unchanged. Showing the agent's own
option labels is a parity step; this table is the interim mapping.

### Tests

- The remote loop through `spawn_remote` against a scripted peer on a
  channel link: an update before the response is drawn before `Done`;
  cancel sends `session/cancel` and the run ends with the agent's
  cancelled answer; an RPC error ends the run with `Error`; compaction is
  refused; link loss sends `Error(CONNECTION_LOST)` and ends the task;
  teardown ends a prompt the peer never answers; each permission row maps
  to the right offered option; an unanswered ask is cancelled at run end;
  a burst with a shell result and an image keeps its order.
- App tests: each command and action in the table is refused on a remote
  app with no state change; a remote checkpoint copies no history;
  `connection_lost` returns queued text and images with the draft, closes
  prompts, refuses the next submission and fails `--exit-on-done`; a
  remote run end closes its prompts and a local one does not.
- Messages-panel tests through the real mapper and server translator: a
  completed remote row keeps its styled body; a dropped completion shows
  the plain output; a late snapshot restyles the finished row without a
  second row; a theme change asks for no local restore.
- Remote loop: a close right behind the response keeps the run's result
  on either branch; a lost link emits no run error.
- App: a remote `Done` sets the context gauge and window.
- `maki connect`: the display model takes the agent's names without
  resolving its spec, and falls back to the placeholder.
- The real TUI needs a terminal, so there is no subprocess test of
  `maki connect` in CI. The live check below, run under a PTY, covers the
  binary.

## Step 5: WebSocket link

`Link::websocket(url, token)` in `maki-acp/src/client.rs`:

- Dependency: `async-tungstenite` with `smol-runtime` and
  `futures-rustls-native-certs`, added to the workspace. `futures-rustls`
  and `url` are already in the lock file.
- `http` becomes `ws`, `https` becomes `wss`; the path and query stay.
  URL credentials and fragments are refused. A token is sent as
  `Authorization: Bearer`, marked sensitive. Redirects are not followed.
- Two pumps like `Link::spawn`: text frames to lines and lines to text
  frames. Ping is answered by the library. A close frame or error closes
  the link. A handshake error reports the HTTP status and the endpoint
  without the token.

### Tests

- URL conversion and refusal cases.
- A loopback WebSocket fixture: handshake with and without the expected
  token (401 is reported as such), one request and one response through
  the link, and a server close fails a pending request.

### Live check

`MAKI_ACP_TOKEN=<token> maki connect --exit-on-done -p "say OK"
http://localhost:7803/acp` under a PTY, plus an interactive run, and the
same against a local `maki connect -- maki acp`. The token comes from the
user at run time.

Delivery order: the WebSocket link lands as its own commit in `maki-acp`
before the TUI commit, so `maki connect` ships with both transports.

## Checks

Each step runs the full set: `cargo fmt`, workspace clippy with
`-D warnings`, `cargo nextest run --workspace`. Step 4 and 5 also run
`just gen-docs-check`, since `maki connect` adds CLI docs.
