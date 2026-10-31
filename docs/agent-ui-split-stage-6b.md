# Agent and UI split: stage 6b, UI packages and configuration

Status: proposed. The second part of stage 6 of the [implementation
design](agent-ui-split-implementation.md), split off by the [stage 6
specification](agent-ui-split-stage-6.md): packages gain a UI entry and a
UI component with its own permissions, approval, load policy and
enablement, the user gains a `ui.lua`, and the client runs both and
manages its own UI packages, so a package's UI code runs where the UI
runs and is approved there. `maki` loads everything it loads today.

## Today

- A package's entry is its sorted `plugin/*.lua` files
  (`maki-lua/src/loader.rs:266`, `package_entrypoints`), loaded as the
  chunks of one load with a third-party agent grant
  (`load_package_with_guard`); a declared custom loader replaces that
  discovery with a function (`loader.rs:797`, `runtime.rs:263`). A package
  that calls a UI API there fails to load under `maki acp`, which the
  project owner chose to keep, and the error tells the author to move the
  code to "the plugin's ui.lua" (`maki-lua-core/src/api/mod.rs:73`), a
  file the package loader does not read.
- A manifest's `[permissions]` table and its `net_hosts`, and the
  top-level `providers`, give what the package asks for
  (`maki-lua-core/src/plugin_permissions.rs:512`, `Requested::from_manifest`).
  Bundled plugins read the same table (`BundledPlugin::permissions`); the
  UI-only `sessions`, `thinking` and `completion` ship only a `ui.lua`
  under a top-level `[permissions]` table.
- Approvals live in `<site>/pack-approvals.json`, one `Entry` per package
  name bound to one `src` (`maki-pack/src/approvals.rs:17-90`; `src` is a
  required field). A managed package whose required grants are missing is
  skipped at startup (`maki-lua-core/src/pack.rs:529`) and in the read-only
  fallback (`:357`); `effective_grant` (`:184`) and `missing_grants`
  (`:380`) compute the grant. A hand-installed package is found by
  discovery (`pack.rs:81`, `loader.rs:946`) with no fetched source or
  declaration. The install prompt, the update review and `/packdel` read
  and write one entry per package; `/packdel` refuses a declared package
  (`pack.rs:1158`) and removes the whole checkout and lock entry
  (`maki-pack/src/manager.rs:413`). Options register once per plugin and
  validate the whole map (`maki-lua-core/src/api/options.rs:204`, `:280`),
  and `packadd` skips a package already loaded (`loader.rs:1065`).
- Managed packages come from `maki.pack.add` in the global `init.lua` only
  (`maki-lua-core/src/runtime.rs:2697`). A second declaration of the same
  name is ignored when its spec is identical, including its `load`,
  `confirm` and `data`, and fails when it differs
  (`maki-lua-core/src/api/pack.rs:196-212`).
- The user's configuration is `init.lua`, global and project
  (`maki-lua/src/loader.rs:471-657`), whose `RawConfig` merges with one
  `enabled` flag and one option map per plugin
  (`maki-config/src/lib.rs:584`). The project file is gated by project
  trust, which checks every gated file kind
  (`maki-config/src/project.rs:137`, `:226`, `:370`).
- The client runs no configuration file and no package, roots its project
  config in a scratch store (`maki-acp-client/src/connect.rs:97`), treats a
  reload as the end of the run (`:114`), and refuses package commands on a
  remote tab (`maki-ui/src/agent/capabilities.rs:121`).

## Design

### Components

A package has an agent component and a UI component. Each component has
its own entry, requested permissions, approval, load policy, options and
enablement, and a host acts only on the components it loads: the agent
composition on the agent one, the UI composition on the UI one, the
combined composition on both.

### Entries

- **Directories.** The agent entry is the sorted `plugin/*.lua`, the UI
  entry the sorted `ui/*.lua`. `package_entrypoints` takes the entry
  directory and moves to the core, with today's symlink containment check
  for each directory.
- **Custom loaders.** A custom loader is an entry source for the component
  of the file that declares it: one declared in `init.lua` is the agent
  entry, one in `ui.lua` the UI entry; a component without a declared
  loader uses its directory. A package may have no entry directory when
  its loader supplies the component.
