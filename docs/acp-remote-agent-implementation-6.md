# Remote agents over ACP: implementation, step 6e

Status: proposed. Step 6e of the [design](acp-remote-agent-design.md)
turns the usage row of the parity ledger on for what the TUI draws outside
the `/usage` modal: the context gauge, the cost and its list-price
reference in the status bar, and the usage line on a turn's last tool row.
It follows the rules of the earlier steps: the agent owns the numbers,
standard ACP carries what it can, and a negotiated Maki extension carries
the rest.

The `/usage` modal draws the session's token totals and per-model ledger.
It stays off for a remote tab with the rest of its row (compaction, `/btw`,
usage, login), and that row brings the ledger.

## What the local TUI shows

A local turn ends with `AgentEvent::TurnComplete`. The App bills it into
the cost and list cost, sets the gauge from its context size, and keeps
the turn's usage line; `ToolResultsSubmitted` then puts the line on the
turn's last tool row. The status bar shows the gauge against the model's
window and the cost, with `~$list source` beside it when the model is
subsidised. Today a remote tab gets only the gauge, and only at `Done`.

## Session usage: applied on receipt

Cost and gauge are session state, not run state. An error has no `Done`,
and Esc moves the App's run id, so an event of the old run is dropped.
The loop therefore publishes the session's usage to the App through a
shared slot, as it publishes the model menu, and the App applies it in
`tick` through a `Watch`, whatever run is current:

```text
pub(crate) struct RemoteUsage {
    used: u32, window: u32,          // the standard gauge
    cost: Option<f64>,               // the standard session total, USD
    list_cost: Option<f64>,          // extension: session total at list price
    subsidised_by: Option<String>,   // extension: the subsidy's display name
}
```

Every `UsageUpdate` replaces it. The App sets the gauge of the main chat
and `state.context_size`, the window of the display model when it is not
zero, the session's and the main chat's cost, and the list cost and its
source. The status bar then draws them as it draws a local session. The
`Done`-based gauge of step 4 is removed for a remote tab.

The slot belongs to one session. `RemoteUsage::from_update` reads one
update, for the loop and for the seed alike:

- **Load.** `spawn_runtime` seeds the slot from the replay's last usage
  update before it spawns the loop, so any live update, which is newer,
  replaces the seed and never the other way round. A loaded session thus
  shows what it paid, as a local load does.
- **`/new`.** `Rebind` carries a fresh, empty slot. The loop publishes to
  it from the start of the replacement, and the App watches it from the
  rebind on, with a fresh `Watch`. A snapshot the old session published
  but the App had not drawn lands in the old slot, which nobody reads, so
  it cannot bring the old cost back after `/new` cleared it.

A currency other than `USD` leaves the cost unset, because the local
display has no other currency. An agent that sends no cost leaves it
unset, and the status bar hides it as before.

## The usage extension

Negotiated in `initialize` metadata, like the tool render extension:
`_meta.maki.usage: 1` both ways, exact version 1, agreed independently of
the render extension. Both live in one `maki` metadata object, so a new
`maki-acp/src/extensions.rs` builds the client's request with every key,
the server's answer with the agreed keys, and reads both into an
`Agreed { render, usage }` value. `render.rs` keeps its wire types and
limits.

When agreed, every `UsageUpdate` from `maki acp` carries
`_meta.maki.usage`:

```text
struct WireUsage {
    list_cost: Option<f64>,        // session total, like the standard cost
    subsidised_by: Option<String>, // from the session's model
    turn: Option<WireTurn>,        // a main-agent turn only
    results_submitted: bool,       // the main agent sent tool results back
}
struct WireTurn {
    model: String,                 // the event's opaque model id
    input: u32, output: u32,       // non-cached input, output
    cache_read: u32, cache_write: u32,
    cost: Option<f64>,             // this turn, USD
    list_cost: Option<f64>,        // `subsidised_list_cost`
}
```

