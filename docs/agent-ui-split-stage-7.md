# Agent and UI split: stage 7, tool data

Status: proposed. Stage 7 of the [implementation
design](agent-ui-split-implementation.md): the agent publishes what a tool
did as data, and the UI draws it. This specification covers the
infrastructure, stage 7a; the migration of every bundled tool to a UI
renderer and the removal of the render bridge from the agent runtime are
stage 7b, with their own specification. `maki` draws every tool as today
after each commit here.

## Refinements

- **Two parts.** The stage row bundles the data path with eighteen tool
  migrations. The data path ships first and changes no drawing; each
  migration then moves one tool's drawing to its renderer and can be
  checked on its own.
- **The bridge stays in `maki`.** The project owner decided (2026-10-30)
  that third-party legacy presentation (`header`, `restore`, `start`,
  handler bodies, `maki.tool_view` in an agent entry) keeps working in
  `maki` until stage 10, as the compatibility rule promises. Stage 7b moves
  the bridge into a crate that only the combined composition uses; the
  agent runtime then ignores those fields with a migration message and
  draws such a tool with the default renderer. This specification does not
  touch the bridge.
- **The summary is stored.** Replay titles are read synchronously today
  (`maki-agent/src/tools/registry.rs:479-486`, `resolve_header`), which
  cannot ask Lua; the host stores each call's summary with its data when
  the call runs, so a replay reads it.
- **The client asks for `toolData` last.** The parent design keeps the
  render extension until every bundled tool has a UI renderer, and a
  client that agrees `tool_data` stops receiving it. So in 7a only test
  clients agree `tool_data`; `maki-acp-client` and `maki connect` ask for
  it in the last commit of 7b, and draw through `toolRender` until then.
- **Left for 7b.** The bridge crate, dropping `maki-lua-render`,
  `maki-highlight` and `crossterm` from the agent crate (stage 5 and
  stage 6 deferred them here), the provider login's raw-mode read
  (`maki-lua/src/api/provider.rs:573-577`), the question form, and the
  per-tool renderers.

## Today

- A tool's agent-facing title is the text of its `header` (`HeaderResult`,
  `registry.rs:124-155`), computed by a round trip to the VM
  (`tool_dispatch.rs:686-701`); replay titles fall back to the tool name.
- `state` survives only inside `TextOutput`, for the plain, markdown and
  `ReadDir` variants (`maki-types/src/tool.rs:148-151`, `:338-343`), and is
  lost for
  diffs, images and every error (`maki-lua/src/api/tool.rs:538-559`,
  `maki-agent/src/agent/tool_dispatch.rs:650-658`). There is no data
  version.
- The host never saves tool outputs: only the TUI writes
  `Session.tool_outputs` on `ToolDone` (`maki-ui/src/app/mod.rs:2462-2465`),
  and `SessionTrack::record_turn` saves messages only
  (`maki-agent/src/session.rs:454-468`). A `maki acp` session therefore
  replays with no outputs.
- Live output is an agent `SharedBuf` that the TUI and the ACP host poll
  (`maki-acp/src/live.rs:28-105`); no agent code emits structured progress.
- On the wire, the render extension (`_meta.maki.toolRender`,
  `maki-wire/src/render.rs`) carries agent-drawn header and body snapshots
  and the final `ToolOutput`.
- Children of `batch` reach Lua as text only
  (`maki-lua/src/api/agent.rs:644`), and `batch` draws them through
  `get_tool`'s header and restore export (`api/tool.rs:1084-1102`).
- `dir_listing` and `partial` mix agent work and drawing
  (`plugins/lib/maki/dir_listing.lua`, `partial.lua:17-37`).

## Design

### Summary

- **Field.** `register_tool` gains `summary(input) -> string`, the
  plain-text summary the agent reads (invocation title, logs). It runs on
  the agent. Without it, the summary is today's header text while the
  bridge runs the header on the agent, and the tool's name in the agent
  runtime once 7b removes the bridge there, as the parent design says.
