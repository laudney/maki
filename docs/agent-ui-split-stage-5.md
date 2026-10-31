# Agent and UI split: stage 5, the Lua core

Status: proposed. Stage 5 of the [implementation
design](agent-ui-split-implementation.md): a new crate, `maki-lua-core`,
holds the VM machinery with no agent edge; the rest of `maki-lua` becomes
domains the core calls through one trait; a plugin host is built in a
composition of domains; the bundled plugins move their non-tool UI to a
`ui.lua`; and `maki acp` runs the agent composition. `maki` does not
change.

## Refinements

This stage refines the implementation design where the code demands it:

- **Two compositions, not three.** Stage 5 builds the combined and the
  agent composition. The UI domain stays a module set of `maki-lua`
  behind the same trait, and the UI composition arrives with
  `maki-lua-ui` in stage 6, when the UI domain can leave the agent crate.
  A UI composition now would have no user.
- **The facade namespaces stay in `maki-lua`.** `maki.session`,
  `maki.model`, `maki.task` and `maki.plan` choose their answer at run
  time today: a headless provider (`SessionSnapshotSlot`,
  `maki-lua/src/api/session.rs:223`), a `UiAction` round trip, or the
  stored session. That choice is the `AgentOps` facade in small, and
  stage 6 gives it its ACP answer; until then the four namespaces are one
  domain of `maki-lua`, the facade domain, installed in both
  compositions.
- **`maki-lua` reaches `maki-highlight` until stage 7.** The render
  bridge calls `maki_highlight` and `maki_markdown`
  (`maki-lua/src/api/ui/mod.rs:7`, `:253`, `:331-359`), so the rule that
  the agent crate never depends on them holds from stage 7. Neither crate
  reaches crossterm or ratatui, so the binary rule is unaffected.
- **The agent start goes to a crate of its own.** Stage 4 left the agent
  start in the root library for this stage. Both `maki` (its TUI and
  headless starts) and `maki-acp-agent` need it, and the TUI must not
  depend on a package named for the ACP agent, so it moves to a new
  library crate, `maki-host`, not into `maki-acp-agent`.

## Today

`maki-lua` is one crate of about 76,000 lines. `runtime.rs` (7,287 lines)
runs the VM thread: a request loop over 31 `Request` variants
(`runtime.rs:377-576`), the plugin load and clear paths, task scopes and
the module loader. Agent, UI and bridge code is woven into it:

- `load_source`, `rollback_load`, `clear_plugin` and `drop_plugin_keys`
  (`runtime.rs:3101-3180`, `:3396-3668`) call tool registry, permission
  rule, provider, keymap, command, hint, window and input stores inline.
- `LuaRuntime` (`runtime.rs:2941-2958`) holds the tool registry, the
  pending tools, the plugin rule store, live and warm tools, the UI
  sender and the question forms next to the VM.
- `create_maki_global` (`maki-lua/src/api/mod.rs:54-192`) builds every
  namespace for every load, and `maki.api`, `maki.ui` and `maki.fn` each
  mix domains.
- `TaskCell` (`runtime.rs:722-768`) carries tool buffers and a live sink,
  and `TaskScope::drop` ends background sessions (`runtime.rs:1879`).
- The watchdog, permission warnings, unload and `maki.notify` reach the
  UI through `UiAction` (`runtime.rs:1414-1491`, `api/top.rs:159`).
- `NetEgress`, in the permission model, asks `maki_providers::plugin`
  for provider origins (`plugin_permissions.rs:112`, `:145`), and
  `maki.fn` jobs strip provider keys through `maki_providers`
  (`api/fn.rs:17`).

A bundled plugin loads only its `init.lua` (`loader.rs:549-555`), which
registers its tools and, for `todo_write`, `memory`, `task`, `sessions`,
`thinking` and `completion`, commands, keymaps, pickers and panels.
`maki acp` loads every plugin like the TUI, with no UI sender, so a UI
registration succeeds and is never used.

## Design

### The core crate

