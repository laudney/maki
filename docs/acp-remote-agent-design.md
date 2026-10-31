# Remote agents over ACP: design

Status: proposed. This is the smallest shape that lets the original TUI
drive an ACP agent. It fits the first milestone of the
[TUI over ACP design](tui-over-acp-design.md) and the
[upstream maintainer guide](upstream-maintainer-guide.md).

## Goal

`maki connect` runs the original TUI against an agent started with
`maki acp`. The end state is parity: a session through `maki connect` and
`maki acp` behaves like the monolithic `maki`. The agent side loads the
same plugins as `maki`, so every tool, hook and renderer exists there.

The TUI, its input, queue, queue panel, cancel key and renderers are the
existing code. The task behind the agent channels changes, and the TUI
learns that a session can be remote.

Parity arrives in stages. The first milestone is a deliberately reduced
session. The parity ledger below lists each behavior with its owner and
carrier. Parity means equivalent observable behavior with correct
ownership. It does not mean enabling every local resource: the client
never starts a local provider and never writes the agent's session.

## Key idea: replace the consumer, and name the remote session

`AgentHandles` is a bundle of channels. One task, `AgentLoop`
(`maki-ui/src/agent/agent_loop.rs`), is the only consumer of the queue,
the cancel registry and the user answers, and the only producer of run
execution events. Restoration replies also use the output channel, tagged
with `RESTORE_RUN_ID`. `spawn_agent_internal` wires them. A second task with the
same four connections can replace it:

| Connection | Local `AgentLoop` | `RemoteAgentLoop` |
| --- | --- | --- |
| `QueueReceiver` | runs the model/tool loop | sends `session/prompt` |
| `RunCancels` | cancels the run token | sends `session/cancel` |
| `answer_rx` | tagged user answers | replies to `session/request_permission` |
| `agent_tx` | `AgentEvent`s from the loop | `AgentEvent`s mapped from `session/update` |

This reuses the queue, queue panel, merged inputs, `--exit-on-done` and
cancel. It is not a drop-in on its own. The TUI also holds local state
that exists only for a local agent: the model slot and provider, the
session claim and store, settings that change without a queue item, and
`SharedMessages`, the mailbox and the MCP handle. A remote session has
none of them. The design therefore adds one value, the session's
**capabilities**, set once when the session is built. A local session has
every capability, so local behavior does not change.

## Capabilities

The capability set is a small value on the session runtime. Every local
mutator that a remote session cannot honor checks it before any state
changes, and reports "unavailable" through the existing notice path. A
check never follows a state change. The checks are at the shared mutators
and not at each call site. The areas are:

| Area | Remote treatment |
| --- | --- |
| Startup: MCP, provider, model slot, model discovery | none of them start; no local credentials needed |
| Runtime construction and the history mirror | no model slot, no claim, no history mirror bound |
| Model change, `/btw`, usage, refresh, login | unavailable |
| Mode (Tab), `/yolo`, `/fast`, `/workflow`, thinking | unavailable; the status bar shows the agent's published values |
| Permission presentation, client folder trust | not used as remote policy |
| Checkpoint, storage writes, shutdown save | off; a remote session is never written to the local store |
| Reset, respawn, `/new`, `/cd`, `/sessions`, load, fork, rewind, export | unavailable; one session per connection |
| Compaction | unavailable, rejected before the UI enters streaming |
| `/reload`, `/trust`, `/packupdate`, `/packdel` | unavailable; the remote UI asks no local trust question |
| Image attachment | allowed only when the agent's prompt capabilities include images; the model's vision flag is not consulted |
| MCP and subagent control, mailbox | unavailable and inert |
| `@` completion, file picker, plan files | off; an agent path is never a client file |
| Connection state | a lost connection rejects later submission |

Plan editing, shell and editor requests that need no agent path stay
local. Implementation lists the exact mutators for each row.

Every "unavailable" row is a debt in the parity ledger and not a design
choice. The ledger names the carrier and the owner of each one. The ACP
server today handles `session/set_mode` and the model config option only,
so the other carriers are Maki methods that the server also gains.

## Run lifecycle

`RemoteAgentLoop` follows the local loop's contract for each queue run:

1. Emit `QueueItemConsumed` only for an input the UI did not already
   display.
2. Send `session/prompt` and map each `session/update` to an `AgentEvent`
   tagged with the run id the queue assigned.
3. The prompt response ends the run: `Done` with the mapped stop reason,
   or `Error` for an RPC error. A cancel sends `session/cancel`, keeps the
   run pending until the response arrives and ends it as a normal `Done`.
   No new prompt is sent while one is pending.