Field names are camelCase on the wire, and the wire types are their own
structs, so `TokenUsage` can change without changing the protocol. The
list cost total starts from the stored per-model list prices on a load,
added up as a local load adds them.

### When the server sends usage

- A main-agent `TurnComplete`: the gauge from the turn, the totals, and
  `turn`. The server keeps this gauge and its window as the main gauge.
- A main-agent `ToolResultsSubmitted`, under the extension only: the main
  gauge, the totals and `resultsSubmitted`.
- A subagent's `TurnComplete`: its cost already folds into the total, so
  the server resends the last main gauge with the new totals and no
  `turn`. The published total therefore never lags child spending, even
  when the run ends right after.
- Automatic compaction, which is on by default: between `AutoCompacting`
  and `CompactionDone` a `TurnComplete` is the compaction turn. Its cost
  counts toward the totals, but its window is the compaction model's, so
  the server resends the main gauge with the new totals and no `turn`.
  `CompactionDone` then sends `context_size_after` against the main
  window. Compaction can fail or be cancelled before `CompactionDone`, so
  every main-agent `Done` and `Error` also ends it, and each session
  starts outside it: an ordinary turn of a later run is never taken for a
  compaction turn.
- After a replay: the restored gauge and totals, no `turn`.

## Client mapping

`DisplayMapper` decodes the extension only when agreed, through the same
sanitizing path as every incoming line. An unreadable extension is
dropped with a `warn!`; the standard gauge and cost still apply. The loop
publishes `RemoteUsage` from every update before it maps it.

A `turn` maps to `AgentEvent::TurnComplete` with an empty assistant
message, the turn's usage, model, cost and list cost. For a remote tab
the App takes from it only the usage line: the session's numbers come
from `RemoteUsage`, and billing the turn again would count it twice.

`ToolResultsSubmitted` marks when a turn's tool results went back. The
client cannot infer it: a stream that fails and is retried within a run
has announced calls that never finish, and the agent sends no update for
the retry. So `maki acp` carries the agent's own boundary: under the
extension, a main-agent `ToolResultsSubmitted` sends a usage update with
the main gauge, the totals and `resultsSubmitted: true`, and the mapper
maps that flag to `ToolResultsSubmitted`. The line then lands on the
turn's last tool row as it does locally, even when the next request
fails. A child's boundary stays out, like the rest of its events. Every
end of a run clears the mapper's open calls, so a call a failed run left
open never takes an update of a later run.

## Not in this step

`/compact` stays off for a remote tab with its row. Automatic compaction
runs on the agent and is reported as above; its chat notice comes with
that row.

## Tests

- `extensions.rs`: the request lists both keys; each extension is agreed
  alone, both together, and not at another version.
- Server: under the extension a main turn's update carries the turn and
  totals; a subagent turn resends the main gauge with the new total and no
  turn; a compaction turn with a different window resends the main gauge
  and `CompactionDone` sends the size after against the main window; a
  compaction that fails or is cancelled, followed by a run without one,
  reports that run's turn normally; the
  replay's update carries the restored list cost; without the extension
  updates are standard only.
- Mapper: a turn maps to `TurnComplete`; `resultsSubmitted` maps to
  `ToolResultsSubmitted`; an unreadable extension and a non-USD cost are
  ignored.
- Server: a main `ToolResultsSubmitted` sends the boundary under the
  extension only, and a child's never.
- Loop and App: a usage update before an error, and one after Esc,
  still set the cost; the window changes on receipt; a remote
  `TurnComplete` sets the usage line on the last tool row and bills
  nothing; a subsidised remote model draws the list-price reference; the
  replay seeds the cost and a live update replaces the seed; an old
  session's snapshot left undrawn across `/new` does not come back.

## Checks

Full set: `cargo fmt`, workspace clippy with `-D warnings`,
`cargo nextest run --workspace`. A live run against the Pantheon listener
shows the cost and a tool row's usage line.
