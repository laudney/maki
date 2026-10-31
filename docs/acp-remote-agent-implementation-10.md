# Remote agents over ACP: implementation, step 6i

Status: proposed. Step 6i of the [design](acp-remote-agent-design.md)
turns `/usage` on for a remote tab, and makes a headless session file keep
its spending per model, which steps 6e and 6g left out.

## What the local TUI does

- Every `TurnCompleteEvent`, of the main agent, a subagent or a
  compaction, is billed by `App::bill`: the session's `token_usage`, its
  billed `cost` and its `subsidised_list_cost` grow, and
  `usage_by_model[turn.model]` takes
  `usage.billed_with_subsidised_list_cost(cost, subsidised_list_cost)`.
  The App's writer saves the rows, and a TUI load settles them in place
  (`settle_session` through `usage_by_model_mut`); an ACP load settles a
  copy instead.
- `/usage` opens the usage modal and refreshes the quota: a detached task
  calls the tab's `provider.fetch_usage()` and stores `Loading`, then
  `Ready`, `Unsupported` or `Error` in the App's `usage_slot`; Ctrl+R in
  the modal refreshes again.
- The modal draws the session total (tokens, cost, the list price beside a
  subsidised `$0`), the provider quota under the provider's display name,
  and one row per model sorted by tokens, priced by `model_cost`.

## Today on a remote tab

`/usage` flashes `UNAVAILABLE_REMOTE`. A headless turn adds only
`token_usage` to the session file (`SessionTurn::drop`): no headless turn
writes `usage_by_model`, so a resume prices the counts against the
session's model when the file has no rows, and leaves the headless
spending out when a TUI run wrote rows before.

## Per-model rows in the session file

`RunLedger` keeps the rows next to its totals. Both places that bill a
response build their `TurnCompleteEvent` first and hand it to
`RunLedger::bill(&event, list_cost)`, which adds `event.usage`, `event.cost`
and `list_cost` to the totals as `add` does today, and the event's row to
`by_model` exactly as `App::bill` does. A child ledger passes both up, so a
subagent's rows reach the turn that spawned it. `add` goes away: the event
is the one description of a billed response, so the file and the App
cannot disagree.

`SessionTurn::drop` adds each row to the stored session with
`add_model_usage`, next to `token_usage`. The local TUI's agent ledger is
not saved by a `SessionTrack`, so nothing is counted twice there.

**The saved baseline.** `settle_session` prices the aggregate counts only
while the session has no rows, and a row's cost only while it has none,
so merging the first headless rows into an older file would drop what
the file held before: token counts with no rows, or rows without a cost.
Before the first merge, the drop settles the stored rows in place with
`settle_session(token_usage before this turn, rows, recorded model,
false)`, the same rule every load uses, so live `/usage`, the file and a
resume agree. The recorded model is the one the session held when it was
opened (`SessionTurn::begin` replaces it).

A file whose rows cover only part of its counts (headless turns after a
TUI run on an older build) keeps that limit: with rows present,
`settle_session` treats them as the bill and the running total as
allowed to drift from them (a compaction or a resume moves it; its
tests pin that), so the uncovered counts cannot be told from drift and
are left unpriced, as every load leaves them today. This writes that estimate to
the file, which the load path avoids today; it is the only way to keep
the older spending once exact rows join it, and it happens once per
session.

## The usage ledger extension

Negotiated in `initialize` like the other Maki extensions: key
`usage_ledger`, version 1, read into `Agreed.usage_ledger`. It rides on
the usage extension's metadata, so it is agreed only together with
`usage` version 1; a peer offering it alone gets neither carrier nor
ledger.

- **The ledger rides on the usage updates.** With the extension agreed,
  every usage update of step 6e carries `_meta.maki.usage.ledger`:
  `{ total, byModel: { <model>: row } }`, the session's whole ledger at
  that point. A row is the wire form `{ input, output, cacheRead,
  cacheWrite, cost?, listCost? }`, kept apart from the stored types as
  the turn of step 6e is; `listCost` is the subsidised list price, and
  `total` carries no cost. The cost, list cost and subsidy
  already ride beside it. So the ledger is session state with one writer
  and wire order, as the gauge is: a `/new` gives a fresh usage slot, and
  an older update never overwrites a newer one.