- **Readers.** `ToolStartEvent.summary` takes it, as it takes the header
  text today. The host stores it with the call's data (below), and the
  replay title and the TUI's restored row text read the stored summary,
  falling back to today's `resolve_header` for a session without data.
- **Bundled tools** keep today's titles through the header fallback in
  7a; each gets its own `summary`, equal to today's header text, in its 7b
  migration, when its header leaves the agent, so no header logic exists
  twice. A replayed title becomes the stored summary where it was the bare
  name before, which the CLI page notes.

### Tool data

- **Type.** `maki_types::tool::ToolData`: the tool name, its data
  `version`, the effective `input`, the `summary`, the `annotations`, the
  `state` (any JSON value), and the `children` (below). `annotations` is
  every annotation the call got, in order: the start one, live ones, and
  the done one. The TUI appends a done annotation to the start one today,
  without duplicates (`maki-ui/src/components/messages/mod.rs:336`,
  `tool_display.rs:758`), and `batch` keeps successive ones
  (`plugins/batch/init.lua:437`); the list keeps the facts, and one shared
  function joins them for display, the same live, remote and on replay.
  The input is the one the call ran with after input hooks
  (`tool_dispatch.rs:139`, kept in `CallRecord` at `:191`), so a replay
  shows what ran, not the model's `ToolUse.input`
  (`maki-acp/src/translate.rs:469`), and no earlier message changes. It
  sits beside `ToolOutput`, which is unchanged:
  `TextOutput.state` keeps being written, so a session read by an older
  build restores as today.
- **Version.** `register_tool` gains `data_version` (a positive integer,
  default `1`); the tool owns it and bumps it when its `state` shape
  changes.
- **State for every result.** A handler's `state` reaches `ToolData` for
  every output variant and for errors, which today drop it.
- **Carried.** `ToolExecResult` and `ToolDoneEvent` carry the `ToolData`;
  `ToolStartEvent` carries the data version beside the tool name it has
  (`maki-types/src/event.rs:28-37`), so a renderer can be chosen before
  the result exists. `ToolDoneEvent` is serialized (`event.rs:39`), so the
  new field is skipped when absent and the headless JSON is unchanged for
  a call without data.
- **Every call.** A Lua tool always has data; an MCP or native tool has
  version 1 and a null `state`, so no renderer claims it by accident and
  it draws with the default.

### Children

- **Identity.** Nested dispatch today has an empty id and clears the
  parent's tool id (`maki-lua/src/api/agent.rs:1220`,
  `api/util/ctx.rs:74`). `call_tool` instead gives each child a path: the
  parent's tool use id and the child's invocation index under its parent,
  so a grandchild (a `batch` child that runs `code_execution`, which calls
  tools) is the child's path plus its own index. The context carries the
  path; a call outside a tool handler (a command, a hook) has none and
  records nothing.
- **Record.** Each child's record, in its parent's `children` in
  invocation order, holds the child's tool, input, `ToolData`, output,
  status (running, done, error, cancelled), annotations and usage. A
  cancelled parent marks its running children cancelled.
- **Live.** A child's start, progress and completion are `ToolProgress`
  events of the top-level call, tagged with the child's path (below), so
  a renderer can draw running children as `batch` does today through its
  live buffer, annotation and usage callbacks
  (`plugins/batch/init.lua:471`). The live annotation and usage events a
  child gets today (`maki-lua/src/api/agent.rs:1184`, `:289`) update its
  record while it runs, so nothing `batch` shows now is lost.
- **Kept.** The host publishes and persists the children with the
  top-level call. `batch` keeps its own `state` for what is its own and
  stops copying its children's text into it in stage 7b;
  `code_execution`'s children are recorded too, though its renderer may
  ignore them.

### Capture and persistence

- **Capture.** `process_tool_calls` (`tool_dispatch.rs:1185-1191`), where
  every `ToolDoneEvent` of a turn meets the results message, records each
  call's `ToolOutput` and `ToolData` in `History`, beside the messages and
  the live frame it already holds (`maki-agent/src/agent/history.rs:83`),
  and marks it unsaved. A child call is captured inside its parent's
  `ToolData`, never as its own entry.
