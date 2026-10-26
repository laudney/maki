# TUI over ACP: design

Status: proposed architecture. This document replaces the former single-VM
and remote-window design. It specifies ownership and observable behavior.
It does not specify implementation units, commits, Rust signatures or a
complete wire schema.

The [survey](tui-over-acp-survey/README.md) describes the existing code.
The [source index](tui-over-acp-implementation.md) locates the relevant
implementations. Neither document defines additional architecture rules.

## Goal

`maki-agent` is a pure agent loop. A host exposes its sessions and operations
through ACP. `maki-ui` is an ACP client with its own Lua runtime. Agent tools
and agent policy run in a separate Lua runtime on the agent side.

The same boundary applies over stdio, HTTP, WebSocket and an in-memory link.
Transport selection must not change tool behavior, session ownership or the
plugin APIs. This design does not claim that these transports already exist.

The separation must support a remote agent, a local agent and a client with
no UI plugins. An agent must run without a Maki UI runtime.

## Component boundaries

```text
maki-ui
  terminal UI and client state
  UI Lua runtime
  ACP client
       |
       | ACP operations, requests and events
       | stdio / HTTP / WebSocket / in-memory
       |
  ACP server
Agent host
  sessions, queues, tasks, persistence, configuration and permissions
  agent Lua runtime, tool registry, providers and MCP services
       |
  maki-agent
    model/tool loop, conversation state and core hook interfaces
```

The agent host is a logical component. Its final crate name and the location
of shared protocol types remain implementation choices.

| Component | Owns | Boundary |
| --- | --- | --- |
| `maki-agent` | Model/tool iteration, request-frame rules, core cancellation and hook interfaces | Accepts tools and services through explicit interfaces. It has no Lua VM, ACP transport, terminal or frontend dependency |
| Agent host | Session lifecycle, input queues, task ownership, models, credentials, MCP, authoritative state and saves | Calls the loop and supplies tools, policy and interaction services |
| ACP adapter | Capabilities, protocol validation, request routing and event translation | Exposes the host without defining a second session driver |
| Agent Lua runtime | Tool handlers, agent hooks, prompt contributions, provider extensions and agent background work | Uses host services. It has no windows, keymaps, focus or input buffer |
| `maki-ui` | Terminal lifecycle, tabs, focus, drafts, selection, scrolling, themes and client preferences | Reads published agent state and sends ACP operations |
| UI Lua runtime | Commands, keymaps, completion, forms, pickers, tool renderers and presentation hooks | Uses local UI APIs and a permission-checked ACP client facade |

One launcher can compose both sides in a process. This does not make agent
objects, Lua registries, channels or mutable session state available to the
client. Internal tool calls and subagent execution can use core interfaces
within the agent host. They do not need an ACP round trip.

Print and SDK frontends use the same host semantics. A frontend adapter may
preserve an existing external format, but it must not introduce another
implementation of session scheduling or agent policy.

## Two Lua runtimes

### Isolation

The agent and UI have separate Lua states, API registrations, hook stores,
permissions and lifecycles. A UI client owns its UI runtime. The agent host
owns its agent runtime and scopes work to the relevant session and task.
Neither runtime shares Lua objects or callbacks with the other.

Runtime machinery and general libraries can be shared as source or Rust
code. Shared support must not give the UI a transitive dependency on
`maki-agent`, its tool registry or provider execution. Each host registers
only the APIs that belong to its domain. The exact crate split is deferred.

A slot chain is local to one runtime. A cross-domain operation needs a typed
request or event with defined permissions, lifetime and failure behavior.
There is no generic remote Lua evaluation or callback invocation API.

### Plugin placement

A plugin belongs to the side whose behavior it controls. An existing package
can supply an agent component and a UI component. Each component loads only
in its own runtime and declares its own requirements. Installing one must
not silently authorize or execute the other.

