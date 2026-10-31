# Remote agents over ACP: implementation, step 6n

Status: proposed. Step 6n of the [design](acp-remote-agent-design.md)
turns plan approval on for a remote tab: when the agent finishes a plan,
the tab shows the plan form, and its rows implement, clear and implement,
refine, or edit the plan, which stays a file on the agent.

## What the local TUI does

- Entering plan mode allocates a plan file in the client's state dir
  (`PlanState::Drafting(path)`), which the prompt's `AgentMode::Plan`
  names.
- A finished write to that path in plan mode (`ToolDone` whose
  `written_path` is the plan path) marks the plan ready and opens the
  plan form (`offer_plan_form`); the `ui.plan_form*` chains may change its
  rows when a plugin layers them.
- **Implement** reads the file, draws it in the chat as a plan message,
  switches to build mode and sends "Implement the plan at `<path>`" (with
  the parallel hint when toggled). **Clear and implement** does the same
  in a new session. **Refine** dismisses the form.
- **Edit plan** (`Ctrl+O`, or the form's editor key) opens `$EDITOR` on
  the file, with the TUI suspended.

## Today on a remote tab

The agent owns the plan file: `maki acp` allocates it in its own state
dir on the first `session/set_mode` to plan of a session and keeps it for
the session (`keep_plan_path`). A remote tab enters plan mode without a
path, so no write is recognized as the plan's, the form never opens, and
`Ctrl+O` is refused.

## Design

The client never opens, reads or writes an agent path on its own disk:
every path below is an opaque string on the client, and every byte of
the plan crosses the link.

### Wire: the plan extension

A Maki extension `plan`, version 1, in `Agreed.plan`.

- **The plan write.** When a call of a plan-mode prompt succeeds and its
  `written_path` is the session's plan file, `maki acp` adds `_meta.maki.
  plan = { path, text }` to that call's completed `tool_call_update`:
  `path` as the agent names the file, and `text` the file's content after
  a full `write`, omitted for another tool or past `PLAN_TEXT_LIMIT`
  bytes. It is sent whatever the render extension does, so a dropped or
  unagreed render snapshot does not hide the plan.
- **`_maki/plan/read`**, params `{ sessionId }`, answers `{ path, text,
  revision }`: the content, `text` `null` when the file does not exist
  yet, and `revision` a hash of the content (`null` with it). An error
  when the session has no plan path.
- **`_maki/plan/write`**, params `{ sessionId, text, revision }`,
  replaces the plan file's content by an atomic write and answers `{}`,
  only when no prompt of the session runs and the file's revision is
  still `revision`; otherwise it answers a busy or a conflict error. The
  agent's own tools write the plan file only inside a prompt, through
  `maki.fs.write`, so no lock can share with them; refusing a write while
  a prompt runs excludes them instead. The check and the write run in the
  request handler that also starts prompts, so no prompt starts between
  them. It writes only the session's own plan path,
  never a path the client names, and errors when the session has none.

### Client

- **Learning the plan.** The remote loop reads the meta on the completed
  update and keeps `{ path, text }` under that call's id in a map shared
  with the App, before the mapper emits the call's `ToolDone`. The App, on
  a remote tab with the extension, takes the entry of a `ToolDone` when it
  handles that event, never at a tick, so the order of the channel holds
  and a later plan write cannot replace an earlier one's entry: when the
  event has an entry and the mode is plan, it adopts the
  path as the plan, pushes the plan message with the slot's text (empty
  without one, as a local edit gives), and marks the plan ready, which
  opens the form as locally. The chat is handed no plan path on a remote
  tab, so it never reads the file on the client's disk, and
  `plan_snapshot` answers the adopted text instead of reading one. The
  text of a session update is sanitized as every update is.
- **`/new` and resume.** The loop clears the map when it installs a new
  session, as the App's `reset_session` clears its plan; an entry the App
  did not take, because its run's events went stale, goes with it.
- **The form.** The built-in rows only: `maki connect` loads no plugin,
  so the `ui.plan_form*` chains are not asked, and no plugin is ever
  handed an agent path.
- **Reading the plan.** `RemotePlan` holds the session link (the
  binding-current session, as `/btw` uses it). Implement and Clear and
  implement read the text with `_maki/plan/read`, bounded by a timeout,
  and draw a sanitized copy of it as the plan message as locally; an
  answer is not an update, so the client does not sanitize it on
  receipt, and the editor below gets the text exactly as read. A failed read flashes its
  error and still sends the implement prompt, since the agent reads its
  own file; the chat then shows no plan message.
- **Editing the plan.** `Ctrl+O` and the form's editor key yield
  `Action::EditRemotePlan`. The event loop reads the text and revision,
  writes the text to a temporary file of the client, opens `$EDITOR` on
  it exactly as `Action::OpenEditor` does, reads it back, and when it
  changed sends it with the revision it read. It flashes the outcome:
  saved, unchanged, or why it failed (no session while a `/new` opens, the
  read, the editor, a conflict because the agent changed the plan
  meanwhile, a prompt running, the write). The temporary file is removed when the edit was
  saved or left unchanged; after a conflict or a failed write it is kept,
  and the flash names its path, so the edit is never lost.
- **Without the extension** a remote tab keeps today's behavior.

## Limits

- Resume: `maki acp` starts a loaded session in build mode with no plan
  path, so a resumed remote tab does not get its plan back, and its next
  plan mode drafts a new file; the old one stays on the agent. A local
  resume restores both. This stays a ledger row of its own.
- The plan message after an `edit` tool's write is empty, as locally, and
  after a `write` past `PLAN_TEXT_LIMIT` bytes too; Implement still draws
  the text it reads.
- The agent's plan file is per agent session: after Clear and implement,
  the new session reads the old session's file by the path the prompt
  names, as a local new session does.

## Tests

- Extensions: `plan` agrees alone and with the others.
- Agent: a plan write's completion carries path and text without the
  render extension, the path alone after an `edit` or past the limit, and
  nothing for another file or in build mode or without the extension;
  read answers text and revision, `null` before the first write; write
  replaces the content at the read revision, refuses a stale one, and
  refuses any while a prompt runs;
  both error without a plan path and for another session.
- Client: read and write round-trip; the loop keeps an entry before it
  emits the call's `ToolDone`, two plan writes keep two entries, and
  `/new` clears them.
- App: the plan write's `ToolDone`, handled with no tick between, opens
  the form and draws its entry's text, also when a second plan write's
  entry is already in; the read text is drawn sanitized; a client file at the same path with
  other content is never read, by the chat or by `plan_snapshot`;
  Implement draws the read text and sends the prompt, also after a failed
  read without the plan message; `Ctrl+O` yields `Action::EditRemotePlan`
  under the extension and is refused without it.
- Event loop: an edited copy is written back with its revision and then
  removed, an unchanged one is not written, and after a conflict or a
  failed write the copy is kept and named; a failed read writes nothing.

## Checks

Full `just ci`. A live run of `maki connect -- maki acp` drafts a plan in
plan mode, sees the form, edits the plan in `$EDITOR` and implements it;
the agent's file holds the edit.
