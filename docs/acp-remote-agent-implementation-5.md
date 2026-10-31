# Remote agents over ACP: implementation, step 6d

Status: proposed. Step 6d of the [design](acp-remote-agent-design.md)
turns on the first part of the session row of the parity ledger: a
session loaded at startup with its history drawn, and `/new` in place.
It builds on [step 6c](acp-remote-agent-implementation-4.md) and keeps
its rules: the agent owns the session, the loop is the only client code
that talks to it, and the wire order decides what is current.

`maki acp` already answers `session/load` and `session/list`. It sends the
stored history as `session/update` lines before it answers `session/load`.
Tabs, `/fork`, loading into a running tab, `/sessions`, rename and delete
stay off; the ledger keeps them as their own rows.

## Startup: resume a session

`maki connect` gains the resume flags of `maki`, in one argument group so
only one way names the session:

- `-c, --continue`: the newest session the agent lists for the workspace
  directory. With none listed, a new session opens, as `maki -c` does
  locally.
- `-r, --resume <ID>` (visible alias `--session`): that session.

After `initialize`, both need `agentCapabilities.loadSession`, and
`--continue` also needs `sessionCapabilities.list`. Without them
`maki connect` exits with an error that names the missing capability. A
failed list or load exits with the agent's error, as a failed
`session/new` does.

`--continue` sends `session/list` with the workspace cwd and follows
`nextCursor` until the agent returns none; a cursor seen before ends the
walk. ACP does not order the list, so the client picks, over all pages,
the session with the greatest `updatedAt` (parsed as a timestamp; a
missing or unreadable one ranks lowest), and the order of encounter
breaks ties.

The `session/new` and `session/load` answers carry the same modes and
config options, so `open` reads both through one value:

```text
struct Opened {
    modes: Option<SessionModeState>,
    config_options: Vec<SessionConfigOption>,
    seq: u64,                    // wire position of the answer
    replay: Vec<SessionUpdate>,  // empty for session/new
    unreadable: usize,           // replayed lines the client could not read
}
```

`RemoteAgent` gains `replay` and `unreadable`. The display model, mode,
menu and `session_seq` come from `Opened` as they came from the
`session/new` answer.

## Client: load with its replay

`AcpClient::load_session(events, id, cwd)` returns
`Loaded { response: Sequenced<LoadSessionResponse>, replay, unreadable }`.

- **Binding.** The replay arrives before the answer, so the client binds
  `id` before it sends the request. If the answer is an error or the link
  closes, it restores the previous binding.
- **End of the replay.** The request is registered with
  `Binds::Replay`. When the reader resolves that response, success or
  error, it then queues `ClientEvent::ReplayEnd` on the event channel.
  Every update the agent sent before its answer is therefore ahead of
  the marker, and no later event is consumed with the replay.
- **No deadlock.** The event channel holds `EVENT_CAPACITY` lines, and a
  full channel pauses the reader. `load_session` reads events while the
  request is pending, so a replay of any length reaches the answer.
- **What it reads.** `Update` lines go to `replay` in order. A
  `Malformed` line counts in `unreadable`. A `Permission` request is
  answered cancelled, because no run can show it. `Closed` ends the load
  with `ClientError::Closed`.

`AcpClient::list_sessions(cwd)` returns the `session/list` answer.

## Drawing the replay

`maki-ui/src/agent/remote_replay.rs` draws a replay into a chat, through
the same `Chat::handle_event` path a live run uses, with a fresh
`DisplayMapper` that has the connection's render flag:

- Consecutive `UserMessageChunk`s become one user bubble with their text
  and images, as `show_user_message` draws a typed prompt.
- An `AgentMessageChunk` with an image draws an assistant image bubble,
  as the local load draws an assistant image block.
- Every other update goes through the mapper, and each event it returns
  goes to `handle_event`. The replay maps to text, thinking, tool rows
  and notices only, so it never starts a run, a permission prompt or an
  error.
- A call the history left without a result is finished at the end with
  no output, as the local load draws it.
- The last `UsageUpdate` sets the context gauge, as a run's `Done` sets
  it.

The event loop draws the replay into the remote tab's main chat before
the first frame, and flashes `MALFORMED_UPDATE` once when `unreadable` is
not zero. The replay starts no run, fires no hook and writes nothing to
the local store. The ledger rule "replay fires none" holds.

## Server: replay rows like live rows

Four changes in `maki acp`, each also correct for other ACP clients:

1. **Header.** A replayed call is a pending card named by the tool, then
   an in-progress update whose title is the header the local load shows
   (`ToolRegistry::resolve_header`), with its raw input and locations,
   then the result. Today the title stays the bare tool name.
2. **Structured output.** When the render extension was agreed, the
   replayed result carries `toolRender.output` with the stored
   `ToolOutput`, so a diff, read, write, grep or todo row draws as the
   local load draws it. Without the extension the standard content is
   unchanged.
3. **Context.** After the replay, one `UsageUpdate` carries the session's
   stored context size, the recorded model's context window and the
   restored cost, so the gauge matches the local load.
4. **Assistant images.** An assistant image block replays as an
   `AgentMessageChunk` with image content. Today the replay drops it.

The local load also asks the UI plugins to restore their styled bodies.
`maki acp` does not run that restore on replay, so a plugin-rendered row
replays with its structured or plain output. The ledger adds that as its
own row, owned by the client renderers of the UI Lua runtime, which draw
from the persisted semantic output and state.

## `/new` in place

`/new` maps to a new `Feature::NewSession`, which every remote backend
supports, because `session/new` is a required ACP method. `/fork` keeps
`Feature::Sessions` and stays off.