`maki-lua-core` takes the VM thread and request loop, task scopes, the
watchdog, the JIT and codegen queue, the module loader and bundled
modules, the permission model (`plugin_permissions`), packages (`pack`,
`api/pack`), options, autocmds with their event vocabulary
(`agent_autocmd`, whose items are all `maki-types` data), the slot
mechanism, `chunks`, `language`, `docs` and `docs_render`,
the errors, and the libraries `async`, `base64`, `env`, `image`, `json`,
`log`, `net`, `split`, `text`, `treesitter`, `uv`, `yaml`, the job half
of `maki.fn` and the top-level `maki` functions. It depends on
`maki-types`, `maki-config`, `maki-storage`, `maki-pack`, `mlua` and the
pure crates those modules use today, and on none of `maki-agent`,
`maki-providers`, `maki-interpreter`, `maki-highlight`, `maki-markdown`,
crossterm or ratatui.

Modules keep their names and paths inside the new crate where they move
whole. Mixed files split: `runtime.rs` keeps the core and its domain code
goes to the domain modules below, `api/tool.rs` gives `register_options`
to core, `api/ui/mod.rs` splits into bridge and UI parts, and `api/fn.rs`
keeps the jobs and gives `winsaveview`, `winrestview` and `mode` to the
UI domain. `key` and `key_lint` go to the UI domain: `Key` wraps
crossterm's key codes (`maki-lua/src/key.rs:7`), and the lint checks
keymap strings in plugin source.

Commit 9's crate does not compile while a core module names a path in a
module that stays in `maki-lua`, so commits 1 to 8 remove every such
path. The known ones: `slot` reads `api::tool::permission_keys`
(`maki-lua/src/api/slot.rs:14`), which moves to the permission model;
`pack` reads `loader::is_bundled` (`maki-lua/src/pack.rs:16`) and
`docs_render` reads `loader::lib_dir` (`maki-lua/src/docs_render.rs:11`).
Both become a `Bundled` value in core, the bundled directories, their
plugin names and the `lib` directory, that `maki-lua` builds from its
`include_dir!` list. The core takes it at spawn, with the directories it
takes today (`maki-lua/src/loader.rs:346-352`), and the core functions
that run without a VM take it as an argument: package discovery checks
reserved names with no runtime (`maki-lua/src/pack.rs:1548`, `:1577`), and
`maki-docgen` calls `docs_render::site_page()` directly
(`maki-docgen/src/gen_lua_api.rs:13`). `maki-lua` keeps wrappers with
today's signatures that pass its `Bundled`, so their callers do not
change.

### Items that move to shared crates

Each item moves with a re-export at its old path, as in stage 1:

| Item | From | To | Why |
| --- | --- | --- | --- |
| `CancelMap`, `CancelSlot` | `maki_agent::cancel` | `maki_types::cancel` | core's spawned tasks (`runtime.rs:2505`); std and `CancelTrigger` only |
| `HookStage`, `Authority`, `OUTPUT_TEXT`, `OUTPUT_IS_ERROR` | `maki_agent::tools::hook` | `maki_types::hook` | the slot mechanism (`api/slot.rs:9-10`); data over `maki_config::Permission`. `impl From<Option<ToolReach>> for Authority` (`maki-agent/src/tools/hook.rs:67`) would break the orphan rule once `Authority` is foreign, so it becomes the function `authority_of(Option<ToolReach>) -> Authority` in `maki-agent`, and its callers change |
| `byte_highlights` | `maki_agent::file_index` | `maki_types::text` | `maki.text` (`api/text.rs:78`); pure, adds `nucleo-matcher` and `unicode-segmentation` to `maki-types` |
| `shell_command` | `maki_providers::child_env` | `maki_config` | jobs (`api/fn.rs:102`); pure over `ShellPreference`, which lives there |
| `Line`, `MAX_READ_BYTES`, `expand_tilde`, `next_line` | `maki-lua/src/api/fs.rs` | core `api/util` | jobs read them (`api/fn.rs:22`) |

Every other agent item a core-bound file names is already a `maki-types`
or `maki-storage` item behind a re-export (`CancelToken`, `SharedBuf`,
`UiWaker`, the snapshot and span types, `ImageSource`, `EventSender`,
`AgentEvent`, `Envelope`, `SessionEndReason`, `TokenUsage`,
`compact_duration`, `add_cost`), and is renamed to its own path.

### Hooks the core calls

Three seams replace the core's direct calls into agent and UI code. The
composition supplies each; a missing one has the stated default.

