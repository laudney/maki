# Remote agents over ACP: implementation, step 6j

Status: proposed. Step 6j of the [design](acp-remote-agent-design.md)
lets a message queued during a remote run reach the agent at the next turn
boundary, as it reaches a local agent, for messages of the running mode
and queued compactions. The ledger's queue row splits from login, which
stays off.

## What the local TUI does

- A message submitted during a run is queued (`QueueItem::Message`), and
  so is `/compact` (`QueueItem::Compact`). The queue panel shows them; the
  user can edit, pop or clear them until they are taken.
- The local agent loop hands the queue to the agent as its
  `InterruptSource`. After every turn, also the last one, the agent polls
  it once (`handle_queued_command`): `QueueReceiver::poll` takes the next
  run under the queue's lock (`pop_run`, a batch of messages sharing one
  mode and workflow, or one compaction). Messages land as user interrupts,
  each emitting `QueueItemConsumed`, the agent takes their mode, and the
  run goes on; a compaction compacts mid-run. A message queued during the
  last turn therefore extends that run instead of starting a new one.
- Taking an item and editing the queue share that lock, so an edit and the
  agent never both get one item.

## Today on a remote tab

The queue and its edits work, since they are client state, but the remote
loop pops the queue only between prompts: a queued message waits for the
whole run to end and then runs as a prompt of its own.

## Design

The client keeps the queue, and the agent asks for the next run at each
turn boundary, so the item is taken under the client's queue lock, as
locally, and no edit can race it.

### An async interrupt source

`InterruptSource::poll` becomes `fn poll<'a>(&'a self, cancel: &'a
CancelToken) -> BoxFuture<'a, Option<ExtractedCommand>>`, so a source can
wait for its answer, and can be stopped by the run's cancel while it does.
`QueueReceiver`, `SubagentInbox` and the tests' sources answer at once.
`handle_queued_command` awaits it with the agent's cancel token, and right
after checks that token: a cancelled wait ends the run with
`AgentError::Cancelled`, so its reason is `Cancelled` and not the last
turn's stop reason.

### Announcing a queued compaction

A host that draws a compaction only from events (the ACP pump) needs its
start: the pump marks the compactor's turn by it, as for an automatic
compaction (step 6e), and sends the `in_progress` update of step 6g. So
`handle_queued_command` emits `AutoCompacting` once the compaction hook
accepts and before the summary request.

`AutoCompacting` gains `manual: bool` (serde default `false`), set on the
start the agent really emits: the queued compaction and the manual
compaction of step 6g say `true`, the automatic one `false`. The local
chat draws its "Auto-compacting" line only for a start that is not
manual, so a local queued compaction looks as it does today. The pump
puts the flag on the `in_progress` update as `_meta.maki.manual`, and the
client draws the automatic line only when it is not set. A compaction a hook
declines emits no start, so nothing is left to mark a later automatic one
as manual.

### The steer extension

Negotiated in `initialize` like the other Maki extensions: key `steer`,
version 1, read into `Agreed.steer`.

**`_maki/steer/poll`**, an agent request, params `{ sessionId }`, sent at
every turn boundary of a run, once, as the local agent polls its queue.
The client answers `{ run: null }`, or `{ run: { prompts: [ [ContentBlock]
] } }` with one prompt per queued message, or `{ run: { compact: {
instructions? } } }`. One round trip per boundary is the whole cost; a
hint that the queue is empty could not be trusted at the boundary, since
a message can be queued while it travels.

### Server

- `InteractiveParams` gains `interrupts: Option<Arc<dyn
  InterruptSource>>`, which the interactive driver gives each turn's
  agent.
- With the extension agreed, `maki acp` passes a `SteerSource` holding
  the session id and the client channel. Its `poll` registers an
  `Ask::Steer` in the session's pending asks, sends the request, and waits
  for the answer, racing the cancel token. A guard removes the ask when
  the future drops, so a cancel before or during the wait leaves none
  behind. A run of prompts becomes `ExtractedCommand::Interrupt` with one
  `AgentInput` per prompt, built as a prompt's is, with the session's
  current mode; a compaction becomes `ExtractedCommand::Compact`. A cancel,
  an answer it cannot read, or the end of the link is `None`.

### Client

- **Which poll may take a run.** Only a poll the live prompt's loop reads
  in its event arm, and only after it checks, right before the take, that
  the run's cancel token and the teardown are not cancelled and that no
  rebind is waiting (one that is becomes the loop's pending rebind). The
  event arm can run before the cancel arm, so a sent `session/cancel` is
  not the test. Every other place the loop reads
  events, the drain after the prompt's answer, idle, a switch before the
  prompt, a replacement or a load, answers `{ run: null }`. The agent
  polls only inside a run, so its poll precedes that run's answer on the
  wire, and the loop drains it before it can send another prompt: no poll
  can feed a later prompt.
- **Which run.** `QueueReceiver::pop_run_if(eligible)` checks the front
  item and removes its batch under one lock, so an edit cannot swap the
  item between the check and the take. A batch of messages is eligible
  when its mode is the prompt's: a local interrupt takes `input.mode`, and
  a run for another mode waits for its own prompt, which switches the mode
  first. The workflow does not matter, as a local interrupt keeps its
  run's. A compaction is always eligible.
- The loop emits `QueueItemConsumed` for each message it answers with, as
  the local agent does, under the prompt's run id, and answers with their
  prompt blocks. A compaction's `in_progress` says it is manual (see
  above), so it draws no automatic line, as locally, and the enclosing
  prompt still ends as a turn, not as a `/compact`.
- Without the extension nothing changes: the queue waits for the run's end.

## Limits

- A queued message for another mode waits for the run's end instead of
  switching the running agent's mode, as a local interrupt would. Steering
  is therefore partial parity: matching-mode messages and compactions.

## Tests

- Agent: the async poll keeps today's interrupt and compaction tests; a
  cancel during the final boundary's wait ends the run as `Cancelled`; a
  queued compaction announces its start marked manual, and a declined one
  announces none; the local chat draws no automatic line for a manual
  start.
- Extensions: `steer` agrees alone and with the others.
- Server: a request goes out at the boundary and its run lands as
  interrupts, one input per prompt, then the run goes on; a compaction
  answer compacts and reaches the pump as a compaction; `{ run: null }`,
  an unreadable answer and a cancel give `None` and leave no pending ask.
- Loop: a message queued during a run is answered to the poll and drawn as
  consumed, so it never runs as a prompt of its own; a run for another
  mode is not taken; a poll read after the answer, with the token
  cancelled before `session/cancel` went, or with a rebind waiting is
  answered `null`; a front item removed before the take is not answered;
  a manual `in_progress` draws no line and a later automatic one does.

## Checks

Full `just ci`. A live run against `maki acp` over stdio queues a message
during a long run and sees the agent take it within that run.
