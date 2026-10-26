# Pure agent loop and agent host

Source snapshot and reading rules: [survey index](README.md).
Canonical ownership: [component boundaries](../tui-over-acp-design.md#component-boundaries).

## Current exports

[`maki-agent/src/lib.rs`](../../maki-agent/src/lib.rs) exports the core loop
alongside headless drivers, MCP, file indexing, sessions, live-session
registries, tool presentation types and shared-memory handles. The crate is
broader than the pure loop described by the target design.

| Current API | Role today | Target boundary |
| --- | --- | --- |
| `Agent`, `AgentParams`, `AgentRunParams` | Model/tool loop | Core loop with supplied services |
| `History`, `RunContext`, frame logic | Provider conversation and context | Core model and request invariants |
| `AgentHook`, `AgentSlot` | Interception at agent lifecycle points | Core interfaces, implemented by the agent host/Lua adapter |
| `Tool`, `ToolInvocation`, `ToolHook` | Tool execution and interception | Execution interfaces separated from presentation |
| `HeadlessParams`, `InteractiveHandle`, `spawn_interactive` | Frontend orchestration | Shared host behavior with frontend adapters |
| `SessionTrack`, stored sessions and claims | Session persistence | Agent host/storage boundary |
| `AgentSeed`, `SessionMailbox`, child cancellation | Live session and background ownership | Agent host services |
| `SharedMessages`, model cells and queues | In-process sharing with frontends | Internal host mechanisms, inaccessible to the ACP client |
| `SharedBuf`, `BufferSnapshot`, `SpanStyle`, `UiWaker` | Lua tool/window display | Local UI runtime or domain-neutral display support |
| MCP and provider setup | Agent services | Agent host, supplied to the core as needed |
| `FileIndex` | Shared file discovery for tools and UI | Domain-neutral service or separate local/remote adapters |

The final crate layout can differ from this table. The dependency direction
cannot: UI support must not pull in the core loop or provider execution.
Core interfaces must not require the UI runtime, ACP framing or a Lua VM.

## Meaning of a pure loop

The loop still performs useful work by calling model, tool and hook
interfaces. It owns iteration and conversation transitions. Purity here is
an architectural boundary, not a promise of mathematically pure functions.
Transport listening, plugin loading, UI interaction, session lookup,
credential setup and durable saves belong to the host.

The host supplies the registered tools and agent hooks. Agent Lua implements
those interfaces through an adapter. This avoids a dependency from
`maki-agent` back to its Lua implementation. The current
[`SlotHook`](../../maki-lua/src/hook.rs) already demonstrates this direction.

[`AgentHook`](../../maki-agent/src/agent/hook.rs) and
[`ToolHook`](../../maki-agent/src/tools/hook.rs) are useful core boundaries.
Tool input interception runs before permission resolution. Output
interception happens before the model receives the result. Moving display
code must not change those semantics or their authority requirements.

## Consolidation needed around the loop

The TUI driver in
[`agent_loop.rs`](../../maki-ui/src/agent/agent_loop.rs) and drivers in
[`headless.rs`](../../maki-agent/src/headless.rs) repeat context setup,
agent construction, model handling and run management. They also differ in
queue, compaction, background seed, hooks and persistence support.

The target has one host definition of session behavior. ACP exposes that
behavior to the TUI and other clients. Print and SDK adapters preserve their
external contracts while using the same scheduling and lifecycle rules.
Internal child agents can invoke the core directly inside the host with the
same permission and ownership services.

## Result API separation

The current tool registry interfaces reference `BufferSnapshot`, and tool
outputs include both semantic content and presentation support. A pure
execution interface needs data and progress, not UI buffers or callbacks.

Tool schemas and policy remain agent-side. A UI renderer consumes a result
contract and cannot alter the stored model output. Default rendering and
replay must remain possible without a matching UI package. See
[the Lua survey](03-tui-components-and-lua.md) for mixed registrations.

## Lifecycle services

The host must support accepted queues, task ownership and pending asks
without a TUI loop. Publishing live-generation state only from the TUI is
insufficient for background agent plugins. Cancellation and interaction
routing belong to the owner that can validate the session, task, run and
generation, not to the displayed row.

Host persistence must include all semantic state currently saved by the
TUI. Moving to `SessionTrack` without checking tool data, child histories,
usage, plans, permissions and queues would lose behavior.
