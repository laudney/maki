# Agent and UI split: stage 7b, renderers and the bridge's exit

Status: proposed. The second half of stage 7 of the [implementation
design](agent-ui-split-implementation.md), after the data path of
[stage 7a](agent-ui-split-stage-7.md): every bundled tool draws through a
UI renderer, the render bridge leaves the agent runtime for a crate only
`maki` uses, and the agent crate stops depending on drawing.

## Refinements

- **Order.** The bridge can leave the agent runtime only once no bundled
  agent entry needs it, so the tool migrations come first, then the
  bridge crate, then the client's request for `toolData`.
- **One commit per tool.** Each migration moves one tool's drawing to its
  `ui.lua` and is checked against today's drawing on its own; the four
  edit tools share one plugin and one commit.
- **The client asks for `toolData` last, as 7a decided.** Until then
  `maki-acp-client` gets `toolRender`, which a migrated tool no longer
  fills, so a migrated tool draws with the default renderer there; `maki`
  draws every tool as today after each commit, since its events carry the
  data. The gap closes in the last commit and never ships.
- **`batch` learns renderers first.** `batch` draws its children through
  their `header` and `restore` exports (`plugins/batch/init.lua:139`,
  `:165`), which each migration removes, and from the text `call_tool`
  returns (`maki-lua/src/api/agent.rs:637`). So the first commit gives
  `call_tool` the child's data: its `ToolData` and output with the text,
  and an `on_progress(update)` callback beside `on_live_buf`, whose update
  is the child's `ToolProgress` (its whole record at start and on each
  change, with the effective input, version and summary, and its streamed
  text and state), so a running child draws what it runs; and
  `maki.ui.renderer` gains `click(data, ctx, row)` beside `header` and
  `body` (`maki-lua-ui/src/api/ui/renderer.rs:353`). `batch`'s legacy
  drawing then keeps a data table per child and draws a child whose tool
  has a renderer through it, header, body and clicks, in `maki`'s one VM,
  and keeps `get_tool` for the others. Every later migration checks
  `batch` too, and `batch`'s own migration reuses the same composition.
- **The question form stays where it runs.** In `maki` the handler opens
  the form in the one VM, through the plugin's shared module, as
  `todo_write`'s state does; under ACP the server serves the call with its
  elicitation tool (`maki-acp/src/server.rs:714-727`) and the handler
  never runs. That tool returns text only today (`:999`), so it gains the
  same structured result: the local tool contract
  (`maki-agent/src/tools/mod.rs:290-300`) carries a `state`, and the
  server's question tool fills `answers` or `dismissed`. Only the card
  moves to the renderer; stage 10 removes the in-VM path.
- **Mixed plugins join the UI host with their features unavailable.** The
  UI host leaves out `memory`, `task`, `todo_write` and `question` today
  (`maki-lua-ui/src/ui_host.rs:39`), because their `ui.lua` commands need
  agent data (`plugins/memory/ui.lua:81`). Their renderers live in the
  same `ui.lua`, so they join, and each such command answers
  "unavailable" in `maki-acp-client` until stage 8 serves it, as the
  parent design says for these features.
- **The login prompt moves to the binary.** The provider login reads a
  secret with `crossterm` raw mode inside the agent crate
  (`maki-lua/src/api/provider.rs:555-600`). Only a binary owns a terminal,
  so the read goes behind a prompt the host passes in.

## Today

- Every bundled tool draws through `header`, `restore`, `start`, a handler
  body or a live buffer, with `maki.tool_view` in its agent entry (the
  per-tool table in the [stage 7 survey section](agent-ui-split-stage-7.md)
  and below).
- The render and bridge domains are installed in both compositions
  (`maki-lua/src/domains/mod.rs:81-86`), so `maki-lua` depends on
  `maki-lua-render`, `maki-highlight` and `crossterm`
  (`maki-lua/Cargo.toml:19-26`).
- Three restores read the agent's files: `read` (`maki.fs.read_lines`),
  `edit` (`maki.fs.read`), `index` (`maki.fs.metadata`); every
  `shorten_path` call reads the drawing process's directory.
- `UiHost` loads only `sessions`, `thinking`, `question` and `lib`
  (`maki-lua-ui/src/ui_host.rs:41-58`).

## Design

### The migration of one tool

Each tool moves the same way:

- **Agent entry** (`init.lua`): a `summary(input)` that returns today's
  header text; no `header`, `restore` or `start`; no `maki.tool_view` and
  no `maki.ui`; the facts its drawing needs in its reply `state` (below);
  live output through `ctx:stream` and `ctx:set_state`; `data_version = 2`.
- **State and output hooks**: from version 2 a reply's state goes to
  `ToolData` only, not into `TextOutput.state` (`maki-lua/src/api/tool.rs:
  602`), which would turn off the call's output hooks
  (`maki-types/src/tool.rs:440`, `maki-agent/src/agent/tool_dispatch.rs:
  391`). The hooks' guard still holds: when an output hook changes a
  call's output, dispatch clears its `ToolData.state` and sets a new
  `rewritten` field on it, and a rewritten call draws with the default
  renderer over its output, live, over the wire, on replay and as a
  `batch` child (`maki.ui.renderer` answers the default for it), so no
  renderer draws text a hook replaced, a tool without state included.
- **UI entry** (`ui.lua`): `maki.ui.register_renderer(tool, { versions =
  {1}, header, body, click })`, drawing from `data` what the old header,
  body and restore drew, with `maki.tool_view` and the other UI libraries.
  Paths are shortened against `ctx.cwd` and `ctx.home`.
- **UI host**: the plugin joins the list `UiHost` loads.
- **Versions**: a migrated tool declares `data_version = 2`, since its
  state takes the new shape below. Version 1 is a call from before its
  migration: 7a saved version 1 data for every bundled call, with the old
  reply state. Each renderer lists `{1, 2}` and draws version 1 the way the
  old `restore` did, the one place it may parse model text.
- **Old sessions**: a loaded call without data (a session from before
  7a, or one an older build rewrote) gets version 1 data built from what
  is stored: the tool and input of its `ToolUse` and `TextOutput.state`
  as its state. The TUI routes it like any call
  (`maki-ui/src/components/messages/route.rs:267` routes it to the legacy
  path today), so a renderer draws it, clicks included.
- **Parity**: a test draws the tool's calls through the renderer and
  through the legacy bridge from the same recorded call and compares the
  snapshots line by line, live, on done and on load; the tool's cases in
  `maki-lua/tests/real_plugins_restore.rs` move to it.
- **Agent load**: a test loads the agent entry in an agent runtime
  without the render domain, as 7a's library tests do.

### Facts per tool

What each tool's reply `state` carries, so no renderer parses model text
or reads the agent's files:

| Tool | State | Renderer draws from |
| --- | --- | --- |
| list | none | output text |
| webfetch, websearch | the display text, which is longer than the model text (`plugins/webfetch/init.lua:156`, `plugins/websearch/init.lua:212`) | state |
| skill | the resolved location and content | state |
| view_image | none | input path; the output image draws natively |
| write | none | input content and path |
| glob | the full file list | state, else output text |
| todo_write | none | input todos |
| memory | none | input and output |
| task | none | output markdown, status |
| question | `answers` or `dismissed`, as today | input questions, state |
| edit, multiedit, edit_lines, insert_lines | the start line of each block | output diff, state |
| grep | the entries | state, else output text |
| read | the displayed lines with their numbers, start, total, the truncation notice, the highlight context lines | state |
| index | `is_dir`, `line_meta` | output skeleton, state |
| code_execution | its view's content (below) | input code, streamed text live, state at done |
| bash | the rewritten command, exit code, timeout, cut reason, its view's content (below) | input, streamed text live, state at done |
| batch | its own view state only | the children's data, through `maki.ui.renderer` |

A tool that streams into a `ToolView` keeps what the view keeps: the
first lines up to its expansion cap, the tail it shows collapsed, and
the count of lines it left out (`plugins/lib/maki/tool_view.lua:143`,
`:250`), bounded as today, so the collapsed and expanded views both draw
as now after done and on replay.

A fact today read from the agent's disk at restore time (`read`'s context
lines, `edit`'s line numbers, `index`'s `is_dir`) is taken once in the
handler and stored in state, so a replay draws what the call saw. Display
text is kept in state for a cancelled or failed call too, because the
TUI drops the streamed text at done
(`maki-ui/src/components/messages/route.rs:285`), and it stays under the
wire's size limit or the call draws with the default.

### The bridge crate