- **One load.** The combined composition runs the eligible components'
  entries as the steps of one load, agent first, in one env and one module
  cache, so a failure in any step rolls back the whole package, as a
  failing chunk does today, and `min_maki_version` is checked once per
  package. The agent and the UI composition run only their own component.
  A package none of whose components this host loads is skipped; one with
  no entry at all fails as an empty package does today.
- **Eligibility.** Each component passes exactly the gate a package
  passes today, against its own approval: a managed component whose
  required grants are missing is skipped, and a hand-installed one follows
  today's discovery rule. On top of that, the UI entry is new code, so it
  runs only when the host holds a UI approval record for the package,
  even an empty one; an agent approval never makes a UI entry eligible.
  This applies to fetched packages. A hand-installed package has no
  fetched source and today grants its manifest directly, without
  approvals (`pack.rs:200`), because the user placed all its code by hand;
  its UI component is granted its `[ui]` table the same way, and moving or
  replacing it on disk is the same act of trust as today.
- **Refusal in the combined composition.** Refusing a package's agent
  component skips the whole package there, as refusing it does today, so
  `maki` loads what it loads today; refusing only its UI component loads
  the agent entry alone and warns that the UI approval is required.
- **Activation in the combined composition.** A package is one load there:
  it is eager when any of its declared, enabled components is eager, and
  an activation by `packadd` from either file loads it with every eligible
  component. A component never joins an env that is already loaded, so
  no registration is cleared and no component runs twice. The agent and
  the UI composition activate their own component as today.

### Manifests

- **`[ui]`.** The UI component's requests are a `[ui]` table read by the
  same parser: `[ui.permissions]` with its `net_hosts`; `providers` has no
  UI meaning and `[ui]` does not take it. A package has a UI component
  only when its manifest has a `[ui]` table, even an empty one, so the
  question whether to approve it is always decided from the manifest; a
  `ui/` entry without a `[ui]` table does not load, and the host warns.
- **Bundled plugins.** The same split holds: `todo_write`, `memory` and
  `task` gain the `[ui]` tables their `ui.lua` needs; `sessions`,
  `thinking` and `completion` move their table under `[ui]`. Bundled UI
  entries already run (`maki-lua-ui/src/ui_host.rs:127`), so the bundled
  grant reader (`maki-lua-core/src/bundled.rs:40`) becomes
  composition-aware in the same commit: the agent composition reads the
  top-level table, the UI composition `[ui]`, the combined one their union.
  The test that checks bundled manifests against the API they call checks
  each entry against its own component.
- **The combined grant.** The combined composition gives the one env of a
  package the union of the grants of its eligible components. The two
  entries share their env and module cache there, as they share them
  today, so one reach for the load is the only honest grant, and the
  eligibility rule keeps an unapproved UI entry out of it. The agent and
  the UI composition grant only their own component. This ends with the
  combined composition in stage 10.

### Approvals

- **Store.** UI approvals live in a separate file in the site,
  `pack-ui-approvals.json`, with today's `Entry` shape: each bound to its
  own `src`, its absence no approval, an entry with empty lists an
  approved empty grant. The existing store keeps the agent approvals
  unchanged, so every existing approval is an agent approval and an older
  build never reads a UI approval at all.
- **Sources.** Each component's approval is bound to its own source: a
  package whose source changes loses each component's approval separately
  and asks again, and approving one component never writes, moves or
  keeps the other's.
- **Prompts and reviews.** The install prompt asks for each component that
  needs approval, the update review shows a diff per component, and
  `/packdel` revokes both. A host asks only for the components it loads:
  the client never asks to approve agent code, and `maki acp` never asks
  to approve UI code.
- **Older builds.** An older build neither reads nor writes the UI file,
  so it cannot drop or misread a UI approval, and it runs no UI entry.

### Declarations

- **Where.** The global `ui.lua` may declare packages with `maki.pack.add`,
  as the global `init.lua` may. Declarations resolve against the lockfile
  and site of the environment that runs the file, so a client on another
  machine installs the UI packages it declares in its own site.