- **`HostSink`**, for what the core tells the user: `flash(text) -> bool`
  and `plugin_unloaded(plugin)`. The watchdog, permission warnings, a
  failed spawn and `maki.notify`'s default path flash; `flash` says whether
  anyone received the text, so each caller keeps logging as it does today
  without a UI. The UI domain's sink sends the `UiAction`s sent today; the
  agent composition's sink receives nothing, and a host with no sink
  installed behaves the same. `as_plugin_load` and the attachment flag it
  reads (`api/util/command.rs:870-979`) move to core, since core runs
  every chunk under it.
- **`ProviderOrigins`**, which `NetEgress` asks instead of calling
  `maki_providers::plugin`: `vouched(slug) -> Option<Url>` and
  `authority(slug) -> Option<String>`. With them it gives a
  `ProviderRoute` for `maki.net`'s provider route: the user agent and the
  connect and stall bounds that `user_agent()` and `Timeouts::default()`
  give today (`api/net.rs:332-352`, `:975`). `provider_get` stays in core
  `net`, and only the agent's `maki.provider` calls it.
- **`ChildEnv`**, which jobs call on each child `Command` before spawn, in
  place of `strip_provider_keys` (`api/fn.rs:322`). The agent domain's
  strips the provider keys as today.

The provider registry these two front is process-wide, so they are
process-wide registrations, not values a host threads into each
`NetEgress`. The agent domain installs both at host start, and the first
install wins: every host installs the same adapter. With none installed,
no provider origin is vouched for or served, and a child's env is left as
it is. That default is safe because only `maki.provider`, an agent API,
gives a plugin a slug, so a runtime without the agent domain never asks.

### Domains

A domain is one part of a runtime: the agent domain (tools, hooks,
prompt slots, providers, interpreter, `maki.fs`, `maki.agent`, background
sessions), the bridge (buffers, highlight, markdown, theme styles,
`tool_view`, tool `header`, `restore`, `start` and click), the UI domain
(commands, actions, keymaps, windows, input, hints, forms, `ui.*` slots,
`maki.notify`'s sink) and the facade domain (`maki.session`,
`maki.model`, `maki.task`, `maki.plan`). Core defines:

```rust
pub trait Domain: Send + 'static {
    fn name(&self) -> &'static str;
    /// Once, on the VM thread, before any load: the domain's stores go
    /// into Lua app data, and its thread-local callbacks are set. `&mut`,
    /// because a domain hands over writers that are not `Clone`.
    fn install(&mut self, vm: &Vm) -> LuaResult<()>;
    /// Per load: the names this domain adds to the plugin's `maki`.
    fn extend(&self, vm: &Vm, maki: &Table, load: &LoadScope) -> LuaResult<()>;
    /// Before a load's chunks run: drop what the previous load of the
    /// name published, and revive what `clear_plugin` tombstoned.
    fn reload(&self, vm: &Vm, plugin: &str);
    fn commit_load(&self, vm: &Vm, plugin: &str) -> Result<(), PluginError>;
    /// A failed load: drop what it published, keep its provider grant.
    fn rollback_load(&self, vm: &Vm, plugin: &str);
    fn clear_plugin(&self, vm: &Vm, plugin: &str);
    /// The explicit unload only, after `clear_plugin`: release the grant.
    fn unload(&self, vm: &Vm, plugin: &str);
    fn end_session(&self, vm: &Vm, session: &SessionRef);
    /// A task scope began: a domain puts its extensions on the cell, so a
    /// task queued before the domain's first use still inherits its share.
    fn begin_task(&self, vm: &Vm, exts: &mut TaskExts);
    /// A task scope ended, after its jobs were cleaned up.
    fn end_task(&self, vm: &Vm, task: TaskId);
    fn docs(&self) -> &'static [DocEntry];
}
```

- **The VM.** `Vm` is the Lua state for the load and teardown calls,
  which need nothing more, and gains the executor and the gates for
  `DomainRequest::serve`, so the lifecycle methods take `&Lua` until then.
- **State.** A domain keeps its stores in Lua app data, as most stores
  are kept today; `LuaRuntime` loses `pending`, `plugin_rules`, `registry`,
  `live_tasks`, `warm_tools`, `ui_action_tx` and `question_forms`, and
  `PluginOwner.tools` becomes the bridge's and the agent's per-plugin
  records.
