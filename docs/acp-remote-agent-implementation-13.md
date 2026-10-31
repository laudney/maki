# Remote agents over ACP: implementation, step 6l

Status: proposed. Step 6l of the [design](acp-remote-agent-design.md)
keeps a remote tab's input history, unsent draft and queued messages
across runs of `maki connect`, as a local tab keeps them.

## What the local TUI does

- **Input history** is per project: `InputHistory::load(state dir, cwd,
  size)` reads it from the user's state dir, and `save_input_history`
  writes it back on quit and on `/cd`.
- **Draft and queue** live in the session's meta: every frame
  `checkpoint` builds `input_draft` (the main chat's unsent input) and
  `queued_messages` (the queue's text, or the queue a lost run handed
  back) into the meta, and writes it at most once per `SOFT_SAVE_DELAY`
  when only they moved; `checkpoint_now` writes at once on shutdown and
  when the session is swapped out. A resume restores the draft into the
  input box (`restore_display`) and puts the queued messages back in the
  queue (`flush_restored_queue`), which runs them.

## Today on a remote tab

`maki connect` runs the tab on a scratch state dir, a `TempDir` removed
at exit, because the agent owns the session. So the input history it
loads and saves is the scratch one and is lost at exit, and `checkpoint`
returns early for a remote tab, so the draft and queue are never written.

## Design

The client keeps its own state, separate from the agent's sessions and
from anything the agent accepted.

- **Where.** The user's real state dir, `StateDir::resolve()`, read by
  `maki connect` next to the scratch one. When it cannot be resolved the
  tab runs as today and keeps nothing.
- **Whose.** Everything lives under `remote/<agent>/`, where `<agent>` is
  a hash of the agent's identity and launch context: for a spawned agent
  its program, arguments and the client's working directory; for a URL
  the URL and the name of the token variable (never the token, which
  rotates). No remote history mixes with a local project's, and agents
  launched differently never share a file (see Limits for what the key
  cannot tell apart).
- **Input history.** `InputHistory` loaded from an explicit file,
  `remote/<agent>/history-<workspace>.json`, where `<workspace>` hashes
  the workspace path `maki connect` sends (`--cwd` or the current
  directory). The path is opaque here: the client does not look for a
  project root on its own disk for a workspace that may live on another
  machine. It is saved where it was loaded, as today.
- **Draft and queue.** A new `maki_storage::remote_drafts` store: one
  small JSON file per remote session, `remote/<agent>/<session>.json`,
  holding `{ input_draft, queued_messages }`. `<session>` is a hash of the
  remote session id, which is an opaque string from the agent and never a
  path component. A file with an empty draft and an empty queue is
  removed. Writes go to a temporary file renamed into place, so a crash
  leaves the old file or the new one.
- **One owner.** A tab holds a lock on the session's sibling
  `<session>.lock` file (`maki_storage::lock::Lock`, as a session claim
  does), which is never renamed or removed, so replacing or removing the
  JSON record never moves the lock to another file. It takes the lock
  before it restores and keeps it for as long as it is bound to the
  session. A tab that cannot
  take it, because another `maki connect` holds that session's record,
  neither restores nor writes it, and keeps its input in memory only.

### Saving

`checkpoint_with` on a remote tab builds the draft and queue as
`build_meta` does and, when they differ from what it last wrote, writes
them. A change to the queue is written at once, whatever its length now:
a dispatched message must leave the file before a crash could replay it,
as a local dispatch reaches disk at once with the message it appends, and
a dispatch can be hidden by a new message queued in the same frame. A
change to the draft alone waits for the soft delay, as locally, and
`checkpoint_now` writes at once.

The session it writes for is the one the client is bound to,
`bound_session()`:

- while a `/new` is opening (the binding is behind and the link is up),
  it writes nothing, so nothing lands in the session the user left;
- once a replacement is installed, the next write goes to the new
  session, under its lock;
- when a replacement failed, which ends the link and turns the tab
  disconnected, the client is still bound to the old session, whose lock
  the tab still holds until a replacement is installed, and the input the
  lost run handed back is written there, so quitting keeps it.

### Restoring

When `maki connect` loads a session (`-c` or `-r`) and takes its record's
lock, the record seeds the tab's placeholder meta. The placeholder has no
transcript, so `spawn_runtime` does not take the local resume path, and
`restore_display` would draw that empty transcript over the replay. A
remote tab instead runs only the input half of a resume, extracted from
`restore_display` as `restore_input`, once the replay is drawn and before
its first checkpoint or initial prompt: the draft into the input box, and
the queue back into the queue (`flush_restored_queue`), which runs it as
a local resume does. A new session restores nothing.

## Limits

- The queued messages are kept as text, as a local session keeps them; a
  message's images are not kept, as locally.
- A message dispatched in the frame before a crash can still be replayed
  on resume, as locally.
- The key cannot see what the environment selects: two accounts behind
  one URL with the same token variable, or one command and directory whose
  environment points at another backend or store, share a namespace. Their
  records only meet if the agents reuse a session id, and the lock keeps
  two clients off one record either way.

## Tests

- Storage: a draft and queue round-trip per agent and session; two agents
  never share a file; a session id with `..`, `/`, `\` or an absolute path
  stays inside the store; an empty one removes the file; a write replaces
  the file whole; a second lock on a record is refused, also after the
  record was replaced or removed.
- App: a remote tab's checkpoint writes its draft and queue for the bound
  session; a draft change once per soft delay, a queue change at once,
  also when a dispatch and a new message keep its length, and everything
  at once from `checkpoint_now`; it writes nothing while a `/new` is opening and writes the
  old session after a failed one; an emptied draft removes the file; a
  tab without the lock writes nothing.
- Connect: a loaded session's file seeds the tab, and the startup
  restores its draft and queue before the first checkpoint while the
  replayed conversation stays drawn; a new session's does not; the
  agent key differs for the same argv in another directory and for
  another token variable; the input history file is the agent's and the
  workspace's.

## Checks

Full `just ci`. A live run types a draft and queues a message during a
run, exits, and `maki connect -c` brings both back.
