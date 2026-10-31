# Agent and UI split: implementation design

Status: proposed. This document makes the choices that the
[design](tui-over-acp-design.md) leaves to implementation: crate
boundaries, binaries, plugin entry points, the tool data contract and the
order of delivery. It does not repeat the design's ownership rules. Each
stage below gets its own implementation specification, written against
the code when its turn comes and reviewed before its first commit; a
stage specification may refine a choice here when the code demands it,
and says so.

## End state

Three binaries:

| Binary | Package | Contents | Must not link |
| --- | --- | --- | --- |
| `maki` | root `maki` | The terminal client and the agent host in one process, over an in-memory ACP link. Behaves as `maki` does today. | nothing |
| `maki-acp-agent` | `maki-acp-agent` | The agent host: sessions, queues, tasks, persistence, providers, MCP and the agent Lua runtime, serving ACP over stdio. | `maki-ui`, `maki-lua-ui`, crossterm, ratatui |
| `maki-acp-client` | `maki-acp-client` | The terminal client: `maki-ui`, the UI Lua runtime and the ACP client, reaching an agent it spawns over stdio, or one over WebSocket. | `maki-agent`, `maki-providers` request code, `maki-lua` (agent) |

The "must not link" column is a test: a `cargo tree` check per binary
package runs in CI and fails on any edge to a listed crate. Each binary is
its own package, because a `[[bin]]` inside the root package shares the
root's dependency list.

`maki acp` and `maki connect` stay as subcommands of `maki` until the two
packages exist, then become thin aliases of them. The `maki` binary links
both sides and still never shares Lua state, registries or channels
between them except through the in-memory link.

Serving ACP over WebSocket or HTTP from `maki-acp-agent` is not part of
this end state. It needs authentication, authorization and an explicit
workspace boundary before network exposure, and is a separate stage after
the split (a front end such as Pantheon serves WebSocket today).

## Crates

```text
maki-types            data both sides read, and the runtime types of the
                      display path: messages, content, images, tool
                      input/output, agent mode, model data, usage, styled
                      snapshots, SharedBuf, UiWaker, the event vocabulary
                      a display reads (AgentEvent and its payloads)
maki-lua-core         VM thread, request loop, task scopes, watchdog, JIT,
                      module loader, bundled modules, the permission model,
                      pure libraries (json, yaml, base64, text, split,
                      async, treesitter, log, env, uv, jobs, net), and a
                      host binding that each runtime implements
maki-lua (agent)      the agent binding: tools, tool and agent hooks,
                      prompt contributions, providers, interpreter,
                      maki.fs with the file index, maki.agent, agent
                      autocmds, background sessions
maki-lua-ui           the UI binding: buffers, windows, keymaps, commands,
                      actions, input, ui.* slots, forms, tool renderers,
                      the agent ops facade
maki-wire             ACP client, the Maki extensions' wire types, the
                      display mapping, client links (stdio spawn,
                      WebSocket, in memory)
maki-acp (host)       ACP server, the multi-session agent host
maki-agent            the model/tool loop, unchanged in role
maki-ui               terminal client
```

Dependency rules, checked by the `cargo tree` tests:

- `maki-types`, `maki-lua-core` and `maki-wire` depend on neither
  `maki-agent` nor `maki-providers`. `maki-types` may depend on
  `maki-storage`, which has no agent edge, and on small data crates
  (`serde`, `flume`, `arc-swap`).
- `maki-lua-ui` depends on `maki-lua-core`, `maki-types`,
  `maki-highlight`, `maki-markdown` and crossterm, never on `maki-lua`
  (agent).
- `maki-lua` (agent) depends on `maki-lua-core` and never on
  `maki-lua-ui`, crossterm or `maki-highlight`.
- `maki-config` splits its agent and UI parts only where a stage needs it;
  stage 1 records which items each side reads.

### What stage 1 moves

The types a display path reads move to `maki-types`, with a re-export at
each old path (`maki_agent::BufferSnapshot`, `maki_providers::Message`)
so the agent side compiles unchanged. Re-exports alone are not enough
where a type has foreign trait impls or crate-private fields, and the
stage 1 specification resolves each such case from the code:

- a foreign trait impl moves with its type (for example `impl
  TitleSource for Message` goes where `Message` goes);
- a crate-private field that provider code reads (for example the image
  verdict inside `ImageSource`) becomes an accessor owned by the type, or
  stays behind with a provider-side wrapper; the type moved never names a
  provider type;
- `AgentEvent` payloads that hold agent logic rather than data (inboxes,
  cancel maps) are replaced in the moved vocabulary by data handles, and
  the agent keeps the logic behind them.