On a remote tab, `/new` resets the tab as it does locally, except that it
fires no session hook (the agent fires its own) and writes no checkpoint.
It then returns `Action::RestartAgent` with no history, and
`respawn_agent` rebinds the remote loop instead of refusing.

A respawn gives a local tab a new loop with new input channels, so no
input meant for the old session reaches the new one. A remote tab keeps
its loop, because the loop owns the connection's event channel. It gets
new input channels instead:

```text
struct Rebind { queue: Arc<QueueReceiver>, run_id: u64 }
```

`AgentHandles` keeps a `rebind_tx` for a remote tab. The respawn makes a
new queue, bumps the App's run id, sends `Rebind` with the new receiver
and run id, and only then points the App at the new queue, which drops
the old sender. Prompts typed after `/new` wait in the new queue, which
the loop reads only after the switch. Prompts still queued for the old
session are dropped with the old queue, as a local respawn drops them.

The handshake has one ordering rule: the loop always takes a waiting
`Rebind` before it reads its queue, and when its queue closes it first
takes a waiting `Rebind` and stops only when none waits. `Rebind` is sent
before the old sender drops, so a replaced queue never reads as a
shutdown.

The loop takes `Rebind` when idle and during a run, including the run's
preflight switches (`settle` watches the rebind channel too):

1. During a run it sends `session/cancel` when the prompt is out, cancels
   every pending ask, and stops waiting for the prompt or the switch.
2. It sends `session/new` with the workspace cwd and settles it to the
   end, without a run token. A replacement is never abandoned, because
   its answer binds the client's session. A `Rebind` that arrives while
   one settles waits, and the loop takes it next, so repeated `/new`
   runs one replacement after another and the last one wins. Asks are
   answered cancelled.
3. Updates are sorted by wire sequence, not by when the loop reads them.
   Every update read while the request is pending is kept with its
   number. On the answer, with sequence number `R`, the loop installs
   the answer's state, then applies the kept updates numbered above `R`
   in order and drops those below it, which belong to the old session.
   The drain after the answer follows the same rule.
4. Installing the answer takes the mode, publishes the model menu, sets
   both sequence numbers to `R`, and starts a fresh mapper.
5. A failed `session/new` is a session-level transition, not a run
   error: the agent may keep the old session or have none, and a prompt
   sent now could land in hidden old context. The loop logs the error,
   emits `Notice(NEW_SESSION_FAILED: ...)` under the rebind's run id, and
   closes, which drops the connection. A notice only draws a line; a run
   `Error` would clear the App's queue before the recovery below could
   read it. The event loop then reports the lost connection as for a
   lost link: unsent text and images return to the input box and no
   later prompt is accepted. A closed link closes the loop as it does
   elsewhere.

**The model across `/new`.** The local `/new` keeps the tab's model. The
agent opens a new session on its own default model, so the loop carries
the model across. The value carried is the pick whose switch the rebind
interrupted, if any; otherwise the old menu's current value, which is
what the agent last confirmed. A refused pick was already retired (step
6c) and is never carried. After the answer, if the new menu offers that
value and its current value differs, the loop stores it as `wanted`
with the new menu's config id, through a compare-and-swap that keeps a
pick the user made meanwhile. The next prompt's preflight then switches
and confirms it as for any pick. If the new menu does not offer it,
nothing is carried and the status bar shows the agent's model.

The loop keeps `plan_modes` from the first session. If a new session does
not offer `build` and `plan`, the next mode switch fails and the run ends
with `MODE_NOT_SET`, so no prompt goes out in the wrong mode.

## Tests

- Client: a load binds the session before its answer and keeps the
  replay in order; a replay longer than `EVENT_CAPACITY` reaches the
  answer; an error answer still ends the replay and restores the
  previous binding; a permission request during the load is cancelled;
  an update after the answer stays on the event channel.
- `maki connect`: `--continue` picks the greatest `updatedAt` over all
  pages, including a newest session on a later page, ranks missing ones
  lowest and keeps the encounter order on ties; a repeated cursor ends
  the walk; with no session it opens a new one; a missing capability
  names itself; `-c` and `-r` conflict.
- Replay drawing: one bubble for a user text with an image; an
  assistant image bubble; agent text,
  thinking, a tool row with its header and result, a structured diff
  row, an unfinished call drawn finished, the context gauge, and no
  envelope sent to the agent channel.
- Server: the replayed header is the registry header; the replayed
  result carries the stored output only under the extension; one usage
  update follows the replay; an assistant image replays as an image
  chunk.
- Remote loop with a scripted peer: an idle rebind opens the new session
  and the next prompt goes after its answer; the old queue closing with
  a `Rebind` waiting does not stop the loop (forced with a barrier); a
  rebind during a run and during a preflight switch sends
  `session/cancel` and draws nothing of the old run; old queued prompts
  are not sent; a second `/new` during a replacement waits for the first
  answer and then opens another session; an update numbered above the
  answer but read before it is applied after the answer's state, and
  one numbered below is dropped; a failed `session/new` emits the
  `NEW_SESSION_FAILED` notice and closes the loop; the confirmed model
  and an interrupted pick carry over, a refused pick is not retried
  after `/new`, a pick the new menu lacks is not carried, and a pick
  made during the replacement is kept.
- Event loop: a failed `/new` with queued text and an image returns
  both to the input box and refuses later prompts.
- App: `/new` on a remote tab restarts the agent with no history and no
  unavailable flash; `/fork` stays unavailable.

## Checks

Full set: `cargo fmt`, workspace clippy with `-D warnings`,
`cargo nextest run --workspace`. Live runs against a child `maki acp` and
the Pantheon listener: `--continue` draws the last session, and `/new`
opens a fresh one that answers.
