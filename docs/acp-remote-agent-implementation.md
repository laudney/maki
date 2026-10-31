# Remote agents over ACP: implementation, steps 1 and 2

Status: proposed. This document builds steps 1 and 2 of the
[design](acp-remote-agent-design.md). Steps 3 and later get their own
implementation text after step 2 shows what the capability changes need.
It supersedes the milestone scope of
[tui-over-acp-implementation.md](tui-over-acp-implementation.md).

Both steps live in `maki-acp`. Neither touches `maki-ui`, the `maki`
binary, or any stored type in `maki-agent`, and neither adds a crate
dependency that the workspace does not already have.

## Step 1: typed stdio client

### Modules

| File | Content |
| --- | --- |
| `maki-acp/src/client.rs` | `AcpClient`, `Link`, `ClientEvent`, `PermissionAsk`, `ClientError` |
| `maki-acp/src/display.rs` | `DisplayMapper`: `SessionUpdate` to `AgentEvent`, the inverse of `translate.rs` |
| `maki-acp/src/lib.rs` | `pub mod client; pub mod display;` |
| `maki-acp/Cargo.toml` | `thiserror = { workspace = true }` (already a workspace dependency) |

`translate.rs` keeps its private `fenced`. It gains `pub(crate) fn
unfenced` and makes `NOTICE_ID_PREFIX` `pub(crate)`, so the server and the
client share one definition of each.

### Link

A link is a pair of channels of whole JSON lines, which is what `flume`
already carries everywhere in this crate. It is not a trait.

```text
pub struct Link { pub tx: Sender<String>, pub rx: Receiver<String>, process: Option<Process> }
```

`Link::new(tx, rx)` builds a channel-only link for a scripted peer.
`Link::spawn(command)` starts a child with `smol::process`, pipes its stdin
and stdout, inherits stderr, and sets `kill_on_drop`. A private `Process`
owns the child and two pump tasks: one writes lines from `tx` to stdin,
one reads lines from stdout into `rx`. `AcpClient` takes the `Process` and
owns it, so dropping the client kills the child and cancels both pumps.

The incoming channel is bounded (`EVENT_CAPACITY`), so a paused client
reader stops the stdout pump and the pipe pushes back on the agent. The
outgoing channel is unbounded; the client writes little. Failure paths:

- stdout EOF or a read error ends the stdout pump, which closes `rx`;
- a stdin write error ends the stdin pump. The stdin pump holds a signal
  that the stdout pump races against its read, so the stdout pump stops
  too and closes `rx`, even when the child keeps its stdout open. The
  next client send returns `ClientError::Closed`;
- a closed `rx` makes the reader fail every waiter with `Closed`, so a
  request already pending when the write side fails is settled too.

### Client

`AcpClient::new(link)` starts one reader task and returns the
client and an event receiver. Requests are async methods:

| Method | Wire |
| --- | --- |
| `initialize(client_info)` | `initialize`, version 1, no filesystem or terminal capability, form elicitation off |
| `new_session(cwd)` | `session/new`; stores the session id |
| `prompt(blocks)` | `session/prompt`; resolves on the response, which carries the stop reason |
| `cancel()` | `session/cancel` notification |
| `reply_permission(ask, outcome)` | response to the agent's `session/request_permission` |

Request ids come from a counter starting at 1. A map from id to a
`flume` one-shot sender wakes the waiting method. Every message is typed
with the schema structs; the reader reads a line as `serde_json::Value`
only to tell a response from a request from a notification, then
deserializes into the schema type.

`prompt` resolves on a response that queues behind the turn's updates.
A caller must therefore drain the event receiver while it awaits
`prompt`. The API documents this obligation, and a test proves that a
prompt with more updates than `EVENT_CAPACITY` completes when the caller
drains, and does not complete while the caller does not.

Reader rules:

1. A response resolves its waiter. An unknown id is logged and dropped.
2. An agent request other than `session/request_permission` is answered
   with a method-not-found error at once.
3. `session/request_permission` becomes `ClientEvent::Permission(ask)`.
   `ask.id` is a local counter value, and the client stores the JSON-RPC
   id and the offered options under it. `reply_permission` takes the local
   id and accepts only an offered option id or a cancel. An unknown or
   already answered ask returns `ClientError::UnknownAsk`; an option the
   agent did not offer returns `ClientError::UnofferedOption` and leaves
   the ask open.
4. `session/update` for the stored session becomes
   `ClientEvent::Update(SessionUpdate)`. An update for any other session is
   dropped. Before `new_session` returns, updates are dropped.
5. A line that is not JSON, or an update that does not deserialize, becomes
   `ClientEvent::Malformed(String)`. The reader keeps going.