| Existing plugin or group | Agent component | UI component |
| --- | --- | --- |
| `bash`, `read`, `write`, `edit`, `list`, `grep`, `glob`, `index`, `webfetch`, `websearch`, `view_image`, `code_execution`, `batch` | Tool schema, execution, tool hooks and structured results | Optional headers, result renderers, expansion and click behavior |
| `task` | Delegation, validation, concurrency, background policy and child lifecycle | `/tasks`, keymaps, picker, task chats and presentation |
| `question` | Validate questions, request user input, validate answers and construct the tool result | Form state, keyboard/mouse handling and answer submission |
| `todo_write` | Validate and publish the task-scoped todo data and its lifecycle | Panel, toggle key, status hint and result rendering |
| `memory` | Memory tool, storage, prompt contributions and permission rules | `/memory`, picker, editing interface and result rendering |
| `skill` | Skill discovery, content loading and prompt contributions | Optional result rendering or browsing UI |
| `completion` | Data services needed for the agent workspace | Trigger handling, source slots, ranking, popup and input edits |
| `thinking` | Model capabilities and requested setting changes | `/thinking`, keymap and picker |
| `sessions` | Session list/read/create/delete/rename operations | `/sessions`, `/rename`, keymaps and focus |
| Provider plugins | Provider declarations, credentials, discovery and requests | Optional selection UI over published model data |
| Shared Lua libraries | Domain-independent helpers or agent helpers | Domain-independent helpers or UI helpers |

These are responsibility assignments, not a requirement to add a UI
component to every tool. A default renderer is always available.

Mixed plugins must be divided explicitly. Their components exchange results,
state or operations through ACP. They cannot depend on shared upvalues,
module caches, coroutine state or coincident local filesystem paths.

### Hooks and authority

| Agent runtime | UI runtime |
| --- | --- |
| Incoming-message, stopping and compaction policy | Input changes, completion and local submission commands |
| Tool input/output interception and permission policy | Tool display, clicks and expansion |
| Prompt construction and provider extensions | Window, key, paste, resize and theme events |
| Authoritative session, run and task lifecycle | Client attach/detach, tab/session/task focus and local reload |

The agent host emits authoritative lifecycle hooks independently of any
client. The UI can observe corresponding ACP events for notifications or
panels. Such observations cannot replace agent policy or change a recorded
result. Replay is marked so loading history does not rerun live actions.

Session closure on the agent and closure of a client tab are different
lifecycle events. A focus change has no effect on tool routing or agent
permissions. UI operations name their target session, task or call.

The agent enforces permissions for every received operation. UI plugin
permissions control local access and which ACP operations the plugin may
request. A UI grant cannot bypass agent policy. Agent-side permission hooks
run before an unresolved permission request reaches the client. UI hooks
control how that request is presented.

## Data and presentation

### Tool results

Agent tool execution produces model-facing output, error status and optional
structured data. Streaming output contains data such as stdout, stderr,
progress, diffs or child-call results. It does not contain a Lua buffer,
terminal geometry, theme colors or window commands.

The UI selects a local renderer from the tool identity and supported data
version. Renderers receive the published input, output, status and permitted
state. They can build local buffers and handle local clicks. Read or action
requests triggered by a renderer still use ACP and agent permission checks.

A result must remain usable without the matching renderer. Standard ACP
content and readable fallback output preserve meaning for other clients and
for an absent, failed or incompatible UI plugin. Optional structured data
has an explicit compatibility rule. It must not be the only place that
contains information needed by the model.

The host persists the semantic data needed for replay. The UI reconstructs
its display from that data. Changing a theme, expanding a result or loading
history does not execute a tool again or ask agent Lua to draw a buffer.

This changes the existing tool API where `handler`, `header`, `restore` and
`body` belong to one registration. UI-only state such as expansion and scroll
position stays client-side. Tool-owned state required to interpret a result
must be serializable, versioned and available on replay.

Display line limits belong to the UI renderer. Agent limits on model-facing
tool output remain tool policy. Changing a display preference cannot change
what the model receives.

### Interactive tools

`question` uses an interaction service supplied by the host. The request
contains semantic questions and answer constraints. The UI renders its own
form and returns data. Standard ACP elicitation is used where it preserves
the required behavior. Any additional behavior needs a negotiated extension.

Requests have an owner and a terminal outcome: answered, dismissed,
cancelled, unavailable or failed. Late replies cannot answer a newer request.
A disconnected or incapable client must not leave a tool waiting forever.
The host reports unavailable interaction or uses an explicitly configured
headless policy. It never treats missing interaction as approval.

