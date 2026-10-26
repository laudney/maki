# Existing ACP adapter and target boundary

Source snapshot and reading rules: [survey index](README.md).
Target contract: [ACP and transport](../tui-over-acp-design.md#acp-and-transport).

## What exists

[`maki-acp`](../../maki-acp/Cargo.toml) uses
`agent-client-protocol-schema`, declared as `1.10` in the workspace and
resolved to `1.10.2` in [`Cargo.lock`](../../Cargo.lock). No schema upgrade
is part of this document rewrite.

[`server::serve`](../../maki-acp/src/server.rs) reads newline-delimited JSON
from stdin and writes JSON replies and notifications to stdout. `Server`
holds one `Option<SessionState>`. Session creation uses
`headless::spawn_interactive`. The adapter has model discovery, mode/config
handling, permission requests and question elicitation.

[`src/cmd/acp.rs`](../../src/cmd/acp.rs) starts the plugin host with no UI
interaction and passes config, prompt slots and a session-end callback to
the server. `maki-acp` itself has no `maki-lua` dependency. It does not
provide the full agent hook relay that the TUI path supplies.

## Current coverage

| Behavior | Current source | Limit for the target |
| --- | --- | --- |
| New/load sessions | `new_session`, `load_session`, `start_session` in `server.rs` | One current session in the server |
| Prompt submission | `handle_prompt` | A second active prompt is refused, not queued |
| Mode and model settings | `handle_set_mode`, `handle_set_config` | Does not match all TUI live-setting behavior |
| Text and thinking | `start_event_pump`, [`translate.rs`](../../maki-acp/src/translate.rs) | Existing useful standard data path |
| Tool results | `translate::tool_done` | Diffs stay structured, most other output becomes fenced text |
| Permission requests | `PermissionRoute`, `OwnedAsks`, [`permissions.rs`](../../maki-acp/src/permissions.rs) | Target must preserve complete answer semantics and ownership |
| Questions | `question_tool`, [`elicitation.rs`](../../maki-acp/src/elicitation.rs) | Native form path when supported, otherwise `question` is excluded |
| Replay | `translate::replay_history` | Insufficient for the TUI's full structured display and child history |
| Agent events | `start_event_pump` | Drops child display events and many progress/lifecycle/render events |
| Transport | `serve`, `read_stdin`, `send` | Only stdio is implemented |

`start_event_pump` is the primary reference for event loss. It omits
subagent transcript events and has a final catch-all for unhandled events.
The target must classify those events by meaning. UI-only buffer events can
be removed as part of the renderer split. Task lifecycle and semantic tool
streams need explicit protocol coverage.

## What the adapter should become

The ACP server adapts requests to the agent host and translates host events.
It does not own a second version of queue, tool, hook or session policy.
Its transport adapter handles framing and delivery separately from host
operations.

The existing permission ownership machinery and elicitation translation are
useful starting points. The question path should become a host interaction
service usable by the agent component of `question`, without calling a UI
window API.

Multiple sessions, accepted queues, tasks, compaction, richer result data,
model capabilities and complete replay are requirements to support or
negotiate. Their exact carriers are deferred. There is no remote-window,
keymap, theme or input-buffer protocol in the target.

## Transport requirements

Stdio remains a supported local link. HTTP and WebSocket adapters must carry
streaming updates, agent-to-client requests, replies and cancellation with
the same behavior. They also need explicit remote authentication and
workspace access policy. An in-memory adapter follows the same data contract
without exporting internal host objects.

Interaction replies and cancellation must continue to be processed while
another operation waits. Pending requests have explicit terminal outcomes
on disconnect. Delivery limits and resynchronization prevent a slow client
from growing unbounded output or silently losing required state.
