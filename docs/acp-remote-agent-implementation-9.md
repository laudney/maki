# Remote agents over ACP: implementation, step 6h

Status: proposed. Step 6h of the [design](acp-remote-agent-design.md)
turns `/btw` on for a remote tab: a side question answered from the
session's transcript, with no tools, outside the session.

## What the local TUI does

- `/btw <question>` opens the btw modal and spawns `run_btw` with the
  tab's provider and model. It works during a run.
- `start_btw` copies the live history mirror, closes dangling tool calls
  with `UNAVAILABLE_RESULT`, takes the session frame's prefix as the
  system prompt (or `BTW_FALLBACK_SYSTEM` without a frame), and appends
  `btw_question`: the `BTW_REMINDER` and the question as one user message.
- `run_btw` streams one request with no tools and default options, and
  forwards text and thinking deltas to the modal as `BtwEvent`s, then
  `Done` or `Error`. The answer is not added to the session and not
  billed.
- Closing the modal drops the receiver; the stream runs to its end.
- `run_btw` keeps its own provider event sender while it waits for the
  forwarder to see that channel close, so the wait never ends and the
  modal never gets `Done` or `Error`. The shared core fixes this.

Today a remote tab refuses `/btw`: the transcript and the model are the
agent's.

## Shared core

The request building and the stream move from `maki-ui/src/app/btw.rs`
into `maki-agent` (`btw` module), so both hosts run one implementation:

```text
pub enum SideAnswer { Text(String), Thinking(String) }

pub async fn side_question(
    provider: &dyn Provider,
    model: &Model,
    snapshot: Option<&HistorySnapshot>,
    question: &str,
    session_id: Option<&SessionRef>,
    answer: &flume::Sender<SideAnswer>,
) -> Result<(), AgentError>
```

It builds the request exactly as `start_btw` does today. The stream
future owns the provider event sender and drops it as soon as
`stream_message` returns, so the forwarder drains what is left and ends,
and the call returns the stream's result. Forwarding stops when `answer`
is closed. The local App keeps its modal and maps `SideAnswer` to
`BtwEvent`, then `Done` or `Error` from the result.

## The btw extension

Negotiated in `initialize` like the other Maki extensions: key `btw`,
version 1, read into `Agreed { render, usage, settings, btw }`.

- **Request.** `_maki/btw`, params `{ sessionId, question }`. The agent
  answers
  `{}` when the stream ends, or an error (`invalid_params` for an empty
  question, `resource_not_found` for another session, `internal_error`
  with the provider's message for a failed stream).
- **Progress.** Before the answer, the notification `_maki/btw/chunk`,
  params `{ requestId, text }` or `{ requestId, thinking }`, once per
  delta, where `requestId` is the JSON-RPC id of the request. The id is
  unique among a connection's outstanding requests, so no second id is
  needed.

ACP reserves names that start with `_` for extensions, and an agent
without the extension answers the request `method_not_found`; the client
only sends it when `btw` was agreed.

## Server

`_maki/btw` is answered off the request loop, so it runs during a turn
and never delays the loop, also not by a slow provider setup: the loop
checks the question and the session, takes `live_snapshot` for it and
the session's resolved model, and spawns a task at once. The task
connects a provider for that model, calls `side_question`, sends each
`SideAnswer` as a chunk on `out_tx`, then the answer.

The server keeps the tasks, pruning finished ones when it spawns
another, and drops them when the connection ends, before it waits for
its writer, so a stalled provider cannot hold `out_tx` open. A replaced
or closed session does not stop a task; its client is still waiting for
the answer.

## Client

- `AcpClient::btw(session, question, chunks: flume::Sender<SideAnswer>)
  -> Result<(), ClientError>` sends the request for `session`, which the
  App reads with `bound_session` when it checks the binding, so a `/new`
  before the call is first polled cannot redirect it, with
  `Binds::Chunks(chunks)`, so the sink lives exactly as long as the
  request's waiter: the answer, a failed send and the end of the link
  all remove it, and a guard in `btw` removes it when the call future is
  dropped first. Only `btw` takes the guard: a routed request's caller
  does not wait for its answer. The reader routes each `_maki/btw/chunk` to the sink of
  the waiter its `requestId` names, before it resolves the answer that
  follows, so every chunk is forwarded before the call returns. The
  caller gives an unbounded sink, so the single reader never blocks on a
  modal that is not read; a chunk for no waiter is dropped. A chunk is
  sanitized like a session update before it reaches the sink.
- **Session generation.** `/new` replaces the session in the loop, while
  the client stays bound to the old one until the new session's answer.
  `AgentHandles` of a remote tab share a `SessionBinding { requested,
  bound }` of two counters: `rebind` bumps `requested` and puts the new
  value in the `Rebind`, and the loop stores it in `bound` once the new
  session is installed. A failed replacement leaves `bound` behind: the
  loop then ends the tab's link, and until the App applies that, a
  `/btw` is refused, not sent to the old session. A `/btw` while `bound`
  is behind flashes `SESSION_OPENING`, so it never asks the old
  conversation.
- `AgentHandles` of a remote tab keep the `Arc<AcpClient>`. For a remote
  tab, `Action::Btw` calls `App::start_remote_btw`, which opens the modal
  and spawns the call, mapping chunks to `BtwEvent` and the result to
  `Done` or `Error`, as the local path does.
- `AgentBackend::Remote` gains `btw`, from the agreed extension;
  `Feature::Btw` is supported when it is set, so `/btw` without the
  extension still flashes `UNAVAILABLE_REMOTE`.

## Tests

- Agent: `side_question` builds the request from the snapshot (frame
  prefix or fallback system, closed dangling calls, the reminder) and
  forwards deltas; the existing `btw.rs` tests move with it.
- Extensions: `btw` agrees alone and with the others.
- Server: `_maki/btw` for the running session sends chunks then `{}`
  without blocking a following request; another session is refused; an
  empty question is refused.
- Agent: a finite answer ends with the result while the answer receiver
  is still open, for success and for failure.
- Server: a pending provider connection does not delay another request;
  the end of the connection drops a pending side task, and `serve`
  returns.
- Client: chunks reach the sink before `btw` returns; a chunk for an
  unknown id is dropped; an error answer returns the error; an unread
  sink does not delay an unrelated answer; dropping a pending call
  removes its waiter while the link stays open.
- Loop: `bound` follows `requested` after a replacement and stays behind
  after a failed one; `/btw` between `/new` and the new session's answer
  is refused.
- App: `/btw` on a remote tab with the extension yields `Action::Btw`,
  without it flashes `UNAVAILABLE_REMOTE`.

## Checks

Full set: `cargo fmt`, workspace clippy with `-D warnings`,
`cargo nextest run --workspace`. A live run against `maki acp` over stdio
asks `/btw` during a turn and after it.
