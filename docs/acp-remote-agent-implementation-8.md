# Remote agents over ACP: implementation, step 6g

Status: proposed. Step 6g of the [design](acp-remote-agent-design.md)
turns compaction on for a remote tab: `/compact` with its instructions,
the automatic compaction's chat line, and what both leave behind. `/btw`,
`/usage` and login follow as their own steps.

## What the local TUI does

- `/compact [instructions]` queues `QueueItem::Compact { instructions,
  workflow }`, also during a run. The agent loop runs `agent::compact`
  with the compaction model, under a fresh frame for the session's model,
  and emits the compaction turn's `TurnComplete`, `CompactionDone` and
  `Done { reason: Compact }`. The history is replaced by the summary and
  saved.
- An automatic compaction emits `AutoCompacting` first. The chat draws
  "Auto-compacting conversation..." for it and flushes on
  `CompactionDone`, and the gauge takes the size after.
- Esc cancels a compaction like a run.

Today a remote tab refuses `/compact`, and the automatic one runs on the
agent with no line in the chat.

## Carrier

Both halves use standard ACP:

- **The command.** `maki acp` advertises `compact` in
  `available_commands_update`, with the input hint "instructions", right
  after each `session/new` and `session/load` answer. A `session/prompt`
  whose only content is one text block reading `/compact` or `/compact
  <instructions>` runs a manual compaction instead of a turn. Any ACP
  client that shows agent commands gets `/compact` this way.
- **The progress.** The schema's unstable `CompactionUpdate` (feature
  `unstable_session_compaction`), sent only when the client advertised
  `sessionCapabilities.compaction`, as the schema requires.

## Server

### Running a manual compaction

`InteractiveHandle.input_tx` carries `SessionInput`:

```text
enum SessionInput {
    Prompt(AgentInput),
    Compact(Compaction { instructions: Option<String>, workflow: bool }),
}
```

The interactive driver runs `Compact` in `compact_session`, which takes
the provider, so a test can give it a stub, as `AgentLoop::do_compact`
does:
`resolve_compaction_model`, the context builder for the session's model
and `workflow`, `AgentHooks`, and `agent::compact`, inside a
`SessionTrack::turn` so the summary history is saved. Three things differ
from the local loop, where the App does them:

- **Start.** The driver emits `AutoCompacting` (the session's gauge and
  window) before it compacts. The pump then marks the compaction turn
  exactly as for an automatic one (step 6e), so the compactor's window
  never moves the main gauge, and progress has a start event.
- **Billing.** `agent::compact` takes an optional `RunLedger`, which the
  summary request bills as soon as its answer arrives, also an answer that
  then proves empty; the automatic compaction bills its run's ledger the
  same way. The driver passes the turn's ledger, and a turn that only
  spent saves its bill under the transcript as loaded, so the session file
  keeps the token counts, as a resume shows. No headless turn writes
  per-model rows (`usage_by_model`) yet. A resume of a session with no
  rows prices the counts against the session's model; one with rows, from
  a TUI run, sums the rows and leaves the headless spending out. The
  `/usage` ledger step adds the rows.
  The local loop passes `None` and keeps billing through its App.
- **End.** `compact` returns `Cancelled` without emitting `Done`. The
  driver then emits `Done { reason: Cancelled }`, so the pump answers the
  prompt `cancelled`; a failure emits `Error` as a run's does.

`CompactionDone` reports the session model's window (`hooks.model`), not
the compactor's, in both loops; pricing stays on the compactor.

The server admits a compaction like a prompt (one outstanding run). Its
`workflow` is the one the client captured when the compaction was queued,
read from the prompt's settings metadata; it applies to that compaction
only and does not change the session's intent. Without the metadata the
intent's workflow applies.

### Reporting progress

With `sessionCapabilities.compaction` agreed, the pump sends one
`CompactionUpdate` per compaction. Its id is a fresh `MakiId`, so it stays
unique within the session across loads.

| moment | status |
| --- | --- |
| `AutoCompacting` | `in_progress` |
| `CompactionDone` | `completed`, with the summary as one text block |
| a main `Error` while compacting | `failed`, with the error |
| a main `Done { reason: Cancelled }` while compacting | `cancelled` |
| a main `Done` while compacting with no `CompactionDone` (a hook skipped it) | `_skipped` |

Without the capability nothing new is sent, and the usage updates of step
6e still size the gauge.

## Client

- `maki connect` advertises `sessionCapabilities.compaction`.
- **`/compact`** is no longer refused for a remote tab. The loop runs
  `QueueRun::Compact` as a prompt of the text `/compact` and the
  instructions, with no mode, model or settings switch first (a local
  compaction runs on the session's model as it is). Under the settings
  extension the prompt carries the compaction's captured `workflow` in its
  settings metadata, with the intent's thinking and fast. The loop first
  drains queued updates, because the agent's `available_commands_update`
  can still wait behind the session answer; then, when the latest one
  advertised `compact`, it sends the prompt, and otherwise it ends the run
  with `Error(UNAVAILABLE_REMOTE)` as today.
  The run is a normal remote run: Esc sends `session/cancel`, the answer
  ends it, and a rebind interrupts it.
- **Progress.** `DisplayMapper` maps `in_progress` to `AutoCompacting`
  when the run is not a manual compaction (the loop tells the mapper which
  run it maps), so only the automatic one gets the chat line, as locally;
  `completed` maps to `CompactionDone` with the summary; `failed` and
  `cancelled` map to nothing, since the run's own end reports them. The
  sizes in `AutoCompacting` and `CompactionDone` come from the mapper's
  last usage, so the pump sends the usage update after a compaction
  before its `completed`; a remote App takes the gauge from its usage
  slot (step 6e). `_skipped` maps to nothing, like `failed`. A manual
  compaction answered `end_turn` ends with `Done { reason: Compact }`, as
  a local one does.

## Tests

- Server: `available_commands_update` with `compact` follows new and load;
  a `/compact` prompt sends `SessionInput::Compact` with the instructions
  and the carried workflow, without changing the intent, and is answered
  when `Done` comes; a second prompt during it is refused as during a run;
  the five `CompactionUpdate` moments under the capability, with fresh ids,
  none without it.
- Driver: a `Compact` input emits `AutoCompacting`, compacts and saves the
  history, credits the ledger so a resume shows the spending, and emits
  `Done`; a cancel emits `Done { Cancelled }` and the next prompt runs;
  `CompactionDone` carries the session model's window when the compactor's
  differs.
- Loop: `QueueRun::Compact` sends the `/compact` prompt with its
  instructions, its captured workflow and no switch, also when queued
  before a workflow toggle; an advertisement still queued behind the
  session answer is read first; without the command it ends with
  `UNAVAILABLE_REMOTE`; Esc during it sends `session/cancel`.
- Mapper: `in_progress` maps to `AutoCompacting` for an ordinary run and
  to nothing for a manual compaction; `completed` maps to `CompactionDone`.
- App: `/compact` on a remote tab queues the compaction; the automatic
  compaction draws its line.

## Checks

Full set: `cargo fmt`, workspace clippy with `-D warnings`,
`cargo nextest run --workspace`. A live run against `maki acp` over stdio
runs `/compact`, checks the gauge drops and the conversation continues.