6. When the link closes, every waiter fails with `ClientError::Closed`,
   pending permission asks are forgotten (there is no link to answer on),
   and the reader emits `ClientEvent::Closed` once.

The event channel is bounded (`EVENT_CAPACITY`). The reader awaits
`send_async` with no timeout, so a UI that stops draining pauses the
reader and the pipe applies backpressure. A response or permission request
that arrives behind a full buffer is handled after the buffer drains.

### Display mapper

`DisplayMapper` holds the state the inverse needs: tool name by call id
and the last context gauge. `map(update) -> Vec<AgentEvent>`.

Name and summary stay separate. Maki's server puts the tool name in the
pending `ToolCall` title and the summary in the in-progress update title.
The mapper takes the name from the schema `name` field when present, else
from the first `ToolCall` title, and remembers it by call id. A
`ToolStart` always carries the remembered name, because the TUI renames
the row from it.

Every tool update first ensures a row exists: an update for an unknown
call id emits `ToolPending`, named from `name`, else the tool kind, else
`tool`. Then every supplied field is processed, in this order:

1. status in progress: `ToolStart` with the summary and `raw_input`;
2. content with a non-terminal status or none: `ToolOutput` with the text;
3. status completed or failed: `ToolDone`. A diff content becomes
   `ToolOutput::Diff`. Otherwise the text content becomes
   `ToolOutput::Plain`, or, with no text content, a string `raw_output`.

A first `ToolCall` whose status is not pending is processed as an update
after its `ToolPending`. A `ToolCall` with the notice id prefix becomes
`ObservationLine`. `AgentMessageChunk` and `AgentThoughtChunk` become
`TextDelta` and `ThinkingDelta`; a non-text block becomes the text
`[unsupported content]`. `UsageUpdate` stores the context gauge. Every
other update emits nothing and stays readable from `ClientEvent::Update`.

`unfenced` applies only to text content, never to `raw_output`. It inverts
`fenced` exactly and returns any text that is not one complete fence
unchanged, which covers a standard agent's plain text.

`done(stop_reason) -> AgentEvent` builds `Done` with zero usage, no cost,
the stored context gauge and the inverse of `map_done_reason`
(`Refusal` maps to `Dropped`, `MaxTurnRequests` to `MaxTurns`). An RPC
error is the caller's `AgentEvent::error`. The mapper invents no per-turn
token, model or billing data.

### Tests

All tests use `test_case`, shared constants and no sleeps.

- Mapper: a round trip through the server's own `translate` functions for
  a full tool lifecycle, a diff and a failed tool; an already-completed
  first `ToolCall`; unknown in-progress, content-only and terminal
  updates; one update with status and content; distinct name and title;
  notice cards; text, thought and non-text chunks;
  `unfenced(fenced(x)) == x` for several backtick runs, and foreign text
  unchanged; each stop reason both ways.
- Client with a scripted peer on a `Link`: `initialize` then `new_session`
  then `prompt` with updates and a response; a malformed line followed by a
  good update; an update for another session; an agent request for an
  unsupported method gets the error reply; a permission request and a
  reply with an offered id, an unoffered id and a cancel; a permission
  request for another session is cancelled; `cancel` writes the
  notification; link close fails a pending prompt and emits `Closed`
  once; a closed outgoing channel fails a request with `Closed`; a prompt
  with more updates than `EVENT_CAPACITY` completes only while the caller
  drains. The scripted peer answers with the server's own builders
  (`methods::initialize_response`, `methods::new_session_response`).
  Ordering uses channels, never sleeps.
- `Link::spawn` is tested on Unix with `cat`, which echoes a line, and
  with a shell child that closes its stdin, prints one line and keeps its
  stdout open: a request then fails with `Closed` instead of waiting.
- A test against the real `maki acp` binary moves to step 4. `maki acp`
  resolves a model and starts model discovery at startup, which needs a
  provider and the network. Step 4 must isolate both for `maki connect`
  anyway, so the test lives there.

### Checks

`cargo check -p maki-acp --tests`, `just lint` and `cargo nextest run -p
maki-acp`. The change touches no docs generated by
`maki-docgen`.

## Step 2: tool render extension

### Wire types

New file `maki-acp/src/render.rs`, used by the server and the client.

```text
RENDER_VERSION: u32 = 1
META_KEY = "maki"        // _meta.maki.toolRender
pub struct WireSnapshot { pub lines: Vec<Vec<SnapshotSpan>> }
pub struct ToolRender {
    pub header: Option<WireSnapshot>,
    pub body: Option<WireSnapshot>,
    pub output: Option<ToolOutput>,
    pub annotation: Option<String>,
    pub written_path: Option<String>,
    pub dropped: Option<DropReason>,
}
pub enum DropReason { Oversize }
```