- **Session.** The data rides in the existing `out` record
  (`LogRecord::Out`, `maki-storage/src/sessions.rs:763`) as an optional
  `data` field beside `d`, and `Session` keeps it with the output under
  the same id (`:448`). So the append writer (`:1054`), the full rewrite
  (`:1251`), the loader (`:1381`), `insert_tool_output`'s rewrite on a
  changed id (`:2033`), fork, and both prunes (`:2100`, `:2178`) carry or
  drop the data with its output, with no cursor of its own.
- **Writers.** `SessionTrack::record_turn` (`maki-agent/src/session.rs:
  454`), the headless and ACP writer, copies the outputs and data from
  `History` before its `save` (a full rewrite, `sessions.rs:2187`), so a
  `maki acp` session saves outputs for the first time. The TUI keeps its
  own writer and stores the data from `ToolDoneEvent` where it stores the
  output today (`maki-ui/src/app/mod.rs:2462-2465`).
- **Compatibility.** An older reader ignores the unknown field, since no
  record denies unknown fields, so this needs no `LOG_FORMAT_VERSION`
  bump; an older build that rewrites the file drops the data, and the
  session then replays as today.

### Stream

- **API.** A handler streams with `ctx:stream(text)`, which appends text
  to the call's output data, and `ctx:set_state(value)`, which replaces
  the call's live `state`. Both run in the agent runtime and need no UI.
- **Event.** A new `AgentEvent::ToolProgress { id, path, append, state,
  child }` carries them; `path` is empty for the top-level call and names
  a child otherwise, and `child` carries the child's whole record (tool,
  data version, summary, input, status, annotations, usage, and at
  completion its output and data) each time it changes: at start, on an
  annotation or usage update, and at completion. A nested call sends no
  `ToolStart` of its own (`maki-lua/src/api/agent.rs:1220`,
  `tool_dispatch.rs:697`), so the start record is what lets a parent's
  renderer pick the child's renderer by version while it runs. Text and
  state of a running child travel in `append` and `state` under its path.
- **Schedule.** The tool dispatcher owns the coalescing, not the ACP
  host's live-buffer timer (`maki-acp/src/live.rs:72`), which runs only
  while a live buffer exists. Per call, it sends pending text and the
  latest state at most once per render tick, and flushes them before the
  call's `ToolDone`, so a call that ends before its first tick loses
  nothing, an error or a cancel included. One tick sends one event per
  path, with the text since the last tick and the latest state and child
  record, so a child that starts and ends inside one tick sends only its
  final record; each record is the child's whole state, so nothing a
  renderer draws is lost.
- **Final.** The handler's returned `state` and output are authoritative:
  `ToolDone` replaces the live state and the streamed text. Streamed text
  is live only and is never persisted; a tool that wants its text kept
  returns it in its output.
- **Drawn by nothing yet.** The TUI and the render extension ignore the
  event, so `maki` is unchanged; a handler that streams keeps drawing its
  live buffer as today until its stage 7b migration.

### Wire

- **Extension.** A new `tool_data` extension, agreed as `toolData: 1`,
  puts the data in `_meta.maki.toolData` of `tool_call` and
  `tool_call_update`: `tool`, `version`, `input` (the effective input),
  and, as they occur, `summary`, `annotation` (one new annotation, added
  to the list), `append`, `state`, `path`, `child`, `output` (the
  `ToolOutput` serde), `children` and `status`. The annotations travel
  only in `toolRender` today (`maki-acp/src/server.rs:1934`, `:1952`; read
  at `maki-wire/src/display.rs:191`, `:223`), so the data carries them
  too. Limits and oversize handling follow the render extension's
  (`maki-wire/src/render.rs:20-24`, `:130-145`), checked on the received
  bytes. Standard ACP content stays the readable fallback.
