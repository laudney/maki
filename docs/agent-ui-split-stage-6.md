# Agent and UI split: stage 6, the UI runtime

Status: proposed. Stage 6 of the [implementation
design](agent-ui-split-implementation.md): a `maki-lua-ui` crate holds the
UI binding over `maki-lua-core`, a `maki-lua-render` crate holds the
buffer and highlight APIs both bindings draw with, the facade namespaces
move to the core over a local and a UI backend, and `maki-acp-client` runs a
UI runtime of its own with the bundled UI plugins `thinking` and
`sessions`, answering them from the agent it is connected to. `maki`
does not change.

## Refinements

- **Two parts.** The stage row bundles the runtime with UI packages,
  the `[ui]` manifest table, component approvals and the user's
  `ui.lua`. The runtime ships first, here; the packages and the user
  configuration are stage 6b, with their own specification, since they
  touch the package manager and its approval store and need nothing of
  this part beyond the UI host.
- **A render crate.** Buffers, blits, highlight, markdown and the theme
  styles are drawn by the agent's render bridge until stage 7 and by the
  UI runtime for good. Neither binding may depend on the other, so they
  move to `maki-lua-render`, which both depend on. Stage 7 drops the
  agent's edge to it, and the design's rule that the agent crate never
  reaches `maki-highlight` then holds as written.
- **crossterm stays in the agent until stage 7.** Two agent-side calls
  read the terminal: the bridge's `terminal_size`
  (`maki-lua/src/api/ui/mod.rs:387`), which `tool_view` calls, and the
  provider login's raw-mode secret read (`maki-lua/src/api/provider.rs:573-577`).
  The first leaves with the bridge in stage 7; the second gets a
  replacement then. `deps-check` keeps `crossterm` pending for
  `maki-acp-agent` and `maki-host` and names why.
- **The facade lives in the core.** All three compositions install
  `maki.session`, `maki.model`, `maki.task` and `maki.plan`. Each
  function keeps today's route, which differs per function: `read` and
  `current` ask the headless snapshot slot first
  (`maki-lua/src/api/session.rs:223`, `:254`), `messages` asks the live
  history first (`:457`), `notify` posts to the mailbox without asking
  the UI (`:730`), and most others ask the UI and fall back to a local
  answer. So the namespaces stay one copy, in the core, with today's
  control flow, over two optional backends: a local one the agent
  provides and a UI one the UI provides. One copy of the code and docs
  keeps the compositions from drifting.

## Today

- The UI domain is about 9,800 lines of `maki-lua`: `domains/ui*`,
  `api/{action,keymap}.rs`, `api/ui/{win,view,marks,syntax}*`,
  `api/util/command.rs` (`UiAction`), `key.rs`, `key_lint.rs` and
  `question_form.rs`, plus UI parts of mixed files: `register_command` and
  `run_command` in `api/tool.rs:968-1274`, the UI half of
  `api/ui/mod.rs`, and the plan row half of `api/plan.rs:157-408`.
- What keeps it from leaving the crate:
  - `UiAction` carries `maki-agent`'s `Transcript` and `SessionExport`
    (`api/util/command.rs:8-9`, `:489-493`, `:794-797`);
  - the agent domain sends `UiAction` for plugin tasks
    (`api/agent.rs:207-262`, `:915`);
  - the facade sends `UiAction` and names `maki_agent` and
    `maki_providers` (`domains/facade.rs:22`, `api/session.rs:11-15`,
    `api/model.rs:6`);
  - buffers, marks and buffer keymaps reach the bridge's `BufHandle`,
    `parse_line` and `TaskView` (`api/keymap.rs:13`, `api/ui/marks.rs:14`,
    `api/ui/mod.rs:31`);
  - `question_form.rs:6`, `:99` builds an `AgentGrant` with
    `DeclAuthority`;
  - `maki-lua` builds the combined composition (`domains/mod.rs:48-91`,
    `loader.rs:388-436`), and one `EventHandle` (`loader.rs:1193-1537`)
    carries core, UI, bridge, agent and facade requests to `maki-ui`.
- `maki connect` and `maki-acp-client` start a combined `PluginHost`
  that loads no plugin and no init file, only so the event loop has its
  readers and a handle for the question form (`src/cmd/connect.rs:109-115`).
  The facade answers a remote tab from local state: `session.list` lists
  a scratch store, `model.get` reads a placeholder model, and `/thinking`
  is missing (`maki-ui/src/app/remote_settings.rs:79`).

## Design

### Buffer ownership