`SnapshotSpan`, `SpanStyle`, `InlineStyle` and `ToolOutput` already
implement both serde directions, so no stored type changes. Conversion
functions `WireSnapshot::from(&BufferSnapshot)` and `into_snapshot` live in
`render.rs`. Every field is optional, so a payload carries only what
changed. A body replaces the previous body.

### Negotiation

The client sends `_meta.maki.toolRender = 1` in `initialize`. The server
reads it where it already parses `InitializeRequest`. It turns the
extension on only for an exact match with `RENDER_VERSION`, and answers
with `_meta.maki.toolRender = 1` in `InitializeResponse`. Any other value,
or none, leaves it off and the response carries no field. The client turns
decoding on only when the response echoes exactly its own version. A later
version 2 defines its own compatibility rule.

### Server emission

All emission is in the event pump (`maki-acp/src/server.rs`, the `match`
that today ends in `_ => continue`). With the extension off, nothing
changes. With it on:

| Event | Emission |
| --- | --- |
| `ToolStart` | the existing update plus `header` from `render_header` |
| `ToolHeaderSnapshot` | a `ToolCallUpdate` with the replacement `header` |
| `LiveToolBuf` | register the buffer under the call id |
| `ToolSnapshot` | a `ToolCallUpdate` with the replacement `body`, also after the call finished |
| `ToolDone` | the existing update plus `output`, `annotation`, `written_path`; a registered buffer is read once more with `take`, sent as `body` and unregistered |
| `Done`, `Error` | every registration of the session is cleared |

`ToolSnapshot` is not treated as final: asynchronous Lua work can send
more than one, and after `ToolDone`. The server forwards each as a body
replacement and keeps no state for it. A buffer registration ends at
`ToolDone`, `Done`, `Error` or the end of the event stream, whichever
comes first, so a cancelled or failed run leaves no registration behind.

Live buffers are polled, not watched. `SharedBuf::set_on_change` has one
slot that the Lua layer owns and clears, so the server never replaces it.
While at least one buffer is registered, the pump races `events.next()`
against a timer of `LIVE_BODY_POLL` with `smol::future::or`, a re-export
of `futures-lite` that `maki-acp` already has through `smol`.
`SessionEvents::next` is cancel safe, so the race loses no event. On a
tick the pump calls `read_if_dirty` on each registered buffer and sends a
replacement `body` for each one that changed. Two mutations before one
tick send one body; mutations across two ticks send two. With no
registered buffer the pump has no timer. In `maki acp` the server is the
only reader of the dirty flag.

The tick source is a parameter of the pump. Production passes a
`smol::Timer` interval. Tests pass a channel, so a test sends one tick and
waits for the update without a sleep.

Every payload passes `fits_limits` before sending. The limits are
constants in `render.rs`: `MAX_SNAPSHOT_LINES`, `MAX_LINE_SPANS`,
`MAX_PAYLOAD_BYTES` (serialized). A payload over a limit is replaced with
`dropped: Oversize`, and the update keeps its standard content. The limits
are independent of the model-result limits.

`theme_gen` is not sent. The client emits snapshots with `theme_gen:
None`. In the messages panel `None` means "stamp with the panel's current
theme generation"; it does not exempt a snapshot from a later staleness
check, and a theme change asks Lua to restore stale rows. A remote row has
no local Lua state to restore from. Step 4 (`RemoteAgentLoop`) owns this:
it keeps remote snapshots out of the restore path and tests a theme change
after a completed remote tool.

### Client decoding

`DisplayMapper::with_render(version)` enables decoding. The client
validates every received payload against the same limits and the wire
schema. For each call id the mapper owns an `Arc<SharedBuf>`:

- a `header` becomes the `render_header` of the `ToolStart`, or a
  `ToolHeaderSnapshot` after the start;
- the first `body` creates the buffer and emits `LiveToolBuf`; each later
  `body` calls `set_lines` on it;
- on completion the mapper emits `ToolSnapshot` with the final lines and
  drops the buffer;
- an `output` replaces the reconstructed `ToolOutput` in `ToolDone`, and
  `annotation` and `written_path` fill their fields.

This is the order the local agent already emits, so the TUI needs no
change. The buffers carry no click handler, so rows are static.

Late render updates. The server forwards a `ToolSnapshot` after
`ToolDone`. Such an update carries only the extension: no status, no
content and no title. A render-only update never creates a row, never
emits `ToolPending` or `ToolStart`, and never reopens a buffer. It emits
`ToolSnapshot` or `ToolHeaderSnapshot` by call id, which the TUI applies
to the existing row and ignores for an unknown id. For a finished call
the mapper keeps only one fact: whether it fell back (below). It keeps no
standard view of finished calls. An invalid late update for a finished
call that did not fall back is dropped with a notice, and the row keeps
its last valid snapshot. Step 1's rule that every update ensures a row
applies only to updates that carry a standard field.

