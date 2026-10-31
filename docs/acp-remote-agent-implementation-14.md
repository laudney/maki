# Remote agents over ACP: implementation, step 6m

Status: proposed. Step 6m of the [design](acp-remote-agent-design.md)
turns the `question` tool on for a remote tab: the agent asks, and the
client draws the same form a local tab draws, with the client's own Lua
host running the bundled form module.

## What the local TUI does

The `question` tool is the Lua plugin `plugins/question`:

- `init.lua` registers the tool. Its handler normalizes the questions
  (`header` defaults to `""`, `multiple` comes from `multiSelect`), opens
  the form with `QuestionForm.open(questions)` and waits for it.
- `question_form.lua` is the form: one tab per question under its header,
  the question text as markdown, options with their descriptions, single
  or multi select, a "Type your own answer" row on every question, and
  dismiss. It returns `{ type = "dismiss" }` or `{ type = "submit",
  answers = { {labels or typed text}, ... } }`, with a hole for a skipped
  question.
- The tool result is `QuestionHelpers.format_answers`: one `header:
  answer, answer` line per question (`Q<n>` without a header, `(no
  answer)` for a skipped one), or `(question dismissed by user)`. It also
  returns `state = { answers }` and a card body that shows the picks.

## Today on a remote tab

`maki connect` advertises no elicitation, so `maki acp` takes the
question tool out of the session (`excluded_tools`): the model cannot ask
at all. A client that advertises form elicitation, such as Zed, gets the
tool, and `maki acp` shadows it with a Rust local tool that sends a
standard `elicitation/create` form and formats the reply. That form has no
headers and allows no typed answer.

## Design

### Wire: the question extension

A Maki extension `question`, version 1, in `Agreed.question`. A client
offers it when it can draw the bundled form.

- **Offering the tool.** `maki acp` keeps the question tool, shadowed as
  today, when the client advertised form elicitation or agreed on
  `question`.
- **The ask.** The shadow sends the same `elicitation/create` request.
  When `question` is agreed it adds `_meta.maki.question = { questions }`,
  the tool's `questions` input as the model wrote it. The standard schema
  and message stay, so the request is still a valid form.
- **The answer.** A client under the extension answers `{ action:
  "accept", content: {}, _meta: { maki: { question: { answers } } } }`,
  where `answers` holds one array of strings per question, empty for a
  skipped one, or `{ action: "cancel" }` for a dismissed form.
- **The result.** The agent formats `_meta.maki.question.answers` exactly
  as `format_answers` does, so a typed answer reaches the model as the
  user typed it. Without them it reads the standard `content` as today.
  Both go through one formatter, so the header, `Q<n>`, the `, ` join and
  `(no answer)` cannot drift between the two.

`maki connect` does not advertise standard form elicitation: it draws
only Maki questions. An `elicitation/create` without `_meta.maki.question`
is answered `decline` at once.

### Client: the form

- **Where the form runs.** The client's plugin host gains one request,
  `EventHandle::open_question_form(questions)`. The runtime builds a
  plugin environment whose `require` is rooted in the bundled `question`
  plugin directory, normalizes the questions and runs
  `QuestionForm.open` in a detached command scope, as `RunCommand` runs a
  command. It replies with a typed result, never the raw Lua table:
  dismissed, or submitted with exactly one `Vec<String>` per question,
  where a hole the form leaves for a skipped question, first, last or
  every one, becomes an empty array, as the tool's `dense` does. No plugin is
  loaded and no tool is registered on the client: only the form module
  and the helpers it needs run there. `normalize` moves from `init.lua`
  to `question_helpers.lua`, so the tool and the host call share it.
- **Who asks.** The client's reader turns a Maki question into
  `ClientEvent::Question(QuestionAsk { id, questions })`, with a local id
  mapped to the JSON-RPC id, as a permission ask is. The remote loop opens
  the form for the ask of its live run and replies with
  `AcpClient::reply_question(id, answer)`.
- **One form, one owner.** The agent serializes its asks on the answer
  channel, so one form is open at a time. The form closes and the ask is
  answered `cancel` when its run ends or is cancelled, when the session is
  replaced, and when the link closes. A late answer finds no pending ask
  and is dropped. The open request carries a cancel channel that the
  runtime races against the form.
- **Closing the window.** Neither the scope's teardown nor dropping the
  form's coroutine closes its window: `TaskScope::drop` clears hooks and
  jobs only, and a `WinHandle` closes when Lua collects it. So the
  request's scope records every window `maki.ui.open_win` opens under it,
  and when the request ends without an answer, by cancel or by an error,
  the runtime closes them at once. Other tasks are unchanged.
- **Without a host.** When the client's Lua host is gone, the open
  request fails, and the ask is answered `cancel`, which the model reads
  as a dismissed question. The client never answers for the user.

## Limits

- The tool row on a remote tab shows the tool's text result, not the
  card body: the card is drawn by the agent's Lua tool, which the shadow
  replaces. Replayed question rows show the same text. Both turn into the
  card with the client renderers of the UI Lua runtime.
- The client runs the bundled form, so a user package that replaces the
  `question` plugin changes the agent's tool but not the client's form.

## Tests

- Extensions: `question` agrees alone and with the others.
- Agent: the tool is offered with the extension and no form capability,
  and excluded with neither; the request carries `_meta.maki.question`
  only under the extension; meta answers format like `format_answers`,
  with a typed answer and a skipped question; `cancel` is dismissed.
- Client: a Maki question becomes a `Question` event, a plain form is
  declined; a reply answers the request it was shown for.
- Loop: an ask opens the form and the form's answers reach the agent; a
  dismissed form answers `cancel`; the end of the run, a `/new` and a
  closed link close the form and answer `cancel`.
- Lua runtime: `open_question_form` returns `submit` answers for a
  scripted set of keys, one array per question with the first, the last
  and every question skipped; a cancel and a failing form close the
  window without a garbage collection and end the call.
- Plugin: the existing question tests pass with `normalize` moved.

## Checks

Full `just ci`. A live run of `maki connect -- maki acp` asks the model to
use the question tool, answers one question with an option and one with
typed text, and the model's next reply quotes both.