The bridge's `TaskView` (`maki-lua/src/domains/bridge/task.rs`) holds the
buffers and the live context in one extension, and its `complete` needs
both to publish a finished task's snapshot (`:137`). The split keeps one
owner per concern: `maki-lua-render`'s `TaskBufs` extension owns the
buffers, their claims and the `ended` flag exactly as `TaskView` does
today, including inline inheritance without a claim, buffers created
after the scope begins, warm claims and the clear order; the bridge's
live extension keeps `LiveCtx` and, to publish, reads the root buffer of
the inherited owner's claim, as `complete` does today (`:141`), never the
child's own buffers. `TaskExt::complete` receives only `&Lua` today
(`maki-lua-core/src/runtime.rs:561`) and runs under the cell lock
(`:2325`), so commit 4 gives it the cell's extension map as well:
`complete(&self, lua, exts: &TaskExts)`. The live extension reads the
buffer extension from that map through a `maki-lua-render` accessor and
never takes the lock again.

### Crates

| Crate | Holds | Depends on |
| --- | --- | --- |
| `maki-lua-render` | `api/ui/buf.rs`, `api/ui/blit.rs`, the render half of `maki.ui` (`buf`, `theme_color`, `theme_style`, `highlight`, `markdown`, `humantime`, `terminal_size`, `display_width`, `truncate_text`), the buffer half of the bridge's `TaskView` (`TaskBufs`, `BufsClaim`, `with_bufs`, `with_task_bufs`), and a `RenderDomain` that extends `maki.ui` with them and puts a buffer view on every task | `maki-lua-core`, `maki-types`, `maki-highlight`, `maki-markdown`, crossterm |
| `maki-lua-ui` | the UI domain, `UiAction` and its request and reply types, `key`, `key_lint`, `question_form`, the plan row half, `UiOps`, the UI composition and `UiHost` | `maki-lua-core`, `maki-lua-render`, `maki-types`, crossterm |
| `maki-lua` | the agent domain, the bridge's live half (`LiveCtx`, live snapshots), `LocalOps`, `PluginHost` | `maki-lua-core`, `maki-lua-render`, and `maki-lua-ui` as a dev-dependency only |

`maki-lua-ui` never depends on `maki-lua`, and `maki-lua` never depends on
`maki-lua-ui` outside its tests.

### Shared types and sinks

- `Transcript`, `SessionHeader` and `SessionExport` are data; they move to
  `maki-types` with re-exports at their `maki-agent` paths, as stage 1
  did. `SessionExport::stored` and `live` read only `StoredSession` and
  `HistorySnapshot`, both `maki-types` data, so they move as methods;
  `Transcript::into_session`, which calls agent-private history helpers,
  becomes the `maki-agent` function `transcript_into_session`. `SessionExport::envelope` calls the agent history's
  `facts_told` (`maki-agent/src/agent/transcript.rs:209`), which its
  `Serialize` needs. It is a scan over the messages
  (`maki-agent/src/agent/history.rs:305`), so it moves to `maki-types`
  with the data and keeps its evaluation point: the envelope is still
  computed when the export is serialized, off the event loop, never when
  the loop builds a live export (`maki-ui/src/event_loop.rs:1124`).
  `UiAction` then names no agent type.
- A core `TaskSink`, beside `HostSink`, takes the plugin task events the
  agent domain sends today as `UiAction::PluginTaskEvent` and
  `PluginTaskOpened`. The UI domain installs a sink that sends those
  actions; with no sink installed nothing is sent, as with no UI today.
  Stage 8 replaces the sink with subagent events over ACP.

### The facade

`maki-lua-core` gets the four namespaces with their control flow as it is
today, and two backend traits the composition installs in Lua app data:

- `LocalOps`, implemented in `maki-lua`: the headless snapshot slot, live
  sessions and live history, stored sessions, the mailbox, `store_new`
  and the model registry, each call as synchronous or as async as today.
- `UiOps`, implemented in `maki-lua-ui`: today's `UiAction` round trips,
  returning today's `Result` with its distinct errors (`NO_UI_ERR`, a
  dropped reply, the UI's own error), which the namespaces still tell
  apart.

A missing backend behaves as today's missing half: no `UiOps` is
`NO_UI_ERR`, as a host with no UI sender gets now, so the agent
composition answers as `maki acp` does; no `LocalOps` is no headless slot,
no live session, no mailbox and no stored session, so the UI composition
answers from the UI or "unavailable". The combined composition installs
both and so takes today's route for every function.

The backends exchange typed values, not JSON, where today's code does:
`export` and `messages` carry the envelope and an `Arc<Vec<Message>>`,
and the core keeps today's Lua conversion, which shares image payloads
as userdata and yields every 256 messages (`session.rs:330`). `Message`
is a `maki-types` type, so the core names no agent type.

### Handles and hosts

- `EventHandle` splits into a core handle (fire, dispatch, end session,
  package context), a UI handle (commands, keybinds, plan form, question
  form) and the agent handle (restore, click, prompt slots, session
  snapshot, cancel). `maki-ui` holds the core and UI handles and an
  optional agent handle, which a client leaves empty.