- **Load policy.** A declaration in `init.lua` sets the agent component's
  `load`, `confirm` and `data`; one in `ui.lua` sets the UI component's.
  Both files must name the same repository and revision for a package;
  a different one fails as today. A component no file declares does not
  load. `maki.packadd(name)` activates the components the calling runtime
  loads, so in the combined composition an activation from either file
  activates both declared components.

### Configuration

- **Files.** `ui.lua` sits next to `init.lua`: the first in the config
  search directories, and a project one gated by project trust where the
  UI runs (a new `GatedFile::UiLua`). Each runs once, as
  `ConfigScope::UiGlobal` or `UiProject`, with the config directory's
  `plugin.toml` `[ui]` table as its grant.
- **Sections.** `maki.setup` in `ui.lua` takes the sections the client
  reads: `ui`, `plugins.<name>` for the UI component, `net` for the UI
  runtime's `maki.net`, and `storage.input_history_size`. A section only
  the agent reads warns and is ignored.
- **Enablement.** `plugins.<name>.enabled` in `init.lua` keeps acting on
  the whole plugin, as today, so an existing `plugins.sessions.enabled =
  false` still disables `/sessions`; the same key in `ui.lua` overrides it
  for the UI component only. In the combined composition, a disabled
  component's entry does not load while the other's does.
- **Options.** Options are scoped to a component: an entry's
  `register_options` registers that component's schema, once per
  component. `init.lua`'s `plugins.<name>` keys go to each component whose
  schema declares them, as one plugin's keys do today; `ui.lua`'s keys go
  to the UI component and override per key. A key no loaded component
  declares is unknown, and the check runs only where every component of
  the plugin loads: the agent and the UI composition check the keys their
  own component declares and leave the rest, so an unchanged `init.lua`
  with UI keys still loads under `maki acp`. The schema store and docgen
  keep the two schemas apart. The runtime knows each chunk's component,
  since the loader tags every step of a load with it.
- **Network.** `net.allowed_private_hosts` is the union of both files in
  the combined composition, which has one process-wide value.
- **Compositions.** The combined composition runs `init.lua`, then
  `ui.lua`, each once, into its one VM. The agent composition runs
  `init.lua` only, its UI calls stubbed as today; the UI composition runs
  `ui.lua` only, with the agent namespaces guarded. Until a user writes a
  `ui.lua`, `maki` behaves as today.

### Trust

- **Gates per runtime.** A runtime asks about, and is held back by, only
  the gated files it executes: `maki acp` the agent's, the client
  `ui.lua`, the TUI both. A new `ui.lua` in a trusted folder therefore
  never makes `maki acp` treat its trusted `init.lua` as unanswered, and
  the TUI asks once about the new file before it runs it.
- **The client's project.** The client finds its project from the
  directory it was started in, not from the remote `--cwd`, and keeps its
  trust answers in its own durable state, not in the scratch store. A
  project `ui.lua` whose trust is unanswered does not run; the client asks
  as the TUI does.

### The UI host

- **Loading.** `UiHost` gains the host steps `PluginHost` has today: run
  the user's `ui.lua`, discover installed packages, resolve the declared
  ones, grant and gate each package's UI component, load the UI entries in
  `packadd` rounds, and report warnings. The neutral steps move to the
  core as a host module over a component policy, which `PluginHost`
  implements for the agent and combined compositions and `UiHost` for the
  UI composition, so both hosts load packages the same way.
- **Ownership.** A lockfile entry gains an `owners` list naming the
  components that installed or declared the package in that site
  (`#[serde(default)]`); an entry without it, written before this stage or
  by an older build, reads as owned by the agent. Hosts add their
  component when they install or resolve a declaration, so ownership is
  durable without running the other side's configuration.
- **Management.** The client runs `/packupdate` locally for the packages
  its `ui.lua` declares and `/packdel` for the UI packages it installed,
  and reloads its UI host after a change, keeping the session and the
  link; a reload no longer ends the run. A UI-only operation never changes
  agent state: the client updates or removes a checkout and its lock entry
  only when the entry's owners are the UI alone; otherwise an update is
  left to a host that loads the agent component, which reviews both, and a
  delete revokes only the UI approval and removes the UI owner. `/packdel`
  keeps today's rule that a declared package is not deleted: a UI package
  is deleted after its declaration leaves `ui.lua`, found by its owner
  record and UI approval. Package commands that act on the agent's
  packages stay refused on a remote tab.
