# Remote agents over ACP: implementation, step 6f

Status: proposed. Step 6f of the [design](acp-remote-agent-design.md)
turns the settings row of the parity ledger on for thinking, fast,
workflow and yolo. The agent owns what a setting does and what is
effective; the client owns the user's intent, as the local App does.

## What the local TUI does

- **Thinking, fast and workflow** are App state that each prompt carries
  in `AgentInput`. When prompts are merged into one run, `merge_inputs`
  takes thinking and fast from the last message; mode and workflow split
  batches. `/fast` and `/workflow` toggle the state; `/thinking` is a UI
  plugin and comes with the UI Lua runtime. The agent clamps the values to
  its model when it runs (`RequestOptions::clamped`).
- **Display.** The badge shows `ThinkingConfig::clamped` then `shown` for
  the model, and `fast` shows only when the model can do it; the intent
  is kept as a ceiling.
- **Yolo** is a `PermissionManager` toggle that applies at once, also to
  later checks in a running turn. It sits after the deny rules and inside
  the plan-mode gate (`check_inner`), so it is agent policy: a client
  answering asks for the user would allow writes that local yolo does not.
- **Persistence.** The App's checkpoint saves all four, also when they
  changed while idle (`build_meta`). `/new` keeps them (`blank_session`);
  load restores them from the session's meta.

Today a remote tab refuses `/fast`, `/workflow` and `/yolo`, and `maki acp`
runs every prompt with its startup defaults.

## The settings extension

Negotiated in `initialize` like the other Maki extensions: key `settings`,
version 1, read into `Agreed { render, usage, settings }`.

When agreed, `session/prompt` carries
`_meta.maki.settings: { thinking, fast, workflow }` from the run's merged
`AgentInput`, with thinking as its `StoredThinking` setting string. `maki
acp` builds that prompt's `AgentInput` from them, so a prompt runs with
the values its batch carries, as locally, and the agent clamps them to the
model it runs, so a model switch before the prompt is never an error.
`SessionTrack::turn` saves them. A prompt without the metadata runs with
the session's current values.

## Config options

Every `session/new`, `session/load` and `set_config_option` answer, and
every `ConfigOptionUpdate`, carries next to the model option:

| id | kind | category | value |
| --- | --- | --- | --- |
| `thinking` | select | `thought_level` | current: the session's intent; values: `Model::thinking_options()` and the intent; each value's name is its effective label, `clamped(model).shown(model)` |
| `fast` | boolean | other | the intent; the option is sent only when the model can do fast |
| `workflow` | boolean | other | the intent |
| `yolo` | boolean | other | the `PermissionManager`'s value |

The session's intent starts from the startup defaults on `session/new`
and from the stored meta on `session/load`, where yolo also goes to the
`PermissionManager`. `set_config_option` sets one of them: thinking is
validated as a setting string, fast and workflow as booleans, and yolo
calls `set_session_yolo(Some(value))` at once, so it reaches the running
turn. A prompt whose metadata differs from the intent updates it. Each
change sends a `ConfigOptionUpdate` when it did not come as a
`set_config_option` answer, so every client sees it.

### Saving the intent

The prompt's values are that run's execution snapshot; the session's
intent is separate and can be newer, for a toggle that came while a run
was out. So the server and the interactive driver share one
`Arc<ArcSwap<SessionSettings>>` (`thinking`, `fast`, `workflow`, `yolo`),
given in `InteractiveParams`. The server writes it on every change, in the
order its single request loop sees them. The driver's `SessionTrack`
writes the session's meta from it at every save, and from a prompt's
input only when it has no shared intent (other drivers keep today's
behavior). Because a save always reads the latest intent, no order of a
prompt and a toggle can store an older value.

- **Idle.** A change also wakes the driver through a coalescing
  `settings_saved` channel (`bounded(1)`); the driver waits on prompts and
  that channel together, and on a wake `SessionTrack::save_settings`
  saves the meta when the session already has a file (an empty session
  writes none, as today).
- **Close and replace.** `SessionTrack` saves unsaved settings when it
  drops. Cancelling the driver drops it, so a change accepted just before
  `close_session` or a replacement still reaches the file.
- **Client exit.** `maki connect` keeps the client, and after the UI
  returns it calls `AcpClient::shutdown(SHUTDOWN_GRACE)`: it closes the
  outgoing side and waits up to the grace period for the link to end,
  which for a child process means its exit, before the pumps kill it.
  The agent then closes its sessions, which saves them. A WebSocket link
  just closes, and the agent's own close does the rest.

## Client

### Seeding before the first prompt

