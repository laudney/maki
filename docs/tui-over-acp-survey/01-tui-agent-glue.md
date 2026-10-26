# Current TUI and agent coupling

Source snapshot and reading rules: [survey index](README.md).
Target ownership: [design](../tui-over-acp-design.md#component-boundaries).

## Current source

The TUI has its own session driver in
[`AgentLoop`](../../maki-ui/src/agent/agent_loop.rs).
[`AgentHandles`](../../maki-ui/src/agent/mod.rs) constructs the driver and
exposes channels, shared history, queue and cancellation handles to the app.
ACP instead uses `spawn_interactive` in
[`headless.rs`](../../maki-agent/src/headless.rs). Print has another path in
that file. These paths duplicate orchestration around the core `Agent`.

| Current connection | Source | Consequence |
| --- | --- | --- |
| Per-tab agent spawn and respawn | `AgentHandles::spawn`, `spawn_agent_internal`, `respawn` in `agent/mod.rs` | UI lifecycle owns agent execution |
| Shared input queue and interrupts | [`shared_queue.rs`](../../maki-ui/src/agent/shared_queue.rs), `QueueSender`, `QueueReceiver` | Scheduling depends on UI-owned types |
| Model cell | [`model_slots.rs`](../../maki-ui/src/agent/model_slots.rs), `ModelSlots` | UI and loop share model/provider state |
| History mirror | `SharedMessages` in [`agent/history.rs`](../../maki-agent/src/agent/history.rs) | UI reads live agent history to save, export and build requests |
| Cancellation | [`run_cancels.rs`](../../maki-ui/src/agent/run_cancels.rs), `RunCancels` | UI run IDs and cancellation watermarks control execution |
| Background work | `AgentLoop::new` calls `SessionMailbox::publish_seed` from [`live_session.rs`](../../maki-agent/src/live_session.rs) | Plugin sessions depend on state published by the TUI driver |
| MCP and prompt preparation | `AgentLoop::initialize`, `do_agent_run`, `do_compact` | Session setup and policy live inside the frontend crate |
| Agent event delivery | [`app/mod.rs`](../../maki-ui/src/app/mod.rs), `handle_agent_event`, `deliver` | The UI filters events and then relays some to Lua |

These are useful locations for extracting responsibilities. Their concrete
handles are not wire types.

## Target split

The agent host owns queue acceptance, run scheduling, model changes,
compaction, interrupts, MCP integration and generation-scoped background
work. It calls the pure loop and publishes semantic results through ACP.
The UI owns tabs, input drafts, display updates and local interaction.

The ACP adapter translates operations into the same host behavior used by
other frontends. Adding a transport must not create another scheduling loop.
An in-memory client follows the protocol contract without reading shared
queues, cancel maps or history pointers.

## Behavior that must survive the move

- Accepted queue items stay visible and have identities before consumption.
  Burst merge and mid-run interruption remain host decisions.
- Cancellation targets the intended run or task. A late completion or reply
  cannot settle a newer request.
- Foreground cancellation preserves background work still owned by the
  current session generation. Reset, rewind and deletion retire replaced
  work and its pending asks.
- A mailbox wake can start a run without a client prompt request. Lifecycle
  events must describe these runs too.
- Agent hooks fire from the host without a UI. Client display filtering does
  not decide whether agent policy executes.

The existing filters in `App::handle_agent_event` and `App::deliver` mix
semantic ownership with display concerns. Extract their intent before
moving them. Stale generation checks belong to the host and protocol
consumer. Decisions about visible tabs and rendered rows stay in the UI.
The old filter sequence is evidence of behavior, not a permanent hook API.

## Boundary checks

A second client or a headless session must use the same queue and
cancellation rules without constructing `maki-ui` types. A UI tab switch
must not change a tool's session context. Internal subagent execution stays
inside the host and need not communicate with its parent over ACP.