4. Emit `QueueDrained` when the queue empties.

A link failure is a session-level transition and not a run-scoped event.
It bypasses run-id filtering, because a cancel followed by a new submit
advances the UI run id past the failed run. The transition settles
current UI work, returns unsent queued text and attachments to the input,
and reconciles the exit-on-done drain state. The loop then stops and the
connection is marked lost. Later submission is rejected before the UI
changes state. The client never resubmits a prompt. The loop keeps reading the link while a prompt is
pending, so cancels and permission requests work at any time.

Queued messages go out as one prompt per queue run: the text of the
merged inputs in order, the images as content blocks, and a client shell
result as leading text. A standard agent cannot see message boundaries. This
reduced behavior is documented.

## Display mapping

The mapping is a reduced inverse of `maki-acp/src/translate.rs`, built on
the typed schema. It does not claim to be lossless.

- Text and thinking chunks become `TextDelta` and `ThinkingDelta`.
- A tool call creates a row. It can arrive already completed, and an
  update for an unknown id creates the row first. Output text and
  completion update the row. Without the tool render extension below,
  structured output, annotation and written path are absent.
- Usage becomes the context gauge. The client does not invent per-turn
  token, model or billing data.
- Child-agent events and any other server-filtered variant are absent.
- `_meta` may carry optional Maki data and is never required.
- An update that fails to deserialize, or has an unknown variant, becomes
  a visible notice. It never ends the loop or the TUI.

## Tool render extension (interim bridge)

The main tools (`bash`, `read`, `edit` and the rest) are Lua plugins. Their
rows come from styled snapshots: a header, a body that `read` and others
return as a separate buffer, live buffer changes while the tool runs, and
a structured `ToolOutput`. The TUI prefers the snapshot over the structured
output. Without them a remote row would be plain text for nearly every
tool.

The end state in the broader design is a client renderer chosen by tool
identity from semantic data, with expansion handled on the client. That
needs the UI Lua runtime. Until then the agent sends its snapshots as an
optional, versioned extension in `_meta` on the tool call and its updates.
The standard ACP content stays the fallback.

- **Wire projection.** A snapshot is lines of spans. A span has text and a
  style: default, a theme token name, or an inline style with colors and
  modifiers. The wire form is defined by this extension and is independent
  of the stored `BufferSnapshot` fields. The client resolves tokens with
  its own theme and uses the default style for an unknown token. Decoding
  is added only where the client needs it.
- **Payload.** Optional header, body, structured output, annotation and
  written path. A body update replaces the previous body.
- **Updates.** The server sends a replacement snapshot on a start, on a
  coalesced buffer change and on completion. Coalescing has a rate limit
  so a fast tool does not flood the link.
- **Negotiation.** The client states the extension version in its
  `initialize` metadata. The server sends the extension only to a client
  that asked, at a version both support. Others get standard content.
- **Limits.** The wire has its own finite limits, independent of the model
  result limits and display preferences: lines, spans, serialized bytes
  per snapshot and per update. An oversized extension is dropped with a
  visible notice and the row shows the standard content. The values are
  constants chosen in the implementation specification.
- **Untrusted input.** The client strips unsafe terminal controls and
  escape sequences from every received string before drawing it. Newline
  and tab survive, so listings and diffs keep their layout. Sanitizing is a
  display step and never changes stored data.
- **Interim limits.** A remote row is a static snapshot. Click-to-expand
  and other behavior that runs in the Lua-owned buffer is not available
  until the client renderers exist. The server shows the collapsed form
  that fits the limits. This is a parity debt, listed below.
- **Streaming cost.** Standard streamed output still carries the full
  accumulated text on each flush, as the server does today. A negotiated
  incremental stream for Maki peers is a parity step. Suffixes are never
  sent as standard replacement content.

## Parity ledger

Each row is one observable behavior. The first-milestone column is the
state when `maki connect` first ships. A row turns on only with its
carrier and a test that compares the remote result with the local one.