`maki connect` reads the options from the `session/new` or `session/load`
answer before the App exists. `RemoteAgent` carries them, and
`spawn_runtime` writes the intent into the App's state before any
submission, also the `--prompt` one: `state.thinking`, `state.fast`,
`state.workflow`, and yolo on the App's `PermissionManager`. The menu goes
into a `RemoteSettings` slot, published by the loop like the model menu
under the wire-order rule.

### Intent and display

The App owns the intent and keeps it through later menus; a remote tab
does not clamp it (`clamp` and the `/fast` model check use the placeholder
display model). The status bar shows what the agent made of it, from the
latest menu: the thinking badge is the name of the intent's value in the
`thinking` option, the fast indicator shows only while the `fast` option
exists and is on, and workflow and yolo show their values. A value the
menu does not list shows its setting string.

### Commands

`/fast` and `/workflow` work when the settings extension is agreed and the
agent offers the option, because the prompt carries them; `/yolo` works
when the agent offers its option. Otherwise they flash
`UNAVAILABLE_REMOTE`. `Feature::Settings` becomes per option.

### Syncing a change

Every toggle writes the App's intent, with the option's new revision, into the
`RemoteSettings` slot and wakes the loop. The loop sends
`set_config_option` for each of the four whose intent differs from the
agent's last menu, one request at a time, latest value first in line. It
sends them as routed requests: the client registers the request with
`Binds::Event(tag)`, and the reader queues its answer on the event channel
as `ClientEvent::Answer { seq, tag, result }`. The answer is handled in
whatever the loop is waiting on (idle, a run, a switch, a replacement), so
it is never lost when a run ends, and mid-run the run goes on.

- **Fence.** The tag holds the session generation, which `/new` bumps; an
  answer of an older generation is dropped.
- **Revisions.** Each option carries its own revision, which the App
  bumps on every change of that option only. A request names the
  option's revision it sends.
- **Refusal.** The loop records the refused option and revision in the
  slot and emits `Notice(SETTING_NOT_SET: ...)`, a session-level notice.
  The App puts that option back to the agent's value only when the refused
  revision is still that option's newest, so a later toggle of it wins and
  a toggle of another option does not matter. A refused revision is not
  sent again.

### Yolo before a prompt

Thinking, fast and workflow need nothing before a prompt: the prompt
carries them. Yolo is policy for the prompt's tool calls, so the preflight
settles it first: while a yolo request is in flight it waits for its
answer, and when the intent still differs from the agent's value it sends
and waits, racing events, answers, teardown, the cancel token and a
rebind as the other switches do. A refused yolo ends the run with
`Error(SETTING_NOT_SET: yolo)` and no prompt goes out.

### `/new`

The App keeps all four intents, as local `/new` does. After the
replacement, the loop syncs every intent that differs from the new
session's menu, so the new session's next prompt waits for a differing
yolo as above.

## Tests

- Extensions: the `settings` key agrees alone and with the others.
- Server: the prompt's metadata builds its `AgentInput` and updates the
  intent; without it the intent applies; the options per model, with
  `thinking_options` values, effective names, and fast only when
  supported; yolo reaches the `PermissionManager` at once; a change while
  idle updates the shared intent and wakes `settings_saved`; a load
  restores all four.
- Agent: a save writes the shared intent, also when an older prompt's turn
  ends after a newer toggle; `save_settings` writes nothing for an empty
  session; dropping the track saves unsaved settings.
- Server: a change accepted just before `close_session` reaches the file.
- Client: `shutdown` waits for a child that exits and kills one that does
  not within the grace period; toggle, exit and resume keep the value.
- Client: a routed answer arrives on the event channel, also when the run
  that asked ended.
- Loop: the prompt carries the merged input's settings; a toggle syncs
  when idle and during a run without ending it; two quick toggles send the
  second after the first answer; a prompt waits for a pending yolo, and a
  refused yolo ends the run with no prompt; `/new` with the agent's yolo
  default on and the disable answer withheld sends no prompt until it
  comes; an answer from before `/new` is dropped.
- App: seeding before a `--prompt` submission; `/fast` and `/workflow`
  unavailable without the extension or the option, `/yolo` without its
  option; no local clamp; the badge and fast indicator follow the agent's
  effective values after a model change; a refused fast reverts after an
  unrelated workflow toggle, and not after a newer fast toggle.

## Checks

Full set: `cargo fmt`, workspace clippy with `-D warnings`,
`cargo nextest run --workspace`. A live run against `maki acp` over stdio
toggles fast and yolo, exits, and a resume shows them restored.