- **Phases.** Each phase runs its core step first and then the domains
  in composition order (agent, bridge, facade, UI), except `commit_load`,
  where the domains commit first and the core records the plugin's owner
  last, so a commit that fails leaves no owner for the rollback to remove.
  The bridge's tool keys arrive in the agent's pending tool, since
  `register_tool` takes them with the handler, so the agent's commit stages
  them for the bridge's; both are the agent binding until stage 7. Where today's code
  interleaves the steps of several domains (`drop_plugin_keys`,
  `clear_plugin`), each step clears its own store and reads no other, so
  the order is equivalent; the implementing commit names any exception it
  finds. The phases map today's paths as follows: a load is `reload`
  (`runtime.rs:3466-3474`), the chunks, then `commit_load`, or
  `rollback_load` when a chunk or a commit failed (`:3614-3621`, which
  keeps the provider grant); `ClearPlugin` is `clear_plugin` then `unload`
  (`:5076-5082`, the one place a grant is released). The revision guard
  and the plugin's env record stay core and are released where they are
  today.
- **Load.** `load_source` keeps its core steps: the pending checkpoint,
  the require root, the env, the chunk runs and the option check. It calls
  `extend` on each domain when it builds the `maki` global, so a failure
  still drops everything the load registered. The `Owner` that `create_maki_global` takes keeps the name
  and gains an opaque domain grant in place of `DeclAuthority` and
  `granted_providers`, which `LoadContext` carries through for the agent
  domain.
- **Clear and teardown.** `clear_plugin`, `drop_plugin_keys`, the
  session-end hook (`runtime.rs:1797-1822`) and `TaskScope::drop` keep
  their core steps and call the domains for the rest, so the order of the
  steps does not change.
- **Thread and handles.** Core's `spawn` takes the request lanes, the
  domains and the layered-tool table the core slot store reads, and
  returns a core handle: the two senders, the join handle and the shutdown
  flag. A
  domain is built on the caller's thread with its channel halves, keeps
  the reading halves for the host side, and moves to the VM thread, so
  `Domain` is `Send`. `PluginHost` and `EventHandle` stay in `maki-lua`
  and wrap the core handle with the domains' handles: the command, keymap,
  hint and input-rule readers, the `UiAction` and wake receivers, the UI
  attachment and the key lint are the UI domain's, and `layered` and
  `background` the agent's (`runtime.rs:4768-4799`). The tool registry and
  the plugin rule store become arguments of the agent domain, and
  `Interaction` stays a core argument the UI domain reads when it is
  built. `cancel_plugin_session` stays synchronous: it stops the session
  in the agent handle's shared registry, never through a request
  (`loader.rs:1118-1124`). The thread-local callbacks `LuaRuntime::new`
  sets (`runtime.rs:3033-3058`) and `publish_walks` (`:4823`) move to the
  `install` of their domain; the `HostHook` consumer and
  `api::net::clear_client_pool` stay core.
- **Requests.** `Request` keeps the core variants (`WarmJit`,
  `LoadSource`, `ClearPlugin`, `RunInitLua`, `CollectPluginOptions`, the
  five package variants, `FireAutocmd`, `EndSession`, `Shutdown`) and
  gains `Domain(Box<dyn DomainRequest>)`:

  ```rust
  pub trait DomainRequest: Send + Any {
      fn serve<'a>(self: Box<Self>, vm: &'a Vm) -> LocalBoxFuture<'a, ()>;
      fn needs_barrier(&self) -> bool { false }
  }
  ```

  Each of today's 18 domain variants becomes a struct in its domain whose
  `serve` is today's match arm. The loop awaits the future in place, as
  it awaits an arm today (`ComputeHeader`, `ComputePermissionScopes`,
  `CollectPromptSlots`, `runtime.rs:5246`, `:5255`, `:5292`), so no reload
  can start while such a callback runs; an arm that spawns today spawns
  inside `serve` on the local executor and returns at once. A domain sends on the bulk or the priority
  lane as its variant does today, so the lanes, the barrier and the
  in-flight gate do not change. `Vm` gives `serve` the Lua state, the
  local executor, the in-flight gate and the restore tracker. The test
  probe (`maki-lua/src/lib.rs:107-194`) downcasts through `Any`.