- **Raw on receipt.** The client sanitizes every string of a notification
  before it decodes it (`maki-wire/src/client.rs:1325`,
  `render.rs:228-239`), which strips carriage returns and escapes. It
  takes `_meta.maki.toolData` out first and decodes it unchanged, so a
  remote renderer gets the same input and state as a local one (an edit's
  CRLF text included). Text reaches the screen sanitized at the display
  boundary instead: every line a renderer or the default renderer draws
  passes `sanitize_display` when its buffer is blitted, locally and
  remotely.
- **Agreement.** The server agrees `tool_data` when a client asks for it,
  and then does not send `toolRender` to that client. A client asks for
  `toolData` only from the commit that can draw it.
- **Replay.** A loaded session replays its persisted data under
  `tool_data`, and its outputs under `toolRender` for an older client, as
  today.
- **Client.** `DisplayMapper` decodes the extension into `ToolStartEvent`,
  `ToolProgress` and `ToolDoneEvent` with their data, so a remote tab gets
  the same events as a local one.

### UI renderers

- **Registration.** In a plugin's `ui.lua` (or a package's `ui/` entry),
  `maki.ui.register_renderer(tool, { versions = {1}, header, body, click })`
  registers a renderer; records are kept per plugin and cleared on reload.
  `header(data, ctx)` returns a string or a buffer; `body(data, ctx)`
  returns a buffer or nothing; `click(data, ctx, row)` handles a click.
  `data` is `{ tool, version, input, output, state, status, summary,
  annotations, children }`.
- **Context.** The render context carries the session's working directory
  and the agent's home (for paths), the UI's `tool_output_lines` and
  width, and the call's restore reason, so a renderer never reads the
  drawing process's directory or the agent's files.
- **Version rule.** A call whose version its tool's renderer does not
  list, and a renderer that raises, draw with the default renderer over
  the standard content (`maki-ui/src/components/
  tool_display.rs:605-677`); a renderer is never given data of an unlisted
  version.
- **Requests.** The UI handle gains render requests for start, progress,
  done, load, click and rebake, with click replay, inline tasks and a warm
  cache, ported from the bridge's restore path
  (`maki-lua/src/domains/bridge/restore.rs`, `requests.rs`); replies reuse
  `store_snapshot` and the restore fan-out (`maki-ui/src/app/mod.rs:
  2338-2358`).
- **Composition.** A renderer can draw another tool's data through
  `maki.ui.renderer(tool, version)`, which returns the renderer that lists
  that version, or the default for a missing or mismatched one, so
  `batch` composes its children in the UI runtime under the same version
  rule.
- **Routing table.** The UI domain publishes which tool and version each
  renderer takes, as the slot store publishes layered tools, so `maki-ui`
  routes a call without asking the VM.

### Routing in the TUI

For each call, at start, progress, done, load, click and rebake, the first
that applies draws it:

1. the UI renderer, when the call has data and the routing table lists its
   tool and version;
2. the default renderer, when a renderer is registered for the tool but
   not for the call's version;
3. the legacy path, when no renderer is registered for the tool, as
   today: the bridge through the agent handle in `maki`, or the
   `toolRender` snapshots from a remote agent;
4. the default renderer.

The legacy path stays unaware of versions, as today
(`maki-lua/src/domains/bridge/restore.rs:155`): a tool's own legacy
fields read its own state.

The route is chosen once per call, from the first event that names its
tool and version, and it owns every later update of that row. That is
`ToolStart` for most calls; an input-hook refusal and a parse failure
send only `ToolDone` (`maki-agent/src/agent/tool_dispatch.rs:155`,
`:635`), which carries the tool and its data, so they route at done with
no synthetic start; a loaded call routes from its stored data, and one
without data takes the legacy path. The
bridge still emits header snapshots, body snapshots and live buffers on
its own for a tool with legacy fields (`maki-lua/src/api/tool.rs:517`),
and the TUI applies them directly today (`maki-ui/src/chat.rs:263`,
`:293`); on a call routed to a renderer or the default, the TUI drops
them. A renderer reload that changes a route rebakes the row: it clears
the old path's snapshots and live watchers before the new path draws.