The public serde of `Message`, `ContentBlock`, `ToolOutput`, `FactsUpdate`
and `PromptFacts` is untouched: a move changes the crate, not the format.

## Lua core and bindings

`maki-lua-core` owns the VM machinery. A runtime is the core plus one
binding, a trait the core calls to install its domain's state and to
serve its domain's requests. The agent binding installs the tool registry
link and `SlotHook`, the provider registration window and the agent
stores; the UI binding installs the UI stores, the UI sender and the
renderer table. The core's request loop serves the neutral `Request`
variants (load, clear, init, autocmd, packages, shutdown) itself and hands
the rest to the binding.

### Compositions

From stage 5 to stage 10 a runtime is built in one of three compositions,
and each compatibility edge has a stage that removes it:

| Composition | Used by | Loads | Removed |
| --- | --- | --- | --- |
| Combined: core, agent binding and UI binding in one VM | local `maki` | each plugin's agent entry, then its UI entry, with one environment and one module cache per plugin, so state shared through a plugin module stays shared; legacy tool presentation fields run as today | stage 10 |
| Agent: core and agent binding | `maki-acp-agent` | agent entries only | never |
| UI: core and UI binding | `maki-acp-client` | UI entries only | never |

Until stage 7, the agent binding also carries the **render bridge**: the
buffer, highlight, markdown and theme-style APIs, `tool_view`, and the
legacy `header`, `restore` and `start` fields, so an unmigrated tool such
as `bash`, whose agent entry requires `tool_view`, runs in
`maki-acp-agent` and is drawn through today's tool render extension. It
has no windows, keymaps, commands or input. Stage 7 removes the bridge
tool by tool: each bundled tool's renderer moves to its `ui.lua`, its
agent entry stops requiring UI libraries, and the bridge goes once no
bundled tool uses it. After that, only presentation that never runs on
the agent is dropped automatically: a third-party tool's unused
`header`, `restore` and `start` fields are ignored with a migration
message, and the tool draws with the default renderer. A call to a
removed API from a handler or an agent entry's top level, such as
`maki.ui.buf` or `require("maki.tool_view")`, fails with an error naming
the call and the migration, as any UI call in an agent entry does.

