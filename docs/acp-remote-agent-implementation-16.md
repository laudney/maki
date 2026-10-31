# Remote agents over ACP: implementation, step 6o

Status: proposed. Step 6o of the [design](acp-remote-agent-design.md)
lets a message queued in the other mode join a running remote agent at
its next turn boundary and switch the run to that mode, as a local run
does.

## What the local TUI does

At every turn boundary the agent polls its queue (`QueueReceiver::poll`)
and takes the next run of queued messages whatever their mode: the
messages typed back to back with one mode and workflow
(`pop_run_from`). Each kept message switches the run to its own mode
(`self.mode = input.mode` in `handle_queued_command`), so a plan-mode
message queued during a build run turns the rest of that run into a plan
run, with the plan file its `AgentMode::Plan` names. A compaction joins
as before.

## Today on a remote tab

Step 6j's steer poll only takes a message of the running mode
(`QueueItem::steers`), since the agent applies the running prompt's mode
to what it takes (`SteerSource.mode`). A message of the other mode waits
for the run to end and starts a run of its own.

## Design

### Wire: the steer mode extension

A Maki extension `steer_mode`, version 1, in `Agreed.steer_mode`. It
rides on the steer extension's poll, so it is never agreed without
`steer`, as the usage ledger needs usage.

- **The answer.** `SteerAnswer` gains `mode`, an optional mode id
  (`build` or `plan`, as `session/set_mode` takes), serialized only when
  set. A client under the extension sets it on every `prompts` answer to
  the mode of the run it took; an agent without the extension never sees
  it, and one with it reads its absence as the running mode.
- **The agent.** `SteerSource` maps the id as `session/set_mode` does,
  and a plan id names the session's own plan file: the one the session
  already has, or a new one that the session keeps from then on, as
  `keep_plan_path` keeps it. That needs the session's plan path shared
  with the source, so `SessionState.plan_path` becomes an
  `Arc<Mutex<Option<PathBuf>>>` read and written by both. The inputs it
  hands the run carry that mode, and the run switches only for an input
  it keeps, exactly where a local run switches (`agent.user_message` may
  drop one, and a cancel may end the run first). So the source keeps no
  mode of its own: `InterruptSource::poll` gains the run's current mode
  as an argument, which the run passes at every boundary and the local
  sources ignore, and an answer without a mode takes that one. The
  `SteerSource.mode` that `handle_prompt` set goes away.
- **No refusal, no weaker mode.** The client has taken the messages off
  its queue once it answers, so the agent never drops them, and it never
  runs a plan-mode message in build mode, which would lift plan mode's
  write restriction. The client only sends `build` and `plan`, which an
  agent that offers both modes knows. A plan id names the session's plan
  file in the agent's state dir (`params.storage`, resolved when
  `maki acp` started); should even that fail, the inputs still run in
  plan mode, with a plan file in the agent's temporary dir that the
  session keeps, and a warning is logged.
- **The plan write marker.** 6n marks a write to the plan file of a
  plan-mode prompt (`PlanRun`, set in `handle_prompt`). A run can now
  enter plan mode at a turn boundary, which nothing outside the run sees
  when it happens, so the marker instead follows the session's shared
  plan path and marks any successful write to it. The client adopts a
  mark only while its tab is in plan mode, as the local TUI checks its
  mode before a write opens the form, so a tab in build mode opens no form
  for an edit of the plan file. A tab in plan mode whose run was steered
  into build mode does open it for a write there, as a local tab in plan
  mode does whatever its run's mode.
- **The session's mode.** A switch inside a run does not change the
  session's `current_mode`, which only `session/set_mode` sets, so the
  next prompt runs in the mode the client asks for, as before.

### Client

- Under the extension, the live prompt's poll takes the next run whatever
  its mode (the filter of `QueueItem::steers` keeps only the compaction
  rule), and answers with the run's mode id. Without it, 6j's rule holds.
- Only an agent that offers the `build` and `plan` modes has mode ids to
  send; without them every message is of the running mode already.

## Limits

- The run's mode after a switch is the agent's; the tab's own mode
  indicator stays the user's choice, as locally, where the toggle and the
  running mode can differ too.

## Tests

- Extensions: `steer_mode` agrees only with `steer`.
- Agent: a `prompts` answer with a plan id hands inputs in plan mode with
  the session's plan path, a new one kept when there was none; a build id
  hands build inputs; no id hands them in the mode the run passed, also
  after a switch to plan earlier in the run; a plan file that cannot be
  made in the state dir still gives plan-mode inputs; a write to the
  session's plan file is marked in any mode, another file's in none.
- Client: under the extension a message of the other mode is taken and
  its mode id sent; without it the message stays queued; a mark adopted
  only in plan mode stays covered by 6n's tests.

## Checks

Full `just ci`. A live run of `maki connect -- maki acp` starts a build
run with a tool call, queues a plan-mode message during it, and sees the
message join the run and the agent switch to planning.
