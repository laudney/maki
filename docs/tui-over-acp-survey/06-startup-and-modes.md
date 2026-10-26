# Startup, transports and run modes

Source snapshot and reading rules: [survey index](README.md).
Target: [component boundaries](../tui-over-acp-design.md#component-boundaries)
and [configuration](../tui-over-acp-design.md#files-configuration-and-compatibility).

## Current launch paths

| Mode | Source | Present arrangement |
| --- | --- | --- |
| TUI | [`cmd/tui.rs`](../../src/cmd/tui.rs), `build_stack`, `run` | Starts one plugin host and passes its readers/handles into `maki-ui` |
| ACP | [`cmd/acp.rs`](../../src/cmd/acp.rs), `run` | Loads agent/tool plugins with `Interaction::None`, then starts stdio ACP |
| Print | [`print.rs`](../../src/print.rs) | Drives the headless path and relays agent events to Lua |
| SDK | [`sdk_mode.rs`](../../src/sdk_mode.rs) | Uses the interactive headless path and its external stream format |
| Utility commands | [`cmd/mod.rs`](../../src/cmd/mod.rs), `cli_stack` | Loads plugins for model/provider and other non-session operations |
| Shared setup | `load_plugins` in `cmd/mod.rs` | Discovers packages, evaluates init files, loads builtins and commits provider registration |

`Interaction` currently combines installation-prompt policy with availability
of a UI action channel. The target separates configuration/install behavior,
agent interaction capabilities and creation of a local UI runtime.

## Target launch models

### Local UI and agent in one process

The launcher creates an agent host, its agent Lua runtime, an ACP server,
an ACP client and a UI runtime. The client and server can use an in-memory
transport. The two Lua states and API sets remain disjoint.

The launcher controls local process lifetime. It does not pass live agent
handles to the UI or run a shared mixed init file in both runtimes.

### Local UI and child agent

The UI connects over stdio. Agent configuration, tools, provider credentials
and workspace context are loaded by the child host. UI packages, theme,
input and terminal handling are loaded by the client.

A launcher can prepare configuration for both environments, but tool
execution and agent hooks must not require the parent's UI runtime.
Diagnostics use a channel that does not corrupt ACP stdout or terminal
rendering. Child shutdown and pending operation outcomes are explicit.

### Remote agent

The client connects to a configured endpoint over a supported remote
transport. HTTP and WebSocket are intended adapters, not implemented modes
at the surveyed snapshot. Authentication, session authorization and workspace
access policy must exist before a network listener is exposed.

The agent resolves its files, environment, packages and credentials on its
own machine. The client resolves local UI configuration and packages on the
client machine. Data needed by the UI, including remote file matches, images,
plans or memory content, comes through an advertised service.

The agent cannot cause executable UI plugin code to load by sending result
metadata. A renderer is selected from locally installed and approved code.

### Headless clients

Print, SDK and third-party ACP clients use agent services without creating a
UI Lua runtime. An unsupported interaction settles as unavailable or follows
an explicit headless policy. Tools must not wait on an absent window channel.

SDK compatibility can be an adapter over the shared host. The design does
not require changing an existing public SDK format to ACP in one release.
It does require one definition of agent session behavior.

## Configuration and packages

Agent and UI domains need distinct entry points and capability checks. An
existing package may contain both components. Loading one does not imply
loading the other. Each environment owns its package revisions and grants.
Exact manifest and configuration syntax belongs to implementation design.

A remote agent's trusted project does not authorize local UI Lua. Local UI
trust does not approve remote project code. Existing package checks and
lockfile protections remain applicable independently in each environment.

A combined `init.lua` migration must classify declarations by owner rather
than execute arbitrary configuration code twice. Mixed third-party plugins
need an explicit migration path and useful diagnostics for APIs unavailable
in their selected domain.

## Reload and connection lifetime

UI reload recreates UI hooks, renderers and local state without changing the
agent tool set. Agent reload follows host lifecycle and request-frame rules.
It cannot mutate tools or system content earlier in an active request frame.

Closing a tab, disconnecting a client, stopping a session and stopping a
locally owned agent process are different operations. The connection policy
states what happens to foreground work on disconnect. Background work stays
under its host owner until that owner's lifetime ends.

A reconnect obtains authoritative state. The UI must not retry a prompt just
because its previous response was lost. A transport does not imply durable
reattachment, exclusive access or safe operation replay.

## Local and remote actions

A local shell command, clipboard action or editor command is identified as
client-local. Agent tool calls run in the agent environment. Content moved
between the environments is explicit and follows access policy. A remote
path is never assumed to identify an equivalent local file.

Agent workspace context is explicit through tools and hooks. Changes to cwd
must state their session scope and cannot rely on the client process cwd.
The exact migration of current process-wide `/cd` behavior needs a deliberate
compatibility decision when that operation is implemented.