- **`--no-plugins`** skips `ui.lua` and packages as it skips the bundled
  plugins.
- **Migration message.** A package's UI call in the agent runtime names
  the package's `ui/` entry as the place to move the code; a bundled
  plugin's names its `ui.lua`.

## Commits

Each commit passes `just ci`, and `maki` behaves as before after each.

1. The neutral host steps move to the core over a component policy;
   `PluginHost` uses them.
2. Components in manifests: `[ui]` in `Requested` and `DiscoveredPackage`,
   the bundled manifests split with the composition-aware bundled grant;
   no package UI entry loads yet.
3. Approvals per component: the UI approval file, prompts, reviews and
   revocation, and eligibility, including hand-installed packages.
4. Entries: `package_entrypoints` per directory, custom loaders per
   component, the eligible components as the steps of one load with the
   union grant, combined activation, component-scoped options, the
   per-component migration message.
5. Declarations and configuration: `ui.lua` scopes, search and sections,
   per-component load policy, enablement and options, and the trust gates
   per runtime.
6. `UiHost` runs `ui.lua` and the UI entries of discovered and declared
   packages; the client starts with them, from its local project and
   trust state.
7. Client package management: lockfile owners, local `/packupdate` and
   `/packdel` for UI packages, and the UI host reload.
8. Docs: packages (entries, `[ui]`, approvals, declarations),
   configuration (`ui.lua`), the CLI's `maki connect` and the ACP page's
   note on packages.

## Tests

- A fixture package with `plugin/` and `ui/`: the combined composition
  loads both as one load sharing a module once both components are
  approved, and only `plugin/` while the UI component is unapproved; the
  agent composition loads `plugin/` only, the UI composition `ui/` only;
  a package with only `ui/` is skipped by the agent composition, one with
  only `plugin/` by the UI one; custom loaders supply either component.
- A manifest with `[ui.permissions]` grants the UI entry only what is
  approved for it; the combined grant is the union of the eligible
  components; an agent approval alone never loads a UI entry.
- An approval store from before this stage is the agent store,
  unchanged; approving the UI component writes only the UI file; revoking
  one leaves the other; a source change drops each component's approval;
  hand-installed `start` and `opt` packages load their agent component as
  today and their UI component with its `[ui]` table granted directly.
- `plugins.sessions.enabled = false` in `init.lua` still disables
  `/sessions`; `ui.lua` overriding it re-enables only the UI component;
  an agent schema and a UI schema of one package each validate only their
  own keys; a package eager in one file and dormant in the other loads
  once with both components in the combined composition.
- A client `/packdel` of a package the agent also owns revokes the UI
  approval only and keeps the checkout; one the client alone installed is
  deleted after its declaration leaves `ui.lua`; a lock entry without
  owners is treated as the agent's.
- Disjoint agent and UI option schemas of one package: an unchanged
  `init.lua` that sets agent keys loads in every composition, a UI key in
  it reaches the UI component, and a key neither declares fails only in
  the combined composition.
- A hand-installed package with a `ui/` entry loads it with its `[ui]`
  grant and no approval prompt.
- Declarations in both files keep each component's load policy; a
  differing repository fails; `packadd` activates the declared components.
- `ui.lua` sets `ui` and `net` options, warns on an agent-only section,
  disables a package's UI component while its agent component still loads,
  and declares a package the UI host then loads; in the combined
  composition it runs after `init.lua` and its option values win, with a
  warning on a conflict.
- A trusted folder that gains a `ui.lua` keeps `maki acp` running its
  `init.lua`; the TUI asks about the new file once.
- A package's UI call under `maki acp` names its `ui/` entry.
- Every bundled plugin loads in each composition with its manifest's
  component.

## Checks

Full `just ci` after each commit. Live after commits 4, 6 and 7: `maki`
with the user's existing `init.lua` and packages behaves as before; a
fixture package with a `ui/` entry asks for its UI component once and runs
its command in `maki` and under `maki connect -- maki acp`; `maki acp`
loads its `plugin/` entry without asking about its UI; and `/packupdate`
in the client updates the UI component and reloads the UI host without
dropping the session.