- **Task scopes.** `TaskCell` keeps its core fields and gains a
  type-keyed map of extensions, each a `TaskExt`:

  ```rust
  pub trait TaskExt: Any + Send {
      /// The share a `maki.async.run` queued under this scope takes;
      /// `inline` for a task queued inside its parent's cell.
      fn inherit(&self, inline: bool) -> Option<Box<dyn TaskExt>>;
      /// A queued task finished, before its scope drops.
      fn complete(&self, vm: &Vm) {}
      /// The scope ended, after its jobs and `Domain::end_task`.
      fn end(&mut self) {}
  }
  ```

  The bridge's `bufs`, `live` and `root_buf` and the agent's `live_sink`
  move into extensions. `PendingAsyncTask`'s `live_ctx` and `owner`
  (`runtime.rs:2397-2403`) become the map its scope's extensions
  inherit: the bridge's share is today's `LiveCtx` and a claim on the
  scope's buffers. An extension has no handle on its cell, so a claim holds
  the buffer state itself, shared with the cell, and the scope's own share
  becomes an `ended` flag that `end` sets: the buffers clear when the scope
  has ended and no claim is left, as the last claim's drop cleared them
  (`:2449-2452`), and its `complete` publishes the
  async snapshot (`:2819-2835`). An inline task takes the `LiveCtx` but no
  claim, as today (`:2044-2050`): its queue lives in the parent's cell, so
  a claim would make a cycle that keeps the buffers and their Lua
  callbacks. `TaskExt` is `Send` because the task handle goes into Lua app
  data, which mlua's `send` feature requires to be `Send`
  (`Cargo.toml:178`, `runtime.rs:1595-1607`). Ending a scope is not
  dropping its extensions, since a warm tool keeps the cell after the scope
  ends (`:2441`, `:4757-4764`): `TaskScope::drop` calls `Domain::end_task`
  after its job cleanup, in place of the direct
  `BackgroundSessions::end_task` call (`runtime.rs:1879`).
- **Docs.** `api_docs` and the docs virtual module read the composition's
  `docs()` lists, in place of a static list of every module, so a module's
  docs appear where the module is installed. Today's list interleaves the
  domains (the `maki.api` entries run tool, autocmd, action, slot), so each
  entry is a `DocEntry` with a rank, its old position, and the composed list
  is a stable sort by rank: the combined composition documents exactly what
  it does today.

### Namespaces

Core builds the `maki` global, its top-level functions and every core
namespace, then each domain extends it. A namespace two domains fill is
created once and extended by each, as `lua_table!`'s `extend` form
already does for `maki.api` (`api/tool.rs:1217`):

| Namespace | Core | Agent | Bridge | UI | Facade |
| --- | --- | --- | --- | --- | --- |
| `maki.api` | options, autocmds, slots | tools, permission rules, prompt hints, `set_prompt`, `get_tool(s)` | | commands, actions | |
| `maki.ui` | | | `buf`, `theme_*`, `highlight`, `markdown`, `humantime`, `terminal_size`, `display_width`, `truncate_text` | the rest | |
| `maki.fn` | jobs, `executable` | | | `winsaveview`, `winrestview`, `mode` | |
| `maki.keymap` | | | | all | |
| `maki.fs`, `maki.agent`, `maki.provider`, `maki.interpreter` | | all | | | |
| `maki.session`, `maki.model`, `maki.task`, `maki.plan` | | | | | all |
| the libraries | all | | | | |

The pure formatting helpers stay on `maki.ui` with the bridge, because
the tool renderers that call them run where the bridge runs, and moving
them would rename a public API.

