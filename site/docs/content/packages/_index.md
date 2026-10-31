+++
title = "Lua Packages"
weight = 11
[extra]
group = "Reference"
+++

# Lua packages

A Lua package lets you add tools, commands, keybindings, and event handlers
without copying its code into `init.lua`. Maki can load a package that you put
on disk, or it can install one from a Git repository and lock it to one commit.

A package directory holds a `plugin.toml` manifest, modules at
`lua/<module>.lua` or `lua/<module>/init.lua`, and two kinds of entry files:

- `plugin/*.lua`, the agent entry, for tools, hooks, and other code that runs
  with the agent.
- `ui/*.lua`, the UI entry, for keybindings, commands, and other code that runs
  with the UI. It loads only when `plugin.toml` has a [`[ui]` table](#ui-permissions).
  Without one, Maki skips the entry and warns.

Each entry runs its files in sorted order and uses the API the
[plugin guide](/docs/plugins/) describes. `maki` loads both entries as one
plugin, so they share one environment and module cache. A global set in one
file is visible to the others. Built-in globals such as `string` or `print`
stay fixed, so defining your own `print` in one file does not change it in
another.

`maki acp` loads only the agent entry, and
[`maki connect`](/docs/cli/#maki-connect) loads only the UI entry. Code in one
entry must not depend on a global the other entry sets.

## Install from Git

Declare managed packages in the global `init.lua`, normally
`~/.config/maki/init.lua`:

```lua
maki.pack.add({
  "https://github.com/example/maki-goal",
  {
    src = "https://github.com/example/maki-review",
    version = "v1.2.0",
  },
})
```

Each entry is a source string, or a table with `src`, `version`, `name`, and
`data`. Maki derives the directory and owner name from `src`, and `name`
overrides it when two sources end in the same repository name.

Maki shows all new packages in one install prompt, on the terminal, before the
UI starts. It writes the selected Git commit to `pack-lock.json` in your config
directory. Commit this file if you want another machine to install the same
revisions.

Every project shares one lockfile and package directory, so a project
`.maki/init.lua` or `.maki/ui.lua` cannot add packages. It can still read state
with `maki.pack.get` and activate a package with `maki.packadd`.

Maki refuses a package name that matches a builtin plugin or a package you
placed by hand, and reports the conflict at startup.

Set `confirm = false` only when the package source is already trusted and Maki
must run without a terminal:

```lua
maki.pack.add({ "https://github.com/example/maki-goal" }, {
  confirm = false,
})
```

This option skips the install prompt. It does not approve package permissions.
Maki rejects an HTTP source carrying a username, password, or token, since Git
and the lockfile would store it. Use a credential helper or an SSH agent.

Set `load = false` to install a package without loading it at startup. Set
`load` to a function when the package needs a custom entry point:

```lua
maki.pack.add({
  {
    src = "https://github.com/example/maki-review",
    data = { module = "review" },
  },
}, {
  load = function(package)
    require(package.spec.data.module).setup()
  end,
})
```

The function runs as the package owner. It receives the package `spec` and its
installed `path`. The `data` field can contain any Lua value.

See [`maki.pack.add`](/docs/lua-api/#maki-pack-add) and
[`maki.pack.get`](/docs/lua-api/#maki-pack-get) for the full signatures.

## Declare the UI entry

A declaration in `init.lua` loads the agent entry of a managed package. To load
its UI entry, declare the package in the global
[`ui.lua`](/docs/configuration/#ui-lua) too, normally `~/.config/maki/ui.lua`:

```lua
maki.pack.add({ "https://github.com/example/maki-review" })
```

Each file sets `load`, `confirm`, and `data` for its own entry. When both files
declare a package, they must give the same `src`, `name`, and `version`, or the
second declaration fails. An entry that no file declares does not load.

Maki installs a package on the machine that runs the file. When `maki connect`
runs on another machine than the agent, it installs the packages its `ui.lua`
declares on its own machine, with its own `pack-lock.json`.

## Pinned revisions

A lockfile entry wins over `version`: once Maki records a commit, it installs
that commit everywhere, and a later `version` in `init.lua` changes nothing. To
move a package, delete its entry from `pack-lock.json` and
start Maki again. Maki resolves `version` and records the commit it picked.

A changed `src` makes the recorded revision meaningless, so Maki installs the
new source and records it. That is also a new trust decision, so the install
and permission prompts come back.

Removing a `maki.pack.add` entry stops the package from loading. Its lockfile
entry and its checkout stay on disk until you delete them.

## Update packages

Run `/packupdate` to update every installed package that the global config
still declares. Pass one package name to update only that package:

```text
/packupdate maki-review
```

Maki fetches the source and shows the old and proposed commits. It compares the
proposed permission request of each entry with the permissions already approved
for that entry, package, and source. Accepting the review applies the revision and approves the
permissions it showed. Declining leaves the installed revision alone.

Use `/packupdate!` to skip the update review. The bang does not cover
permissions, so Maki still asks about a new one when it loads the updated
package.

Pass `++lockfile` to restore the commit already recorded in the lockfile
instead of resolving the declared version:

```text
/packupdate ++lockfile maki-review
```

Project config and packages cannot update global package state.

## Remove packages

First remove the package declarations from the global `init.lua` and `ui.lua`
and reload.
Then remove the inactive package:

```text
/packdel maki-review
```

`/packdel ++all` removes every installed package that is no longer declared.
Maki shows what it is about to remove and waits for you to accept. A package
that is still active, here or in another Maki process, is refused. The approvals
of both entries go away once its files are gone.

Use `/packdel!` to skip that review. The refusals still apply.

## Packages in `maki connect`

In [`maki connect`](/docs/cli/#maki-connect), `/packupdate` and `/packdel` act
on the UI packages of the client's machine. `pack-lock.json` records whether the
agent, the UI, or both installed each package, and the client changes only what
the UI alone installed. For a package the agent also uses, `/packupdate` in the
client refuses and asks you to update it where the agent loads, which reviews
both entries. `/packdel` in the client removes only its UI approval and keeps
the checkout.

## Package permissions

A managed package can ask for guarded APIs in `plugin.toml`:

```toml
[permissions]
fs_read = true
fs_write = true
net = true
run = true
env = true
agent = true
session_read = true
session_write = true
session_settings = true
```

The manifest states what the package wants, and Maki asks about new permissions
in a separate prompt. A package with no `plugin.toml` asks for nothing, and
every guarded call it makes fails. The
[permission list](/docs/lua-api/#plugin-permissions) covers what each name
gates.

Maki stores a package's [`net_hosts`](/docs/permissions/#plugin-egress-net-hosts)
with its approval. An update that widens the list or drops it asks again before
it loads. An approval from before Maki stored hosts counts as every host, so
adding a list to that package never prompts.

A package that registers provider slugs lists them at the top level of
`plugin.toml`:

```toml
providers = ["greenpt"]
```

Maki stores the names with the approval, and the install prompt says the
package will receive the saved key for each name models.dev serves. An update
that adds a name asks again. A load that registers a slug models.dev serves
and the approval never covered is refused. List every slug the package
registers, so a name models.dev adds later is already approved and the
catalog client can never take over a working package's slug.

An approval applies only to the same package name and source. Maki keeps
approvals in `<maki-data>/site/pack-approvals.json`, where `<maki-data>` is the
data directory from the
[directory layout](/docs/configuration/#directory-layout). Approvals describe
trust on this machine and must not be committed with `pack-lock.json`.

Only the interactive UI can ask. `--print`, SDK mode, the ACP server, and the
other subcommands never prompt, so a package waiting for a decision comes back
as a startup warning instead of loading.

### UI permissions

The UI entry asks for permissions in a `[ui]` table, which takes
`[ui.permissions]` and its `net_hosts`. It has no `providers`, since only the
agent entry registers providers:

```toml
[permissions]
fs_read = true

[ui.permissions]
net = true
net_hosts = ["api.example.com"]
```

A UI entry that needs no permission still needs an empty `[ui]` table.

The UI entry is new code, so Maki approves it on its own. It asks about the UI
entry after the agent entry, and an agent approval never approves UI code.
Refusing the agent entry skips the whole package. Refusing only the UI entry
loads the agent entry and shows a warning that the UI approval is required. UI
approvals live in `<maki-data>/site/pack-ui-approvals.json`, beside the agent
approvals. `maki connect` asks about UI entries only, and `maki acp` never
loads one.

## Upgrading plugins

A plugin written for an older Maki may need these changes:

- JSON null now decodes to [`maki.NIL`](/docs/lua-api/#maki-NIL), which is truthy. Pass `{ luanil = { object = true, array = true } }` to [`maki.json.decode`](/docs/lua-api/#maki-json-decode) to get nil back.
- Messages from [`maki.session.messages`](/docs/lua-api/#maki-session-messages) changed shape. `hidden` is gone, and `display_text = ""` now marks a hidden message. A thinking block keeps its text in `thinking` instead of `text`. An image block has a `source`, which is a [`maki.image.Image`](/docs/lua-api/#maki-image-Image), instead of `media_type`.
- Under `maki acp`, a `plugin/*.lua` file that calls a UI API such as `maki.keymap.set` fails at that call. Move that code to a `ui/*.lua` entry and add a [`[ui]` table](#ui-permissions). See [ACP](/docs/acp/#config-and-packages).
- A package that calls session APIs or layers a tool or agent slot must list the `session_read`, `session_write` and `session_settings` keys it uses in its `plugin.toml`. The [session permissions](/docs/permissions/#session-permissions) and the [layer price table](/docs/hooks/#rules) say which. A `plugin.toml` you wrote yourself grants every key it does not list. An `init.lua` without a `plugin.toml` is denied every guarded call, the session APIs included.

## Install by hand

Clone a package into a Neovim-style package directory:

```text
<maki-data>/site/pack/<group>/start/<name>/
<maki-data>/site/pack/<group>/opt/<name>/
```

Pick any `<group>` name except `core`, which Maki reserves for its own installs
and never scans.

A `start` package loads at startup. An `opt` package stays installed until
something activates it.

Placing a package by hand means you trust its code the way you trust your own
`init.lua`. Maki grants it exactly the permissions its `plugin.toml` requests,
with no approval step. The same holds for its UI entry and the `[ui]` table.

## Activate an installed package

An `opt` directory, or a managed package declared with `load = false`, starts
with `maki.packadd`. Call it from `init.lua` or from another package:

```lua
maki.packadd("my-package")
```

Maki loads the named package after the calling Lua task returns. A package
that `maki.packadd` activates can activate another one in turn. Maki reports a
name that no installed package matches, and refuses a package that
`plugins.<name>.enabled = false` disabled.

## Managed checkouts

Maki restores a missing checkout only when the global config still declares the
package and its source matches the lockfile entry. Installs and approval writes
share one file lock, so two Maki processes cannot interleave them. The kernel
releases that lock when the process exits, so a crash leaves nothing to clean
up.

Each revision gets its own directory under
`<maki-data>/site/pack/core/<name>/<commit>/`. At startup Maki deletes stale
revisions, skipping any that a running process still holds.