| Behavior | First milestone | Carrier | Owner |
| --- | --- | --- | --- |
| Tool rows with styled header, body, structured output | snapshot bridge, static | tool render extension | agent produces, client draws |
| Click-to-expand, live tool interaction | off | client renderers from semantic data | client (UI Lua runtime) |
| Incremental tool output | accumulated text | negotiated incremental stream | agent |
| Per-turn usage line, cost, list-price reference, context gauge | on (step 6e) | standard `UsageUpdate` cost, Maki usage extension | agent |
| Conversation grouping, merged-message boundaries | flattened | Maki prompt and replay data | agent |
| Questions (the `question` tool) | on (step 6m) | question extension on `elicitation/create`, the bundled form in the client's own Lua host | agent asks, client draws |
| Plan approval, and editing the plan | on (step 6n) | plan extension: the plan write's completion, `_maki/plan/read`, `_maki/plan/write` at a revision | agent owns the file, client draws and edits a copy |
| A resumed session's plan, and other forms | off | agent-side plan state on load, negotiated form elicitation | agent |
| Child agents, tasks, background work, task history | off | Maki task methods and events | agent |
| Build and plan mode | on (step 6a) | `session/set_mode` before each prompt, mode carried by the prompt | agent confirms |
| Model | on (step 6c) | the model select config option, a remote picker, switch before each prompt | agent confirms |
| Thinking, yolo, fast, workflow | on (step 6f) | prompt settings extension, config options | agent applies, client owns the intent |
| Other modes and `/thinking` | off | config options, UI Lua runtime | agent confirms |
| Permission prompts with the agent's own options | on (step 6b) | `session/request_permission` options, exact option id | agent asks, client draws |
| `/compact` and the automatic compaction's chat line | on (step 6g) | agent command `compact`, unstable `CompactionUpdate` | agent |
| `/btw` | on (step 6h) | btw extension: `_maki/btw` and `_maki/btw/chunk` | agent |
| `/usage`, its ledger and per-model rows in headless session files | on (step 6i) | usage ledger extension on the usage updates, `_maki/usage/quota` | agent |
| Queue edits, and queued messages of the running mode and compactions joining a run at its turn boundaries | on (step 6j) | steer extension: `_maki/steer/poll`, answered under the client's queue lock | client owns the queue, agent asks |
| A queued message of another mode switching the running agent's mode | on (step 6o) | steer mode extension: the steer answer names the mode, the poll gets the run's mode | agent switches, client names it |
| `/login` for a spawned agent | on (step 6k) | ACP terminal auth (`maki acp --login`), login extension `_maki/auth/changed` | agent owns the credentials, client runs its login |
| `/login` for a WebSocket agent, and switching to a newly logged-in provider's default model | off | an agent-side login flow | agent |
| Resume at startup, `/new` in place | on (step 6d) | `session/list`, `session/load` with its replay, `session/new` on the same connection | agent mutates |
| Tabs, `/fork`, load into a running tab, `/cd`, `/sessions`, rewind, export, rename, delete | off | one connection per tab, session methods, Maki methods | agent mutates |
| Plugin-styled bodies of replayed tool rows | structured or plain output | client renderers from persisted semantic output and state; an agent restore bridge only as an interim | client (UI Lua runtime) |
| Lifecycle hooks (turn start, run end, session events) | agent fires its own | host-owned dispatch, exactly once; client observes events and fires no agent hook; replay fires none | agent |
| Drafts, input history, queued input text | on (step 6l) | client store `remote/<agent>/` in the user's state dir: history per workspace, draft and queue per session under a lock | client |
| MCP and subagent control | off | Maki methods | agent |
| Reload, trust, packages | off | split into agent and client scope | each side |
| Completion, `@` files, file picker | off | agent data service | agent data, client UI |
| UI plugins (`/sessions`, `/thinking`, completion) | off | UI Lua runtime, `ui.lua` | client |
| Providers, credentials, session store writes | never on the client | none | agent |

The last row is an ownership rule and stays off by design.

## Permissions

`session/request_permission` becomes `AgentEvent::PermissionRequest`.
The event gains an optional list of the agent's exact options (id, label,
kind). Absent means a local request; an explicitly empty list means a
remote request with nothing to choose, and it never enables the local
choices. The field is additive with a default, so the public serde
contract holds. With options present, the prompt shows the agent's
labels. Kind only chooses shortcuts.

Each ask gets a unique local id mapped to its JSON-RPC id, its offered
options and its run. The reply is a tagged answer with three cases: a local
permission decision, an opaque remote option id and cancellation. The
local encoding is unchanged. The loop validates a remote id against the
pending ask's options. The tool-call id is not the ask identity, because
concurrent asks can share it. A cancelled or ended run replies cancelled to
every pending ask and retires its displayed prompt. A late answer finds no
pending ask and is dropped.

## Sessions

The client initializes the connection and reads the agent's capabilities
before `session/new`. It advertises no filesystem, terminal or
interaction service it does not implement, and it hides status fields the
agent does not publish instead of showing local defaults. A connection
owns one session at a time. At startup it is created with `session/new`,
or loaded with `session/load` when the user resumes one (step 6d); if
that fails, `maki connect` exits with the error. A load draws the replayed
history like a local load and fires no hook. `/new` replaces the session
on the same connection. Tabs, loading into a running tab and `/cd` are
later steps. The loop ignores updates for any other session id.