- The UI parts of a host are a core type, `UiParts`: the UI domains and an
  opaque bundle of their handles. `maki_lua_ui::ui_parts(interaction)`
  builds them without naming `PluginHost`. `Composition::Combined` carries
  them, so every combined start receives them from its caller:
  `PluginHost::start`, `PluginHost::new` and `with_all_builtins` take them,
  and `maki-host`'s `load_plugins` and `cli_stack`, which build combined
  hosts for `auth`, `models`, `index`, `prompt` and `migrate`, take the
  composition as a value. The root starts, `maki-docgen` and the tests
  build the parts; `maki-host` names only core types and never reaches
  `maki-lua-ui`. `maki-docgen` composes both crates' docs lists.
- A neutral test host in `maki-lua-ui`'s `test_support`, which starts core,
  render and the UI domain over fixture plugins, lands with the crate
  cut, so the UI tests that build an agent-backed `PluginHost` today
  (`question_form.rs:237`, `api/action.rs:302`) move with their code; a
  test that needs the agent domain stays in `maki-lua`, which may
  dev-depend on `maki-lua-ui`.
- `UiHost`, in `maki-lua-ui`, starts the UI composition: core, render,
  the UI domain and `UiOps`, with no agent domain. It loads the bundled
  UI entries of its own `Bundled` list, `thinking`, `sessions`, and the
  `question` and `lib` directories for `require`, so the client embeds no
  agent plugin. A composition without the agent domain guards `maki.fs`,
  `maki.agent`, `maki.provider` and `maki.interpreter` with the mirror of
  stage 5's message: `maki.<namespace>.<name> is not available in the UI
  runtime: move agent code to the plugin's init.lua`.
- `memory`, `task` and `completion` keep their UI in the combined
  composition only: their UI entries call `maki.fs` and `maki.task`, which
  the client cannot answer until stage 8, so `UiHost` does not load them.

### The client

- The client start moves from `src/cmd/connect.rs` into a
  `maki-acp-client` library, and root `maki connect` calls it, as stage 5
  did for the agent. It runs `UiHost` with the bundled UI plugins, or none
  under `--no-plugins`: `maki-acp-client` gains that flag, and root `maki
  --no-plugins connect` passes the root flag through, the one change to a
  command line in this stage. In the client it means no bundled UI
  plugin either, since the client has no builtin it must keep, so its help
  text says so, and the root flag's help notes the client meaning. The logging setup it shares with the agent start
  moves to a crate with no agent edge, so the client stops reaching
  `maki-host` and `maki-acp`.
- `maki-ui` answers the facade for a remote tab from the agent it is
  connected to:
  - **Ids.** A remote tab is named by its bound ACP session id.
    `session.current` returns it, and every request that takes a session
    id resolves an ACP id of a remote tab before it parses a local
    `MakiId` (`maki-ui/src/event_loop.rs:1754`), so `plan.read`, `read`,
    `prompt`, `set_mode` and focus of the current tab accept it. The UI
    APIs that take a session option parse it into a local `MakiId` in Lua
    today (`open_win`, status hints: `maki-lua/src/api/ui/mod.rs:177`,
    `:1092`), which also gives a malformed id its error at once
    (`maki-lua/tests/plugin_host.rs:8874`). The combined composition keeps
    that check, so `maki` keeps its synchronous error. In the UI
    composition every tab is remote and names itself by its ACP id, so
    there the session option is sent as given for the event loop to
    resolve, and `maki.ui.open_win(buf, {session =
    maki.session.current()})` opens in the remote tab.
  - **Rows.** `session.live` returns the remote tab as the one live row.
    `session.list` follows `session/list` pages and returns rows in the
    shape the local list returns: the ACP `updatedAt` string becomes
    epoch seconds, which the `sessions` picker subtracts
    (`plugins/sessions/ui.lua:144`), and a missing title gets the local
    default. The live tab is marked live as a local one is.
  - **Model and thinking.** `model.get` and `model.available` read the
    remote settings and model menus. `model.set` sends the change and
    waits for the agent's answer to that `set_config_option`: it returns
    the confirmed state, or an error when the agent refuses, the link
    drops or the session is replaced first. Today
    `RemoteSettings::change` only records an intent and reports a refusal
    later (`maki-ui/src/agent/remote_settings.rs:224`,
    `maki-ui/src/agent/remote_loop.rs:1125`), so it gains a reply for the
    Lua call; the menus keep their behavior. The loop sends only
    differences from the confirmed state (`remote_loop.rs:1074`), so: a
    value equal to the confirmed one completes at once with that state; a
    setting the agent does not offer fails at once; and a call whose
    intent a newer change replaces before it is sent completes with an
    error that names the newer change, while the newer call gets the
    agent's answer.
  - **The rest.** Delete, rename, export, messages, notify, new, focus on
    another id and the task calls answer "unavailable" until stage 8, and
    so does `model.set` with a model spec: the model menu switches models
    as today, but a Lua call would need a confirmed answer for the switch,
    which arrives with the stage 8 host operations; no bundled UI plugin
    switches models from Lua.

  `/thinking` thus changes the agent's thinking, and `/sessions` lists
  the agent's sessions.

