# Application state ownership

Source snapshot and reading rules: [survey index](README.md).
Target rules: [state and lifecycle](../tui-over-acp-design.md#state-and-lifecycle).

## Current source

[`App`](../../maki-ui/src/app/mod.rs) combines display state with agent
session data. [`event_loop.rs`](../../maki-ui/src/event_loop.rs) starts
agents, handles Lua UI actions, performs model/provider work and coordinates
shutdown. [`app/session.rs`](../../maki-ui/src/app/session.rs) checkpoints
shared history into a stored session and submits it to
[`StorageWriter`](../../maki-ui/src/storage_writer.rs).

The current UI owns persistence for the TUI path. This includes more than
messages: structured tool output, child histories, usage, plan metadata,
permission state, drafts and queued messages. The headless
[`SessionTrack`](../../maki-agent/src/session.rs) path is not a complete
replacement for those writes.

## Target owners

| State | Target owner | What the other side receives |
| --- | --- | --- |
| Conversation and request frames | Agent host and core loop | Published history and display events |
| Accepted queue items and running requests | Agent host | Queue/run snapshots and lifecycle events |
| Tool input, output, saved semantic state and child history | Agent host | Replay data and live result updates |
| Model, thinking, fast, workflow and mode | Agent host | Capabilities, current settings and confirmed changes |
| Usage and costs | Agent host | Published counters and breakdowns |
| Plans and workspace paths | Agent host | Plan content, status and defined operations |
| Permissions, trust and credentials | Owning agent environment | Relevant status and scoped interaction requests |
| Agent MCP and package state | Agent host | Advertised services and administration results |
| Tabs and session/task focus | UI | Explicit operation targets when needed |
| Unsent input, local history and selection | UI | A submission only after user action |
| Theme, scroll, expansion, picker and window state | UI | No agent protocol state |
| Renderer-specific caches | UI | No agent ownership |

A UI display copy is derived state. It must be replaceable from a host
snapshot and subsequent events. It cannot become a second writer of the
agent's session file.

## Files that cross ownership today

| Current path | Work to separate |
| --- | --- |
| `App::checkpoint_with`, `build_meta`, `restore_display` in `app/session.rs` | Agent saves and replay data versus client drafts and display reconstruction |
| [`app/session_state.rs`](../../maki-ui/src/app/session_state.rs) | Agent settings versus UI selections and pending changes |
| [`app/tasks.rs`](../../maki-ui/src/app/tasks.rs) | Agent child lifecycle versus list, focus and chat presentation |
| [`app/mode.rs`](../../maki-ui/src/app/mode.rs) | Agent plan/workflow policy versus local plan forms |
| [`app/btw.rs`](../../maki-ui/src/app/btw.rs) | A provider operation over authoritative history versus its local panel |
| [`chat.rs`](../../maki-ui/src/chat.rs) | Semantic event data versus display messages and history grouping |
| [`components/login_picker.rs`](../../maki-ui/src/components/login_picker.rs) | Credential work in the provider environment versus login presentation |

## Persistence and restore

The host persists enough semantic data to reconstruct tools, tasks, costs and
plans. The UI keeps drafts and preferences in separate client storage keyed
by agent and session identity. Migration must preserve existing drafts and
accepted queues without assigning both to the same owner.

History loading must preserve hidden-message rules, image grouping, child
identity and readable tool results. Local renderers reconstruct the display.
Restoring a result does not call its tool handler or request a remote Lua
buffer. Current restore paths in `chat.rs` are a feature reference, not a
requirement to retain `RestoreItem` across the boundary.

## Failure paths

A failed save is reported as an agent error without corrupting terminal
output. A disconnected UI has no authority to write its stale session copy
back. A failed setting change leaves the host's confirmed setting intact.
A late event for a replaced generation cannot repopulate a new task list.