- **`_maki/usage/quota`**, params `{ sessionId }`. Answered by a task, as
  `_maki/btw` is: it connects a provider for the session's model and
  answers `{ provider, ready: ProviderUsage }`, `{ provider, unsupported:
  {} }` or `{ provider, error }`, where `provider` is the display name of
  the provider it asked; a failed connection is an `error` answer. The
  server keeps the task with the side tasks, so the end of the link drops
  it. Another session is refused (`resource_not_found`).

## Server

The pump bills every `TurnComplete` it sees, main or not, into a
`SessionLedger { total, cost, list_cost, by_model }` with the same rule
as `App::bill`. It replaces `Spent`: the usage updates take `cost` and
`list_cost` from it, so there is one sum, and under the extension they
also carry `total` and `by_model`. A load seeds it from the stored
session: `token_usage`, the settled copy of the rows `load_session`
already builds, and the two totals it computes from them.

## Client

- `SessionUsage` gains `ledger: Option<Ledger { total, by_model }>`, read
  from the update under the extension. The remote App's
  `apply_remote_usage` copies it into `state.token_usage` and the session
  rows, next to the cost it copies today, so the modal reads a remote
  session as it reads a local one and stays live during a run.
- The quota fetch writes into a fresh slot each time, for local and remote
  tabs alike: the refresh puts a new `usage_slot` on the App and hands that
  one to its task, so a late or reordered answer lands in a slot nobody
  draws. A `/new` also gives the App a fresh slot.
- The quota answer names its provider: `usage_slot` holds `UsageFetch {
  provider, state }`, where a local fetch takes the tab model's display
  name when it starts. The modal heads the quota with it, not with the
  current model's name.
- `AcpClient::usage_quota(session)` sends the request for a session the
  caller read with `bound_session`, as `btw` does. `AgentHandles` of a
  remote tab keep a `RemoteUsage { client, binding }` when the extension
  is agreed, and `AgentBackend::Remote` gains `usage_ledger`, which
  `Feature::Usage` follows. For a remote tab, `Action::RefreshUsage`
  checks the binding (refused with `SESSION_OPENING`) and spawns the quota
  request.
- `UsageModalContext` gains `estimate`: a local tab prices a row that
  recorded no cost from its model's table, a remote tab does not, since
  the agent's rows carry their own cost.

## Tests

- Agent: `RunLedger::bill` adds the row a `TurnCompleteEvent` describes,
  also through a child ledger; a headless turn saves its rows; a resume
  settles them per model; an older file with token counts and no rows,
  and one with an unpriced row keep that spending after a headless turn
  adds rows; the load's live ledger, the file after a turn and a reopen
  agree.
- Extensions: `usage_ledger` agrees only with `usage` at version 1, not
  alone and not beside another `usage` version.
- Server: the pump's ledger bills main, subagent and compaction turns as
  `App::bill` does; a load seeds it; an update carries it only under the
  extension; the quota request answers `unsupported` for a provider
  without an endpoint and `error` for a failed connection, off the
  request loop, with the provider it asked; another session is refused.
- Client: the update's ledger decodes into `SessionUsage`; the quota
  request goes for the given session and decodes its answer.
- App: `/usage` on a remote tab with the extension opens the modal and
  yields the refresh, without it flashes `UNAVAILABLE_REMOTE`; a remote
  ledger in the usage slot fills the totals and rows; a reply to an older
  refresh does not replace the latest one; the quota heading is the
  answer's provider.

## Checks

Full `just ci`: fmt-check, workspace clippy with `-D warnings`, tests,
`gen-docs-check`, machete, pylint. A live run against `maki acp` over
stdio opens `/usage` after a turn, and a resume shows the same rows.
