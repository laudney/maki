# Remote agents over ACP: implementation, step 6c

Status: proposed. Step 6c of the [design](acp-remote-agent-design.md)
turns the model row of the parity ledger on. It builds on
[steps 6a and 6b](acp-remote-agent-implementation-3.md) and uses the same
rules: the agent owns the setting, the loop is the only client code that
talks to it, and a change is checked right before the prompt it affects.

The carrier is the standard ACP select config option for the model. `maki
acp` sends it in the `session/new` response, sends `ConfigOptionUpdate`
when model discovery finds more models, and answers
`session/set_config_option` with the new options.

## The menu and the wanted model

`maki-ui/src/agent/remote_models.rs`:

```text
pub(crate) struct ModelMenu {
    pub(crate) config_id: String,          // the option's id
    pub(crate) current: Option<String>,    // current value id
    pub(crate) choices: Vec<ModelChoice>,  // in the agent's order
}
pub(crate) struct ModelChoice { pub(crate) value: String, pub(crate) name: String }
pub(crate) struct ModelPick { pub(crate) config_id: String, pub(crate) value: String }
pub struct RemoteModels {
    pub(crate) menu: Arc<ArcSwapOption<ModelMenu>>,   // absent: no model option
    pub(crate) wanted: Arc<ArcSwapOption<ModelPick>>,
}
```

The model option is the select option whose category is `model`, else the
one with id `model`. Grouped options are flattened in the agent's order.
Every remote tab has a `RemoteModels`. Its menu is absent while the agent
offers no model option, and every `ConfigOptionUpdate` carries the full
option set, so an update can add the menu or remove it.

- **Writers of `menu`:** only the remote loop, from the `session/new`
  options, from every `ConfigOptionUpdate`, and from a
  `session/set_config_option` response. Each write records the wire
  sequence number of the line it came from (see Ordering).
- **Writers of `wanted`:** only the App, when the user picks a model. A
  pick names the option it came from. The loop takes it with an atomic
  swap, so a later pick waits while an earlier switch settles.

The values stay opaque. The client never resolves a value with
`Model::from_spec`, never reads local provider configuration and never
writes model tiers or recent models.

## Ordering

Responses and updates reach the loop on different channels, so their
arrival order in the loop is not their wire order. The client numbers
every incoming line in wire order: `ClientEvent::Update(seq, update)`, and
`set_mode` and `set_config_option` return `Sequenced { seq, value }`. The
loop keeps the sequence number of the last write to the mode and to the
menu, and a response or an update writes only when its number is higher.
An update the agent sent before its answer is older and loses to the
answer. An update sent after it is newer and wins, even when the loop
reads it first. The mode switch of step 6a uses the same rule.

The `session/new` answer seeds both: `new_session` returns `Sequenced`,
and `RemoteAgent::session_seq` starts the loop's mode and menu sequence
numbers at it. The reader binds the session while it handles that answer,
before it reads the next line, so an update the agent sends right behind
the answer is kept rather than dropped as belonging to no session.

## The picker

`maki-ui/src/components/remote_model_picker.rs` wraps `ListPicker` like
the theme picker does. An item shows the agent's name, and the value as
its detail when the two differ. The picker remembers the config id of the
menu it opened from, and a pick returns a `ModelPick`. `/model` and the
model picker key open it for a remote app with the latest menu and the
current value selected; with no menu they flash `UNAVAILABLE_REMOTE`. A
remote app never opens the local picker. A pick stores `wanted` and
flashes that the model switches with the next prompt. It has no tier keys
and starts no discovery.

The status bar names the menu's current model, so it shows what the agent
confirmed. The local display model is not renamed. The App watches the
menu slot with the existing `Watch` in `tick`, so a menu the loop
publishes repaints an idle tab, and the view renders from the watched
copy.

## The switch before the prompt

The preflight runs both switches first and the checks after both, because
each switch drains updates that can change what the other one checked:

1. Drain queued updates.
2. The mode switch of step 6a, when needed.
3. The model switch: take `wanted`. If its config id is not the latest
   menu's, or its value is not among the menu's choices, the run ends with
   `Error(MODEL_NOT_SET: MODEL_NOT_OFFERED)` and nothing is sent. If the
   value is already current, nothing is sent. Otherwise the loop sends
   `session/set_config_option` and settles it as the mode switch is
   settled: racing client events, answers, teardown and the run's cancel
   token, draining queued events before it reads the result, still
   awaiting the request after an Esc, and refusing asks while a cancelled
   switch settles. A failure ends the run with `Error(MODEL_NOT_SET: ...)`.
4. A cancelled run ends as cancelled.
5. The model check: if the menu's config id and current value are not
   the picked pair, the run ends with `Error(MODEL_NOT_SET)`. The pair, not
   the value alone, because a replaced option can reuse the same opaque
   value for something else.
6. The mode check of step 6a.

No prompt is sent when the run ends at any step. `wanted` stays cleared,
so a refused model is not retried on the next prompt, and the error says
why. Both switches share one generic `settle`.

A model change is not a permission boundary, so a queued prompt runs on
the model wanted when it starts, as a local queued prompt runs on the
model of its turn.

## Capabilities

`ModelChange` is supported for every remote backend, because the menu can
appear at any time. The picker checks the latest menu when it opens.

`AcpClient` gains `set_config_option(config_id, value)`, typed with the
schema structs.

## Tests

- Menu parsing: a model-category option, an option found by id, grouped
  options flattened in order, an agent with no model option.
- Client: sequence numbers follow the wire across updates and a response;
  an update right behind the `session/new` answer is kept.
- Remote loop with a scripted peer: a pick different from the current
  model switches before the prompt and the response updates the menu; a
  pick equal to the current one sends nothing; a `ConfigOptionUpdate`
  replaces the menu, and one without a model option removes it; a failed
  switch and a response that keeps another model end the run with no
  prompt; Esc during the switch ends it cancelled with no prompt; an
  update older than the answer loses and a newer one wins; a mode update
  during the model switch stops the prompt; a pick from a replaced option
  is refused unsent; an answer that swaps in another option with the same
  value sends no prompt; `wanted` is cleared in every case.
- Picker: it lists the agent's names in order, selects the current one,
  and a pick carries the option's config id.
- App: `/model` on a remote app with a menu opens the remote picker and
  never the local one; without a menu it flashes `UNAVAILABLE_REMOTE`; a
  pick stores `wanted` and leaves the local model alone; a published menu
  repaints an idle tab once and renames the shown model.

## Checks

Full set: `cargo fmt`, workspace clippy with `-D warnings`,
`cargo nextest run --workspace`. A live run against the Pantheon listener
switches the model and checks the reply and the status bar.