### Dependency tests

| Package | Must not reach | Still reached after stage 6 |
| --- | --- | --- |
| `maki-lua-render` | `maki-agent`, `maki-providers`, `maki-interpreter`, `maki-lua`, `maki-lua-ui`, `ratatui` | none |
| `maki-lua-ui` | `maki-agent`, `maki-providers`, `maki-interpreter`, `maki-lua`, `ratatui` | none |
| `maki-acp-agent`, `maki-host` | `maki-ui`, `maki-lua-ui`, `crossterm`, `ratatui` | `crossterm` (bridge `terminal_size`, provider login, until stage 7) |
| `maki-acp-client` | `maki-agent`, `maki-providers`, `maki-lua`, `maki-acp` | `maki-agent`, `maki-providers`, `maki-lua` (through `maki-ui`, until stage 10) |

## Commits

Each commit passes `just ci`, and `maki` behaves as before after each.

1. `Transcript`, `SessionHeader` and `SessionExport` move to `maki-types`;
   their agent methods become `maki-agent` functions.
2. `TaskSink`: plugin task events go through it.
3. The UI code leaves agent and mixed files: `register_command` and
   `run_command` leave `api/tool.rs`, `api/ui/mod.rs` splits into its
   render and UI halves, the plan row half leaves `api/plan.rs`.
4. `maki-lua-render` with `git mv` of the render modules and a
   `RenderDomain`; `TaskExt::complete` gains the extension map;
   `deps-check` covers it.
5. The facade moves to the core over the `LocalOps` and `UiOps` traits;
   both implementations stay in `maki-lua` until commit 8.
6. `EventHandle` splits into the core, UI and agent handles.
7. `PluginHost::start` takes the UI parts from its caller.
8. `maki-lua-ui` with `git mv` of the UI domain, `UiAction`, `key`,
   `key_lint`, `question_form`, the plan row half and `UiOps`, with its
   neutral test host and the UI tests moved onto it; `maki-ui`,
   `maki-docgen` and the root start import from it; `deps-check` covers
   it.
9. `UiHost`, its `Bundled` list and the agent-namespace guard, tested over
   fixture plugins and the bundled UI plugins.
10. `maki-ui` answers the facade for a remote tab from the agent.
11. The client start moves to the `maki-acp-client` library and runs
    `UiHost`; root `maki connect` calls it; `deps-check` drops `maki-acp`
    from the client's pending list.
12. Docs: the CLI page's `maki connect` section (it says the client starts
    no plugin, `site/docs/content/cli/_index.md:221`), the client's
    commands and `--no-plugins`, and the generated Lua API notes on which
    facade operations a client answers.

Commits 5 and 8 are the large ones; each may split by namespace or file
when the diff demands it, keeping each step green.

## Tests

Every existing test passes, apart from import paths and the
`PluginHost::start` arguments. New tests:

- the UI composition loads `thinking` and `sessions` and registers their
  commands and keymaps, and loads no tool;
- in the UI composition, a call to an agent name from a UI entry fails with
  the mirror message, and another plugin still loads;
- each facade function answers in the agent, UI and combined compositions
  as it does today in the matching host (fixture `LocalOps` and `UiOps` record the
  calls);
- for a remote tab, over a fake ACP agent as the connect tests use:
  `session.list` returns the agent's sessions across pages with epoch
  `updated` values; `session.current` returns the ACP id and `plan.read`
  accepts it, and `maki.ui.open_win` with it opens in the remote tab;
  `model.set` with a thinking level returns the confirmed state, returns
  at once for an unchanged value, fails at once for a setting the agent
  does not offer, fails when the agent refuses, the link drops or the
  session is replaced, and a superseded call fails while the newer one
  completes; delete answers "unavailable".

## Checks

Full `just ci` after each commit. Live after commits 8 and 11: `maki` with
`/memory`, `/tasks`, `/sessions`, `/thinking` and the todo panel behaves as
before. `maki-acp-client -- maki-acp-agent` answers a prompt and draws a
tool body; `/thinking` changes the agent's thinking level, `/sessions`
lists the agent's sessions; and `maki-acp-client --no-plugins --
maki-acp-agent` answers a prompt with neither command.