- **Crate.** `maki-lua-bridge` takes `maki-lua/src/domains/bridge*`, the
  render domain's installation, `LiveView`, the `get_tool` header and
  restore exports, and the agent paths that serve the bridge
  (`domains/agent/tools.rs:266-355`, `api/util/ctx.rs:26-41`,
  `api/tool.rs:516-534`). Only the combined composition installs it, so
  third-party `header`, `restore`, `start`, handler bodies and
  `maki.tool_view` in an agent entry keep working in `maki` until stage 10.
- **Agent runtime.** `maki acp` and `maki-acp-agent` do not install it.
  A tool's `header`, `restore` and `start` are ignored there, with one
  warning per tool naming the migration, and the tool draws with the
  default renderer; `maki.ui.*` and `require("maki.tool_view")` in an
  agent entry fail with the migration message, as any UI call does.
- **Dependencies.** `maki-lua` then depends on neither `maki-lua-render`
  nor `maki-highlight`; `just deps-check` gains that rule.

### The login prompt

`maki-lua`'s provider login asks a `LoginPrompt` the host passes in for a
line or a secret. The binaries give the terminal implementation, which
keeps today's raw-mode masked read and moves `crossterm` out of
`maki-lua`; an agent with no terminal answers "not available", as a
missing UI does.

### The client asks for `toolData`

The last commit makes `maki-acp-client` and `maki connect` ask for
`toolData`; the server then sends no `toolRender` to them, and every
bundled tool draws through its renderer remotely, live and on replay. A
third-party tool that still draws through the bridge draws with the
default renderer there, as the compatibility rule says.

### Docs

The plugins page gains renderers, data versions, `summary`, the stream
API, and a migration guide for a third-party tool: what moves to `ui.lua`
and what stays, with the `glob` migration as the worked example.

## Commits

Each commit passes `just ci`, and `maki` draws every tool as today after
each.

1. Renderer support: `shorten_path` against `ctx.cwd` and `ctx.home`, the
   parity harness, data for old sessions, the state rule for output hooks,
   `call_tool`'s child data and `on_progress`, `maki.ui.renderer`'s
   `click`, and `batch` drawing a child through its renderer.
2. list.
3. view_image.
4. websearch.
5. webfetch.
6. skill.
7. write.
8. glob.
9. todo_write.
10. memory.
11. task.
12. question, with the structured result of the server's question tool.
13. The edit family.
14. grep.
15. read.
16. index.
17. code_execution.
18. bash.
19. batch.
20. The bridge crate, and the agent runtime ignores legacy fields.
21. The login prompt, and `crossterm` leaves `maki-lua`.
22. The client asks for `toolData`.
23. Docs.

## Tests

- Per tool: the parity test (live, done, load, click where the tool has
  one), the same inside `batch` (a state-dependent child and a streaming
  one), version 1 data saved by 7a and a session from before 7a, and its
  agent entry loads without the render domain; display text above the
  model limit and output above the expansion cap, at done and on replay,
  clicks included, where the tool keeps them.
- Output hooks: a hook that rewrites a migrated tool's output still runs,
  and the call draws its rewritten output with the default renderer, for
  a tool with state, one without (`write`), and a `batch` child, locally,
  over ACP and on replay.
- `batch`: a running child whose input hook replaced its code draws the
  replaced code.
- Question: accepted, skipped and dismissed answers, live and replayed,
  in `maki` and through the server's question tool.
- Mixed plugins: each command that needs agent data answers
  "unavailable" in `maki-acp-client`.
- Bridge crate: a third-party fixture with `header`, `restore` and a
  handler body draws as today in `maki`; in the agent composition it
  loads, warns once, and draws with the default; `maki.ui.buf` in its
  agent entry fails with the migration message.
- Dependencies: `just deps-check` fails if `maki-lua` reaches
  `maki-lua-render`, `maki-highlight` or `crossterm`.
- Login: the terminal prompt keeps today's masked read (unit test of the
  byte handling); an agent without a prompt answers "not available".
- Client: with `toolData` agreed, a bundled tool draws through its
  renderer remotely, live and on replay; a legacy third-party tool draws
  with the default.

## Checks

Full `just ci` after each code commit; the docs commit runs the
documentation checks only. Live after each migration in `maki`:
the tool draws as before, live, after a reload, and on click. Live after
commit 22: `maki-acp-client -- maki-acp-agent` and `maki connect -- maki
acp` draw every bundled tool as `maki` does, live and on replay.