Stage 5 also splits the entry of each bundled plugin that registers UI
features from its agent entry today: the commands, keymaps, pickers,
panels and windows of `task`, `memory`, `todo_write`, `question` and any
other such plugin move to its `ui.lua`, so the agent composition loads its
agent entry without them. State both halves use today as chunk locals
(for example `todo_write`'s `sessions` and `focused_session`) moves into a
module private to the plugin that both entries `require`. Each entry runs
as its own chunk, so locals never cross, but the combined composition
gives a plugin one module cache, so both entries get the same table and
`maki` keeps its behavior; in separate runtimes each side gets its own
copy and only uses its own half. Their
UI components reach agent data through the agent ops facade from stage 6
on, and the remote operations they need arrive in stage 8; until then,
in `maki-acp-client`, such a feature answers "unavailable".

### Agent-coupled shared code

Two pieces of today's shared code call the agent directly and move behind
the binding: the provider-origin authority (`DeclAuthority`,
`keep_granted`, `release_granted`) and the network egress that
`maki.provider` shares with `maki.net`. The permission model stays in the
core; the authority it grants a provider plugin is the agent binding's.

## Plugin entry points

- **Bundled plugins** have an agent entry, `init.lua`, and an optional UI
  entry, `ui.lua`.
- **External packages** keep their agent entry, the sorted
  `plugin/*.lua` files, and gain an optional UI entry, the sorted
  `ui/*.lua` files. A package with only `ui/` is a UI-only package; one
  with only `plugin/` is agent-only, as every package is today.
- The agent runtime loads only agent entries; the UI runtime loads only UI
  entries.
- **Manifests.** `plugin.toml` keeps its permissions for the agent
  component and gains a `[ui]` table with the UI component's own.
- **Approvals.** An approval names its component: (package, source,
  component). Existing approvals migrate to the agent component only; a
  UI component needs its own approval, so no agent approval ever
  authorizes UI code, and approving one component never replaces the
  other's record.
- **User configuration** splits the same way: `init.lua` configures the
  agent, `ui.lua` configures the client. `maki` loads both, each once,
  into its own runtime.
- **Shared Lua libraries** under `plugins/lib` are split by what they
  touch: pure helpers are bundled into both runtimes, `tool_view`,
  `list_picker`, `toast` and their siblings into the UI runtime only,
  `output_limits` into the agent runtime only.

### Compatibility

Local `maki` keeps today's single VM until stage 10. The split runtimes
serve `maki-acp-agent` and `maki-acp-client` first, so no existing plugin
breaks in `maki` before then, and no shim tries to move callbacks between
VMs: a UI callback can capture agent-side locals, and moving it would
need shared Lua state or remote callbacks, both of which the design
rejects.

Mixed plugins migrate explicitly, by splitting their code into the two
entries. The bundled ones migrate in stages 7 and 8. A third-party
package that calls UI APIs from its agent entry fails to load that call in
`maki-acp-agent` with a message naming the call and the migration, and
keeps working in `maki` until stage 10. Stage 10 is a breaking change for
packages that have not migrated, and needs an upstream decision and a
migration guide before it ships.

## Tool data contract

The agent publishes what a tool did; the UI draws it.

- **Agent side.** A tool registration keeps `handler`, `schema`,
  `audiences`, `permission`, `permission_scopes`, `describe` and the other
  execution fields; permission scopes stay an agent interface. `header`,
  `restore`, `start` and a handler's `body` are presentation and move to
  the UI component. The plain-text summary the agent reads from `header`
  today (the invocation summary, logs) comes from a new execution field,
  `summary(input) -> string`; without one the summary is the tool's name.
- **Persistence.** The host saves each tool call's structured output,
  `state` and data version with the session, as the TUI does today on
  `ToolDone`; `SessionTrack` gains that capture, since the model-facing
  `ContentBlock::ToolResult` does not hold the structured state.
- **On the wire.** A tool call and its updates carry, in a `tool_data`
  extension, the tool name, the raw input, the output as the existing
  `ToolOutput` serde, the tool's `state`, and the data version. Streamed
  output is data (text chunks, progress), never a buffer. The standard
  ACP content stays the readable fallback.
- **Versions.** The tool owns its data version. A renderer registers the
  versions it reads, `maki.ui.register_renderer(tool, { versions = {1},
  header, body, click })`; a call whose version it does not list gets the
  default renderer, never a guess.
- **UI side.** Renderers draw from `(input, output, state, status)` in a
  plugin's `ui.lua`. A missing, failing or mismatched renderer falls back
  to the default one over the standard content. Replay calls the same
  renderer over the persisted data, so a restore never runs agent Lua.
- **Migration.** The current tool render extension (agent-drawn snapshots)
  remains until every bundled tool has a UI renderer; a client that
  negotiates `tool_data` stops receiving it.
- **Subagents and batch.** `batch` and `code_execution` publish their
  child calls as data; their renderers compose the children's renderers
  on the UI side, replacing `get_tool`'s export of header functions.

## Agent operations from UI Lua

`maki.session`, `maki.model`, `maki.task` and `maki.plan` keep their Lua
signatures in the UI runtime and are served by an `AgentOps` facade
implemented over the ACP client, the same for an in-memory and a remote
agent. Each call names its session or task; focus stays a UI-local fact.
A call the agent does not support answers "unavailable". UI plugin
permissions decide which calls a plugin may make; the agent checks its
own policy on every operation.

## Queue and persistence

- **Accepted queue items move to the host.** Today the client owns the
  queue and the agent takes runs from it (step 6j's steer poll). The host
  takes ownership before stage 10: an enqueue is a submission the host
  accepts, gives an identity and persists, and the host decides when it
  runs, joins a run or is dropped, with a terminal outcome the client
  sees. Steering becomes the host's own decision over its queue, and the
  steer poll is retired. The client keeps only unsent input.
- **Sessions.** The host is the only writer of sessions, for local tabs as
  for remote ones, through `SessionTrack`.
- **Client store.** The store from step 6l (`maki_storage::remote_drafts`)
  keeps drafts, their writers and input history for every tab, keyed by
  agent identity and session.
- **Migration.** When the split `maki` first opens an existing session,
  `input_draft` and `input_draft_writers` move from its metadata into the
  client store, and `queued_messages` become accepted host queue items,
  since a local run would have taken them; nothing unsent is turned into
  an accepted item, and nothing accepted is left as a draft.

## Stages

Each stage ships on its own: the workspace builds, `just ci` passes, and
`maki` behaves as before unless the stage says otherwise. A stage's
implementation specification is written and reviewed when it starts.

| Stage | Delivers | Proof |
| --- | --- | --- |
| 1. Shared types | `maki-types` with the data, buffer and event types a display path reads; re-exports at the old paths | `maki-types` has no edge to `maki-agent` or `maki-providers` |
| 2. Links | `serve` over any link; an in-memory link pair; `maki acp` keeps stdio | protocol cases pass over stdio and in memory |
| 3. Client crate | `maki-wire` split from `maki-acp`: client, extension wire types, display mapping, client links | `maki-wire` has no agent edge; `maki connect` uses it |
| 4. Binaries | `maki-acp-agent` and `maki-acp-client` packages wrapping today's `maki acp` and `maki connect` | both run; the dependency tests exist and list the edges still to cut |
| 5. Lua core | `maki-lua-core`, the binding trait and the three compositions; `maki` runs the combined one, `maki acp` the agent one with the render bridge; bundled plugins' UI registrations move to their `ui.lua` | `maki-lua-core` has no agent edge; every bundled agent entry loads in the agent composition; every Lua test passes; `maki` unchanged |
| 6. UI runtime | `maki-lua-ui`; UI entry points, the `[ui]` manifest table and component approvals; `maki-acp-client` runs the UI runtime and the bundled UI-only plugins (`thinking`, `sessions` panels over what the agent already serves) | a client with UI plugins and one with none both work against `maki-acp-agent` |
| 7. Tool data | `summary`, host capture of tool data, the `tool_data` extension with versions, UI renderers for every bundled tool, replay from data | the render extension is off for a client that negotiates `tool_data`; a failing or mismatched renderer draws the default |
| 8. Host features | per feature, the host operation and then its UI component: the host queue; sessions by id (list, fork, rewind, delete, rename, export, `/cd`); tasks and subagent events; MCP status and control; memory, todo and completion data services; plugin administration per domain (`/reload` of the client or the agent runtime, `/trust` where the code runs, `/packupdate` and `/packdel` per component); the mixed plugins `todo_write`, `memory`, `task`, `completion` split over them | each parity ledger row it covers is on, over `maki-acp-client` as in `maki` |
| 9. WebSocket serving | optional: `maki-acp-agent` serves WebSocket with authentication, authorization and a workspace boundary | the design's remote adapter rules |
| 10. One `maki` | `maki` runs `maki-ui` against the host over the in-memory link; `maki-ui` drops its in-process agent loop and its `maki-agent` and `maki-providers` dependencies; the combined composition goes | the `maki-acp-client` dependency test lists no edge left; `maki` passes its existing tests; the stored-session migration runs |

Stages 1 to 4 are mechanical and low risk, and make the three binaries
exist early. Stages 5 and 6 are the Lua split itself. Stage 7 and the
steps of stage 8 close the parity ledger, each row with its own step as on
this branch so far; within stage 8 a host operation always ships before
the UI component that needs it. Stage 10 removes the old path, which the
design rejects as a long-lived second execution path, and needs all of
stage 8; stage 9 is independent of it.

## Risks

- **Stage 5 touches the 7,000-line `runtime.rs`.** The split follows the
  `Request` variants' domains (neutral, agent, UI) recorded in the survey
  and the binding boundary above; task scopes keep their presentation
  fields in the UI binding, and in the agent binding's render bridge
  until stage 7.
- **Stage 1's reach.** The event vocabulary is large; its specification
  lists every moved type and every payload replaced by a data handle
  before the first commit.
- **Process globals.** `ToolRegistry::global`, the provider plugin
  registry, `LIVE_SESSIONS` and `LIVE_HISTORIES` belong to the agent side;
  in the one-process `maki`, only the agent host touches them, which
  stage 10 guarantees by removing the UI's `maki-agent` dependency.
- **Upstream.** This is fork work. Stages 1 to 3 are the most likely to
  be accepted upstream on their own, since they change no behavior.

## Revision: one path, no parity proofs

After stage 7b reached its sixth tool, the branch stood at about 43,000
net new lines (moved lines excluded). Two rules of this plan caused most
of it: every stage kept the old in-process path and the new ACP path
alive at once, and every tool migration proved exact parity against a
frozen copy of its legacy drawing. Both rules are dropped. The ownership
rule stays: the agent links no UI or render code, and the client links
no agent, provider or agent-Lua code, with no allowance left in the
dependency tests.

What changed against the stages above:

- Stage 10 came forward. `maki` runs `maki-ui` against the host over the
  in-memory link, the in-process driver and the combined composition are
  gone, and the host serves the operations they covered (queues,
  sessions, rewind, tasks, MCP, `/cd`, reload and plugin administration,
  the last over one `_maki/plugins` request).
- The render bridge, `maki-lua-render`, the `toolRender` extension and
  renderer version routing are gone. Every bundled tool draws on the UI
  side through its renderer or the default; a stored call without tool
  data draws its text.
- The parity harness and the saved legacy plugin copies are gone; tests
  cover behavior, wire and serde contracts, regressions, races and the
  dependency boundary.
- Each Lua VM loads one component with one binding and its own plugin
  configuration. An unknown plugin option key now fails instead of being
  ignored.
- `maki-acp-client` keeps generic ACP support; Maki extensions add to
  it.

The branch ends at about 20,000 net new lines.