A composition without the UI domain sets an `__index` on `maki.ui`,
`maki.api`, `maki.fn` and `maki.keymap` (installed empty) that raises
`maki.<namespace>.<name> is not available in the agent runtime: move UI
code to the plugin's ui.lua` for a missing name, so a UI call from an
agent entry fails with the call and the migration named, as the design
requires. A composition with every domain keeps today's plain tables, so a
missing name is `nil` in `maki` as before. No bundled plugin tests a
`maki` field for presence. A user's `init.lua` is not an agent entry:
the docs tell users to rebind keys there, and `maki acp` accepted such
calls before this stage and nothing read what they registered. So in a
composition without the UI domain, a config load gets a stub for each
UI name the UI domain installs (`UI_NAMES` in the core, which a test
keeps equal to the UI domain's names), logged once per name. A stub
returns an inert value that can be indexed, called, used as a table
key and turned into an empty string, and `maki.keymap.normalize`
returns its argument, so a config that binds keys, keeps a window
handle or keys a table by normalized keys still loads and its
`maki.setup` still applies; any other missing name still raises. What
a UI call returns is inert there, so a config that computes with it, a
window's size say, gets no number: such code moves to `ui.lua` with
stage 6b. Until stage 6b gives the client a `ui.lua`, that is the
compatible behavior.

### Compositions

`maki-lua` builds a host in one of two compositions, and
`PluginHost::start` takes it:

| Composition | Domains | Used by |
| --- | --- | --- |
| Combined | agent, bridge, facade, UI | `maki` (TUI and headless), `maki connect` until stage 6, `maki-docgen`, tests |
| Agent | agent, bridge, facade | `maki acp`, `maki-acp-agent` |

The UI domain's sink is the combined composition's `HostSink`; the agent
composition logs.

### Plugin entries

- **Bundled plugins.** A bundled plugin has an `init.lua`, a `ui.lua`, or
  both. The combined composition loads both as two chunks of one
  `LoadSource`, `init.lua` first, so they share one env, one `require` and
  one module cache (`runtime.rs:3477-3506`), and a failing chunk rolls
  back the whole plugin, as a failing chunk of a package does today. The
  agent composition loads only `init.lua`, and skips a plugin without one.
  A test checks that every entry of `BUNDLED_PLUGINS` but the module-only
  `lib` (`loader.rs:168-169`) has at least one of the two.
- **Options.** Options are checked per load after the last chunk
  (`runtime.rs:3380-3394`, `:3514-3516`), so a UI-only plugin's options are
  checked where it loads and nowhere else.
- **Packages and user configuration** do not change in this stage; their
  UI entries are stage 6.
- **Private modules.** `require` searches every bundled directory and
  takes the first hit (`runtime.rs:1010-1013`), so a module shared by a
  plugin's two entries gets a name no other plugin uses, as
  `memory_helpers` and `picker_rows` do. `ui.lua` itself is read by the
  loader, never required.

The bundled plugins split as follows:

| Plugin | `init.lua` keeps | `ui.lua` gets | Shared module |
| --- | --- | --- | --- |
| `todo_write` | the tool, its bridge `header`, `restore` and body, the prompt hint | the panel, `sync`, `toggle`, `<C-t>`, the `TurnEnd`, `SessionEnd` and `TaskFocusChanged` autocmds | `todo_state.lua`: the per-session lists, the focused session and an `on_write` hook |
| `memory` | the tool, write rules, prompt hints, bridge rendering | the toast, rows, views, view preference, `/memory` | `memory_helpers.lua` gains the pure `resolve_dir` family |
| `task` | the tool, `runner`, `background` | `require("picker")` | none: `picker.lua` shares no local (`plugins/task/picker.lua:1-6`) |
| `sessions`, `thinking`, `completion` | nothing: no `init.lua` | today's `init.lua`, moved | none |
| `question` | unchanged | none | none |

- **`todo_write`.** Its handler calls the panel's `sync` today
  (`plugins/todo_write/init.lua:183`). It calls `State.on_write(session,
  task, items)` instead, which `ui.lua` sets to store the list and sync
  the panel, and which is `nil` in the agent composition, so no list
  accumulates there. A `ToolDone` listener cannot replace the call: the
  event has no task id and its input needs `session_read`
  (`maki-lua/src/agent_autocmd.rs:34-42`, `:170-173`). Stage 8 replaces
  the hook with the todo data service.
- **`question`.** Its handler opens the form, an agent-to-UI call, but
  under ACP the server shadows the tool with its native elicitation tool
  or drops it (`maki-acp/src/server.rs:714-727`), so the handler never
  runs in the agent composition. The form module's top level makes no
  `maki` call, so the agent entry loads. Stage 7 moves the form.

### The agent start

A new library crate, `maki-host`, takes the root modules that start an
agent: `load_plugins`, `declared_packages`, `cli_stack`, `report_warnings`,
`sanitize_warnings` and `trust_mode` from `src/cmd/mod.rs`, `src/setup.rs`,
`src/provider_scripts.rs`, `src/project_trust.rs`, `src/cmd/acp.rs` and the
login path `--login` runs, with `update::backup_path`, the one item
`provider_scripts` reads from the self-updater (`src/provider_scripts.rs:77`).
`load_plugins` and `cli_stack` take the composition. `maki-host` exposes
`acp(AcpArgs, &HostFlags) -> Result<()>`, which runs the agent
composition, and the root's TUI and headless starts call its helpers with
the combined one. `AcpArgs` and `HostFlags` move with it, so the root
library re-exports them and its command line does not change.
`maki-acp-agent` depends on `maki-host`, `clap` and `color-eyre` and no
longer on `maki`; the shared tail `exit` moves to `maki-host` too.

### Dependency tests

`deps-check` gains `maki-lua-core`, and `maki-acp-agent`'s pending list
shrinks as the edges go:

| Package | Must not reach | Still reached after stage 5 |
| --- | --- | --- |
| `maki-lua-core` | `maki-agent`, `maki-providers`, `maki-interpreter`, `maki-highlight`, `maki-markdown`, `crossterm`, `ratatui` | none |
| `maki-acp-agent` | `maki-ui`, `maki-lua-ui`, `crossterm`, `ratatui` | `crossterm` (the UI domain's keys, until stage 6) |

## Commits

Each commit passes `just ci`, and `maki` behaves as before after each.

1. The shared items move to `maki-types` and `maki-config` with
   re-exports, and the `maki.fs` helpers that jobs read move to
   `api/util`.
2. Every core-bound file names its `maki-types` and `maki-storage` items
   by their own paths.
3. `HostSink`: the watchdog, permission warnings, unload and
   `maki.notify` reach the user through it.
4. `ProviderOrigins`, `ProviderRoute` and `ChildEnv`.
5. The `Domain` trait with `install`, `extend` and the load and teardown
   calls; the agent, bridge, UI and facade domains take their stores, their
   `LuaRuntime` fields and their parts of `create_maki_global`, `maki.api`,
   `maki.ui` and `maki.fn`.
6. `DomainRequest`: the 18 domain variants move to their domains.
7. The task scope extensions, their inheritance by queued tasks,
   `complete` and `end_task`.
8. Docs per domain.
9. `maki-lua-core` is created with `git mv` of the core modules,
   `maki-lua` depends on it, and `deps-check` covers it.
10. Compositions: `PluginHost::start` takes one, the loader loads
    `ui.lua`, and the agent composition raises on missing UI names. Its
    tests use fixture plugins, since the bundled agent entries still
    register UI until commit 13, and every start still picks the combined
    composition.
11. `todo_write` splits, and the panel tests in
    `maki-lua/tests/real_plugins_restore.rs` (`:19`, `:483-556`) load its
    `init.lua` and `ui.lua` as two chunks of one load.
12. `memory` and `task` split.
13. `sessions`, `thinking` and `completion` move to `ui.lua`.
14. `maki-host` takes the agent start, `maki acp` and `maki-acp-agent` run
    the agent composition, and `deps-check` drops `maki-ui` from
    `maki-acp-agent`'s pending list.

Commits 5 and 6 are the large ones; each may split further by domain when
the diff demands it, keeping each step green.

## Tests

Every existing test passes, apart from import paths, the
`PluginHost::start` argument and the `todo_write` loaders above; the
existing tests run the combined composition. New tests:

- every bundled plugin loads in the agent composition, and that load
  registers no command, action, keymap or window;
- in the agent composition, a call to a UI name from an agent entry
  fails with the migration message, and a package that makes one fails to
  load while the others load;
- in the combined composition, a bundled plugin's `ui.lua` sees the
  module table its `init.lua` required: the `todo_write` panel tests
  (`maki-lua/tests/real_plugins_restore.rs:483-556`) load only `init.lua`
  today and change to load both entries in one load;
- `BUNDLED_PLUGINS` holds no plugin with neither entry;
- `HostSink`, `ProviderOrigins` and `ChildEnv` defaults: a flash in the
  agent composition logs, `maki.net` refuses a provider origin when no
  origins are supplied, and a job's environment is unchanged when no
  `ChildEnv` is supplied.

## Checks

Full `just ci` after each commit, and `just deps-check` from commit 9.
Live after commits 10 and 14: `maki` with a todo list, `/memory`, `/tasks`,
`/sessions` and `/thinking` behaves as before; `maki-acp-client --
maki-acp-agent` and `maki connect -- maki acp` answer a prompt, run a
tool and show its rendered body.