Plan approval and permission prompts follow the same separation of policy
from presentation. The agent owns plan content and execution decisions. UI
plugins own forms, rows and local navigation.

### UI plugins that need agent data

`thinking`, `sessions` and `/tasks` obtain capabilities and state through the
ACP client facade. Accepted changes are confirmed by the host. The UI does
not persist an assumed model, task status or session title as agent truth.

Completion handles typing, navigation and input edits locally. A completion
source for a remote workspace obtains data through an agent data service.
Responses carry enough request context to discard stale results. Local
sources remain local. No per-keystroke RPC is required for window handling
or reading the UI input buffer.

`/memory` accesses memory on the agent through defined operations. It must
not interpret an agent path as a file on the UI machine. Provider credentials,
skill discovery, tool execution and agent environment resolution stay on
the agent side.

## ACP and transport

### Contract

ACP is the frontend boundary. Use standard operations and content where they
represent the required behavior. Add negotiated Maki extensions for concrete
gaps such as queue control, background tasks or richer result data. The
[survey of the current adapter](tui-over-acp-survey/04-acp-server.md) records
existing coverage without freezing new method names.

Extensions describe agent concepts. They must not expose `UiAction`,
`AgentEvent`, `EventHandle`, `RegistryKey`, `SharedBuf` or other internal
objects as a catch-all protocol. Lua windows, keymaps, input reads and theme
updates are local UI operations and need no ACP extensions.

Clients negotiate supported operations and data versions. The server
advertises only usable capabilities. Unsupported requests fail explicitly.
A client without Maki extensions still receives valid standard ACP content.
The UI can connect to another ACP agent with a reduced feature set.

The contract must cover these behavior groups:

| Group | Required behavior |
| --- | --- |
| Conversation | Text, thinking and images, stable display grouping, history and replay |
| Sessions | Create/load, settings, title, list, close/delete, fork and rewind where supported |
| Runs and queue | Submit, enqueue, interrupt, compact, cancel, accepted-item identity and terminal outcomes |
| Tools | Start, progress, streamed data, completion, errors and semantic result data |
| Tasks | Parent identity, status, background promotion, child messages, cancellation and retained results |
| Interaction | Permission, questions and authentication requests with owner-scoped replies |
| Agent services | Models, usage, plans, MCP state, remote workspace data and agent plugin administration |

This table defines capabilities to preserve or negotiate. It does not claim
that each row has a standard ACP carrier or that every client must implement
it. The [gap inventory](tui-over-acp-survey/08-gap-analysis.md) maps features
to owners.

### Transports

Stdio, HTTP, WebSocket and in-memory links implement the same logical
request, notification, reply and cancellation semantics. Stdio already
exists. The other adapters can be delivered separately without changing the
architecture or the two plugin domains.

An HTTP adapter must provide streamed agent updates and a way to complete
agent-to-client requests. A single blocking prompt response is insufficient.
Remote adapters require authentication, authorization and an explicit
workspace boundary before network exposure. ACP schema negotiation alone
does not provide these protections.

An in-memory adapter may avoid text framing. It still validates the same
data contract and preserves identity, ordering, cancellation and errors. It
cannot pass live handles or provide a more capable private UI path.

The client must keep processing requests and notifications while an earlier
request is pending. The server must remain able to receive cancellation and
interaction replies while a tool waits. Neither side holds an owning-state
lock across a peer request. Output buffering is bounded and streaming avoids
resending the full accumulated output on every update.

## State and lifecycle

### Ownership

The agent host is the only writer of authoritative conversation history,
accepted queue items, task results, tool data, usage, settings and permission
policy. It continues to maintain this state without a connected UI.

The UI owns drafts, local input history, tabs, focus, themes, scroll state,
selection, picker state and renderer preferences. Client state is stored
separately and keyed by agent identity and session. A draft becomes agent
state only when the host accepts its submission. Existing stored drafts and
queued messages need distinct migration treatment.

