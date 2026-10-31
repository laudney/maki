# Remote agents over ACP: implementation, steps 6a and 6b

Status: proposed. Step 6 of the [design](acp-remote-agent-design.md) turns
parity ledger rows on, one carrier at a time. This document builds the two
rows whose carriers `maki acp` already serves with standard ACP:

- **6a: mode.** `session/set_mode`, the modes the agent advertised and its
  `CurrentModeUpdate`.
- **6b: exact permission options.** The prompt shows the agent's own
  options and answers with the chosen option id. This replaces the interim
  mapping by option kind from [steps 3 to 5](acp-remote-agent-implementation-2.md).

Model switching is not in this document. The existing model picker
resolves `provider/model` against local provider configuration, drops
providers it does not know, and offers tier assignments that write local
storage. A remote model needs a picker that keeps the agent's opaque
values and labels, so it is its own step.

Later rows (session load and list, compaction, tasks, MCP, the UI Lua
runtime) need either server extensions or the plugin split and get their
own documents.

## 6a: mode

### The mode travels with the prompt

A local prompt already carries its mode: `AgentInput.mode`, captured at
submit time, and the queue treats it as a run boundary (`batch_key`), so
a queued message keeps the mode it was typed in. A remote prompt uses the
same carrier. There is no control channel and no settings projection in
the App:

- Tab on a remote app toggles `state.mode` at once, as it does locally.
- `enter_plan` on a remote app sets `Mode::Plan` without allocating a
  plan path. `agent_mode()` for a remote app returns `AgentMode::Plan`
  with an empty path instead of asserting a local plan file. The path is
  never read: the remote loop sends only the mode id, and plan file UI
  (plan toggle, plan editor, plan panel hint) stays off for a remote app.
- The status bar label and color are the local ones, because the local
  mode is the mode the next prompt asks for.

### The loop matches the agent before each prompt

Mode matching runs only for an agent that advertised both `build` and
`plan` (`RemoteAgent::plan_modes`, read from the `session/new` modes). An
agent without that pair, or without modes, gets its prompts unchanged.

The remote loop tracks the agent's current mode id. It sets it from the
`session/new` response, from every `CurrentModeUpdate`, and from a
confirmed `session/set_mode` (below). Before it sends a prompt, it drains
the updates already queued, then maps the run's `AgentInput.mode` to an id
(`Build` to `build`, `Plan` to `plan`). If the agent is in another mode,
the loop sends `session/set_mode` first and waits for the response,
racing client events, answers, teardown and the run's own cancel token.
An Esc during the switch stops watching the token but still waits for
the switch to settle, because the agent may switch anyway and the next run
must start from its real mode. The run then ends as cancelled and sends no
prompt. Right before the prompt goes out the token is checked once more,
so a cancel that lands together with the switch response still wins.
Every way the preflight ends a run (failure, cancel, mismatch) also
cancels the asks the agent sent during it, as a run end does.

- Success: the loop takes the requested id as current, unless a
  `CurrentModeUpdate` arrived while the request was pending; then the
  latest update wins. So a confirmation never overwrites an update the
  agent sent after it.
- Failure: the run ends with `AgentEvent::Error` naming the mode, and the
  prompt is not sent.
- Last check: right before the prompt goes out, after any switch and the
  drain that follows it, the tracked mode must equal the run's mode. If an
  update said otherwise, the run ends with `Error(MODE_NOT_SET)` and the
  prompt is not sent. Plan mode is a permission boundary, so a prompt never
  runs in a mode the user did not ask for.

The Maki server keeps one plan path per session: the first `plan` mode
allocates it and every later `plan` mode reuses it (`keep_plan_path`), so
plan, build, plan returns to the same plan file, as in the TUI.

Ordering follows from the queue: each run is sent with its own mode, in
queue order, and a run of merged messages shares one mode by
construction. Rapid Tabs only change the local mode, whose last value the
next submission captures.

### Capabilities

`AgentBackend::Remote` gains `plan: bool`, true when the agent advertised
both `build` and `plan` mode ids. `ModeChange` is supported for a remote
backend with `plan`; without it Tab flashes `UNAVAILABLE_REMOTE`. Other
mode ids an agent may advertise are not selectable from the client; that
needs a mode picker and is a later step.

`AcpClient` gains `set_mode(mode_id)`, typed with the schema structs.

## 6b: exact permission options

### Event

`AgentEvent::PermissionRequest` gains one additive field:

```text
#[serde(default, skip_serializing_if = "Vec::is_empty")]
choices: Vec<PermissionChoice>
pub struct PermissionChoice { pub id: String, pub label: String, pub allow: bool }
```

Empty means a local request. `PermissionChoice` lives in `maki-agent`
beside `PermissionAnswer`. Every existing constructor passes an empty
list, so local behavior and serialized local events do not change.

### Prompt

With choices, the prompt shows the agent's labels as a list in the
agent's order, with a cursor. The view scrolls so the cursor's line, with
its wrapped rows, stays on screen. `Up` and `Down` move it, `Enter` picks the
option under it, a digit `1`..`9` picks that option directly, and `Esc` or
`Ctrl+C` cancels. Every option is reachable, however many there are. The local rows (`y`, `a`, `A`, `s`, `n`, `d`,
`D`) and the deny guidance editor do not appear. The answer is
`PromptAnswer::Remote(Some(option_id))` or `Remote(None)` for a cancel.
A local request answers `PromptAnswer::Local(PermissionAnswer)` exactly as
today.

The App sends a remote answer as
`remote_loop::encode_reply(request_id, option)`: the request id, a
separator, then a selected tag with the option id or a cancel tag. The tag
keeps an empty option id, which the schema allows, distinct from a cancel. The remote loop decodes
it and checks the option against the ask's offered ids before it removes
the ask. An offered id is sent; a cancel replies cancelled; an id the
agent did not offer sends nothing and keeps the ask until its run ends. The interim `offered_option` mapping and
`RemoteAgent::always_is_session` are removed.

An ask with no offered option is answered cancelled at once and shows no
prompt, so an empty list can never fall back to the local choices.

### Tests

- Prompt: a remote request shows each label and no local row; a digit
  answers that option; with ten options `Down` nine times and `Enter`
  answer the tenth; `Esc` cancels; a digit beyond the list does nothing; a
  local request is unchanged.
- Remote loop: an answer selects the exact offered id, including two
  options of one kind; an unoffered id sends nothing and the ask stays open;
  a cancel replies cancelled; an ask with no options is cancelled without
  an event.
- Serde: a `PermissionRequest` without choices serializes as before.

## 6a tests

- Remote loop with a scripted peer: a plan run against an agent in build
  sends `session/set_mode` with `plan` before the prompt; a build run
  against an agent in build sends no `set_mode`; a failed `set_mode` ends
  the run with `Error` and sends no prompt; a `CurrentModeUpdate` that
  contradicts the switch stops the prompt; Esc during the switch ends the
  run cancelled with no prompt; an agent without the pair gets no switch;
  a run queued in plan then a run queued in build reach the peer as
  switch, prompt, switch, prompt.
- Server: plan, build, plan keeps the first plan path.
- App: Tab on a remote app with `plan` toggles the local mode without a
  plan path; without `plan` it flashes `UNAVAILABLE_REMOTE`; a remote plan
  submit builds `AgentMode::Plan` input without asserting.

## Checks

Full set: `cargo fmt`, workspace clippy with `-D warnings`,
`cargo nextest run --workspace`. A live run against the Pantheon listener
toggles the mode.