## Where the code lives

- `maki-acp` gains a stdio client beside `translate.rs`: a typed JSON-RPC
  client over `agent-client-protocol-schema` and the display mapping. The
  server and the client share one place that knows the mapping, including
  the tool render extension.
- `maki-ui` gets the capability value and `RemoteAgentLoop`.
- The binary gets `maki connect`.

Whether `maki-ui` depends on `maki-acp` or the client sits in a smaller
crate is decided in the implementation specification. Local execution
stays in `maki-ui` for now. That is a temporary dependency and not the
final boundary of the broader design.

## Lua and configuration

`maki connect` loads no agent `init.lua` and no agent plugins. The first
milestone runs the TUI with built-in behavior and no user plugins. A UI
plugin host and a `ui.lua` entry point wait for the plugin split.

## Transport

Stdio against a child `maki acp` is the first and only transport, and the
in-repo test peer. A WebSocket link comes later. The client is generalized
over a link when that second consumer exists.

## Connection lifetime

Suspend and `$EDITOR` never disconnect. Event delivery is bounded and a
full buffer pauses the reader with no delivery timeout. The pause happens
only while the UI does not drain. A permission request or prompt response
that arrives meanwhile is handled when the UI returns. Only a closed link
or a framing failure ends the connection.

## Delivery

1. **Typed stdio client in `maki-acp`.** Client, standard display mapping,
   tests against the existing server. No UI change, no new dependency.
2. **Tool render extension.** The server emits and the client reads the
   styled header and structured output under negotiation. Round-trip
   tests, bounds, sanitizing, and a client without the extension.
3. **Capabilities.** The capability value and its checks at the shared
   mutators, with a session built without a model slot or claim. Local
   behavior is unchanged and tests show it.
4. **`RemoteAgentLoop` and `maki connect`.** The loop, permissions with
   exact options, the lifecycle, tests against `maki acp` and user docs.
   This is the first usable, reduced session.
5. **WebSocket link.** One link with dependencies, authentication and the
   workspace boundary. Pantheon-specific behavior stays out of upstream.
6. **Parity steps, each separate.** One change per ledger row or closely
   related group. The ledger order is informational and delivery follows
   dependencies: click-to-expand and the other client renderers ship with
   the UI Lua runtime. Each adds its carrier, turns its rows on and tests
   parity with the local path. The UI Lua runtime
   with `ui.lua` and client renderers come after the plugin split.

Steps 5 and 6 may stay in the fork if upstream declines them.

## Acceptance cases

- The remote loop uses the same `EventLoop`, queue and dispatch as the
  local loop. No second loop exists.
- Local behavior is unchanged after step 3: the existing tests pass and
  every capability is on.
- Against `maki acp` and a scripted standard-only agent: one submission
  draws one user message, a queued message runs after the first,
  `--exit-on-done` exits, and cancel then a new prompt works only after
  the cancelled response arrives.
- Each unavailable feature reports "unavailable" and changes no state,
  including Tab, `/yolo`, `/fast`, `/workflow`, compaction and `/cd`.
- A malformed update, an unknown variant, an already-completed tool call
  and a failed `session/new` behave as defined.
- A tool row from the render extension draws like the local row for the
  same call, including a body and an inline style. A client or agent
  without the extension shows the standard content. Unknown style tokens
  use the default style, unsafe controls are stripped while newline and tab
  survive, and an oversized extension is dropped with a notice.
- A link loss while idle, and cancel then submit then link loss before
  the cancelled response, both settle the UI, return unsent text and
  attachments to the input, reject later submission and let
  `--exit-on-done` finish.
- `/reload`, `/trust`, `/packupdate` and `/packdel` report "unavailable"
  and change no state. Image attachment follows the agent's capabilities.
- A paused UI under sustained output, then a permission request and a
  prompt response, is handled after resume.
- Permission answers use only offered option ids, including arbitrary
  ids and several options of one kind, and keep their labels.
  Concurrent asks with one tool-call id do not cross. A cancelled run
  cancels pending asks and a stale answer is dropped.
- A remote session writes nothing to the local session store.
- Parity is tracked by the ledger: a row turns on only with its carrier
  and a test that compares the remote result with the local one.
- Identical path names on client and agent never cross.

## Non-goals

- A second frontend loop, a trait with one implementation, a remote-window
  API or remote Lua callbacks.
- Reconnection, durable reattachment and a client-side queue separate from
  the existing one. Session replacement is a parity step, not part of the
  first milestone.
- A fork-specific endpoint in upstream code.