Sessions and tasks have explicit identities and generations. An accepted
queue item has an identity before it reaches history. A hook can reject or
rewrite it, so live user display identity must not be guessed from a future
history index. Text and images belonging to one displayed message retain
one grouping on live delivery and replay.

The host defines semantic event order, validates stale actions and persists
required replay data. The UI uses the published identities to reject stale
updates and retains display state only for the represented generation.

### Cancellation and failures

Foreground cancellation, child cancellation, session close and client
detachment have separate meanings. Cancelling a foreground run preserves
background work that the host still owns. Rewind, replacement and deletion
end work owned by the replaced generation and retire its interaction asks.
A cancelled ask cannot record a later reply as a new permission decision.

The connection contract declares whether disconnect stops a foreground run
or leaves it active. Explicit session closure ends its owned work. A local
launcher can tie the lifetime of its private agent process to the UI process.
These policies cannot depend on whether the link happens to be in memory.

Transport loss makes pending operations uncertain until reconciled. The UI
marks the connection state and does not automatically resubmit a prompt or
repeat a tool action. Reconnection loads an authoritative snapshot and a
defined subsequent event stream. Durable reattachment is a separate
capability and is not implied by using HTTP or WebSocket.

A renderer or UI plugin failure falls back to the default display and does
not rerun the agent. Agent plugin failures follow the relevant tool or hook
error policy. Runtime reload cancels that runtime's owned callbacks. A UI
reload does not reload agent tools. An agent reload ends or replaces its
owned work through the host and preserves saved session data.

### Files, configuration and compatibility

Every operation resolves paths and credentials in its owning environment.
The agent carries explicit workspace context through tool, hook, provider
and plugin operations. The UI carries explicit local context. A remote
agent must not depend on the UI process cwd or environment. The scope of a
`/cd` action is explicit in the request.

Agent configuration and UI configuration have separate entry points and
permission domains. A combined launcher can select both, but it must not
execute the same mixed `init.lua` twice. Project trust is evaluated where
code runs. Trust on the UI machine does not authorize agent-side packages,
and an agent cannot install executable UI plugins by advertising a tool.

The two components of a package can use a shared release, but the data
contract must handle a missing or different-version peer. Package resolution
and lockfiles remain owned by the existing package mechanism in each
environment. The entry-point and configuration migration syntax is deferred.

Provider requests remain append-only within a frame. Tool or prompt changes
that require a new frame use the existing frame rules. Separating plugins
does not permit rebuilding earlier requests or changing thinking prefixes.
Existing session and export serde remains public API. Required migrations
must preserve older data and follow the log-version rules.

## Acceptance criteria

- A tool runs through the agent host with no `maki-ui` or UI Lua runtime.
- `maki-ui` and its Lua host have no direct or transitive agent-loop or
  provider-execution dependency.
- Completion, keymaps, windows, themes and tool presentation run in UI Lua.
- Agent hooks fire without the UI and UI observations cannot fire them again.
- Mixed plugins use data and operations. A missing UI component has default
  presentation, and missing agent operations are reported as unavailable.
- Tool streams and replay work with a default renderer and with a matching
  UI renderer. Renderer failure has no execution side effects.
- Question and permission requests settle on answer, cancel, disconnect and
  unsupported capability without accepting stale replies.
- Local and remote clients resolve paths, packages, configuration and
  credentials in the correct environment.
- Equivalent protocol cases have the same results over stdio and in-memory
  links. HTTP and WebSocket adapters must meet that contract when added.
- Queue, task, cancellation, persistence and restore behavior remains covered
  by focused cases, including detached work and generation changes.

The validation scope follows the changed boundary. Documentation changes
need document checks. Implementation checks are selected when source changes
are made. This document does not prescribe repeated workspace runs, review
round counts or a fixed PR sequence.

## Decisions left for implementation design

- Exact host/shared-runtime crate boundaries and shared wire types.
- Manifest entry points, separate configuration files and migration support
  for existing mixed third-party plugins.
- Result-data schemas, version negotiation and the smallest required ACP
  extensions, checked against the schema selected at implementation time.
- Remote transport framing, authentication, connection policy and rollout.

These choices must preserve the ownership rules above. The rejected
single-VM window-remoting architecture is not a compatibility fallback.