With no bundled renderer in this stage, every tool draws as today in
`maki` and in `maki-acp-client`.

### Shared libraries

- `dir_listing` splits into its listing (agent: `maki.fs`, instructions)
  and its view (UI); `partial` splits into the model-facing cut text
  (agent) and the view paint (UI), so model text no longer uses
  `maki.ui.humantime`.
- The pure formatting helpers (`humantime`, `display_width`,
  `truncate_text`) get a home both runtimes reach, `maki.text`, with the
  `maki.ui` names kept as aliases until stage 7b removes the render names
  from the agent runtime.

## Commits

Each commit passes `just ci`, and `maki` draws every tool as today after
each.

1. `summary`: the field, its fallback to the header text, and its live
   reader.
2. Tool data types: `ToolData`, `data_version`, state for every result,
   carried on the result and the events.
3. Stream: `ctx:stream`, `ctx:set_state`, `AgentEvent::ToolProgress`,
   the dispatcher's coalescing and flush, ignored by every consumer.
4. Children: paths, records and child progress events.
5. Capture and persistence: `process_tool_calls`, `SessionTrack`, the
   `data` field of the `out` record, the stored summary and input on
   replay.
6. Wire: the `tool_data` extension, its agreement turning `toolRender`
   off, server sends and replay, `DisplayMapper` decoding; only test
   clients ask for it.
7. UI renderers: `register_renderer`, the context, the version rule, the
   render request, `maki.ui.renderer`, the routing table.
8. Renderer clicks: the click request with click replay, inline tasks
   and a warm cache, ported from the bridge's restore path.
9. Routing in the TUI.
10. Shared libraries: `dir_listing`, `partial` and the `maki.text` helpers.

## Tests

- Summaries: every bundled tool's start title is today's header text; a
  replayed title is the stored summary; a session without data replays as
  today.
- Data: a handler's `state` reaches `ToolData` for a diff, an image and an
  error; `data_version` defaults to 1; an input hook's replacement is the
  stored input.
- Children: a batch child's data is recorded under the parent in
  invocation order with its path; a grandchild's path extends its
  parent's; a cancelled parent marks running children cancelled; a call
  outside a handler records nothing.
- Persistence: a `maki acp` session reloads with its outputs and data; the
  data survives an append, a full rewrite, a fork, a prune and a
  resume-then-save; an older session file without the field loads as
  today.
- Stream: `ctx:stream` and `ctx:set_state` produce coalesced
  `ToolProgress` events with no live buffer; a call that ends before its
  first tick, with an error or a cancel, flushes its progress before
  `ToolDone`; the returned state replaces the live state.
- Wire: a test client that agrees `toolData` gets data, annotations and
  child updates and no `toolRender`; one that does not gets `toolRender`
  as today; replay of persisted data; oversize data is dropped as the
  render extension drops it; an input and a state with CRLF and control
  characters decode unchanged remotely, and draw sanitized.
- Children live: a child's annotation and usage update its record before
  it completes; a running child picks a matching renderer, and the
  default on a mismatch, locally and remotely.
- Routes without a start: an input-hook refusal and a parse failure route
  at `ToolDone`, and their clicks and rebakes keep that route.
- Annotations: the joined display text is today's, live, remote and on
  replay.
- Renderers (fixtures in the UI composition): a registered renderer draws
  a fixture tool; a mismatched version and a raising renderer draw the
  default, and a tool with no renderer takes the legacy path; a tool with
  legacy fields and a renderer draws only through the renderer, and on a
  version mismatch only through the default, with its legacy snapshots
  and live buffers dropped; `maki.ui.renderer` composes a child; reload
  clears a plugin's renderers.
- Routing: with no renderer, every bundled tool draws as today in `maki`,
  live, restored and remotely.

## Checks

Full `just ci` after each commit. Live after commits 5, 6 and 9: `maki`
draws `bash`, `edit` and `batch` as before; `maki connect -- maki acp`
reloads a session with its outputs; `maki-acp-client -- maki-acp-agent`
draws tools as before through `toolRender`.