Fallback. A payload that is marked `dropped`, fails validation or does not
parse must not leave an earlier styled snapshot in place, because the TUI
prefers a snapshot, even an empty one, over plain output. Maki's
completion updates carry no title and a diff completion carries no text,
so the current update alone is not enough. The mapper therefore keeps,
per live call, the latest standard title and the latest standard output.
On fallback it replaces the header with a plain snapshot of the retained
title, or the tool name without one, and the body with plain lines of the
retained output:

- text content: its lines;
- a diff: a `--- path` line, then each removed line with `-` and each
  added line with `+`, from `old_text` and `new_text`;
- a string `raw_output`: its lines;
- nothing: one line with the tool name.

The mapper writes these lines into the call's shared buffer with
`set_lines` before it emits the snapshots, so no later read restores the
styled body. A call that fell back stays plain: its id goes into a set of
fallen-back calls that lives as long as the mapper, and every later
extension payload for it, live or late, is ignored. Fallback is rare, so
the set stays small. The mapper emits one `Notice` for the call.

### Sanitizing

`render.rs` has `sanitize_display(&str) -> Cow<str>`. It removes ESC and
the sequences it starts, the other C0 controls except newline and tab,
DEL and the C1 range. The mapper applies it to every received string
before it builds an event: span text, output text, annotation, written
path, titles, tool names and message chunks. It does not change stored
data. If the TUI already has a function with this behavior, the mapper
reuses it and the commit says so.

### Tests

- Round trip: a snapshot with default, named and inline spans, a header, a
  body and each `ToolOutput` variant survives write, serialize, parse and
  decode; the decoded events equal the local ones.
- With a channel tick: two mutations before one tick send one body;
  mutations across two ticks send two; snapshot A, then mutation B, then
  `ToolDone` sends B as the final body; a `ToolSnapshot` after `ToolDone`
  is forwarded; `Done` and `Error` without `ToolDone` clear the
  registration and stop the timer; a tool that never registers a buffer
  creates no timer.
- Negotiation: the server sends nothing with the extension off; a client
  that did not ask gets the standard update; a client that sent `2`, `0`
  or a non-number gets no extension and no response field; a response
  that echoes a different version leaves client decoding off.
- Limits: a payload over each limit yields `dropped` and keeps the
  standard content. On the client, a valid styled body followed by each
  of these replaces the header and body with plain snapshots and one
  notice: a dropped text completion, an oversized diff completion with no
  title, a malformed completion, and an invalid render-only update. After
  the fallback the shared buffer holds the plain lines, and a later valid
  payload for the call is ignored.
- A late render-only update after `ToolDone` emits a snapshot for the
  existing id and no `ToolPending`.
- A valid styled body, then an oversized completion, then a valid late
  render-only body: the late body is ignored, and no pending, start or
  live-buffer event follows the fallback.
- An invalid late render-only update for a call that did not fall back
  emits one notice and no snapshot.
- The events above are checked at the mapper. Their effect in the real
  messages panel (one completed row, plain output after a fallback) is
  tested in step 4, where `maki-ui` first consumes the mapper.
- Sanitizing: ESC sequences, C0, DEL and C1 are removed; newline and tab
  stay; a diff and a code listing keep their layout.
- A server test drives the real pump with a registered `SharedBuf` and
  the channel tick source.

### Checks

The full set: formatter, `just lint`, `just test`. The change touches the
server, so the existing ACP server tests must pass unchanged. No generated
docs change.

## Constants and files

| Constant | Where | Note |
| --- | --- | --- |
| `EVENT_CAPACITY` | `client.rs` | bounded event buffer |
| `RENDER_VERSION`, `META_KEY` | `render.rs` | |
| `MAX_SNAPSHOT_LINES`, `MAX_LINE_SPANS`, `MAX_PAYLOAD_BYTES` | `render.rs` | chosen when step 2 is written, with a measured typical size |
| `LIVE_BODY_POLL` | `server.rs` | production tick interval, also the rate limit |

Files touched: step 1 adds `client.rs` and `display.rs`, and edits
`lib.rs`, `translate.rs` and `Cargo.toml`. Step 2 adds
`render.rs` and edits `server.rs`, `display.rs`, `client.rs` (negotiation)
and `lib.rs`.

## Open items resolved during implementation

- The exact values of the wire limits, from a measured typical payload of
  the built-in tools.
