# TUI over ACP: source survey

This survey records the current source and the boundaries that must change.
The [design](../tui-over-acp-design.md) is the canonical target: a pure
`maki-agent` loop behind an ACP service, an ACP client in `maki-ui`, and two
disjoint Lua runtimes for agent and UI plugins.

Source snapshot: `c7554a36`, inspected on 2026-10-26. Source links and symbol
names are navigation aids. Current shared-memory arrangements describe the
starting point, not requirements for the new protocol.

The former conclusion that all Lua must remain on the agent side is
withdrawn. Mixed execution and presentation in today's plugins requires an
API split. It does not require remote windows or shared plugin state.

## Documents

| Document | Question |
| --- | --- |
| [01: TUI and agent coupling](01-tui-agent-glue.md) | Which session and loop responsibilities are currently in the UI? |
| [02: Application state](02-tui-app-state.md) | Which state belongs to the agent host and which belongs to the client? |
| [03: Lua and presentation](03-tui-components-and-lua.md) | How do plugins and hooks divide between the runtimes? |
| [04: ACP adapter](04-acp-server.md) | What does the existing adapter carry and where does it lose behavior? |
| [05: Core and host](05-agent-engine-api.md) | What belongs in the pure loop and what belongs in its host? |
| [06: Startup and modes](06-startup-and-modes.md) | How do local, remote and headless clients load their own domains? |
| [07: Protocol constraints](07-acp-schema-and-upstream-guidance.md) | Which source and project constraints guide the boundary? |
| [08: Feature inventory](08-gap-analysis.md) | What remains local, moves to the host or requires protocol data? |

Start with the design. Use document 03 for the plugin split and document 08
for feature coverage. The [source index](../tui-over-acp-implementation.md)
keeps code-level references without an implementation specification.

## Evidence limits

This is a repository inspection, not a live interoperability test. It does
not establish the latest ACP release or promise transport support that the
code does not implement. The pinned schema and current adapter are described
in documents 04 and 07. New wire details need a separate check when they are
designed. HTTP and WebSocket are architecture targets, not existing features.
