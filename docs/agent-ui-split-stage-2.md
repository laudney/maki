# Agent and UI split: stage 2, links

Status: proposed. Stage 2 of the [implementation
design](agent-ui-split-implementation.md): the ACP server runs over any
link, not only stdio, and an in-memory link pair joins a client and a
server in one process. `maki acp` keeps stdio, and its behavior does not
change.

## Today

- `serve(params)` (`maki-acp/src/server.rs`) builds the `Server` around
  two channels. `out_tx: Sender<Value>` carries what the server sends,
  and `Incoming::Line(String)` carries what it reads. Two tasks tie them
  to stdio: a writer that serializes each `Value` to stdout with a
  newline, and `read_stdin`, which reads lines until EOF.
- The client's `Link` (`maki-acp/src/client.rs`) is a channel pair of
  lines, `tx: Sender<String>` and `rx: Receiver<String>`. `Link::new` takes
  bare channels. `Link::spawn` and the WebSocket link add pumps that frame
  those lines on a byte stream. `AcpClient::shutdown` sends
  `END_OF_INPUT` (`""`), and the stdio pump turns it into closing the
  child's stdin.

## Design

- **One core.** `serve_channels(params, input: Receiver<String>, output:
  Sender<String>)` is today's `serve` without the two stdio tasks. It reads
  one JSON-RPC message per received string, and stops reading when the
  input disconnects or carries `END_OF_INPUT`. It writes each outgoing
  message as one string without a newline. Its shutdown is today's: it
  closes the session, ends the writer, and drops `output` last. Stage 2
  changes neither side's shutdown, so what a request still pending at
  shutdown gets depends on the link: `Link::spawn`'s pumps stop reading
  once they pass `END_OF_INPUT`, so it fails with `ClientError::Closed`,
  while the in-memory link has no pumps and keeps resolving replies until
  the server's output closes, so it may still get its answer (a cancelled
  prompt's response, for one). Nothing in stage 2 relies on either; the
  in-memory `maki` of stage 10 must not rely on a reply to a request
  pending at shutdown.
- **Byte streams.** `serve_stream(params, reader: impl AsyncBufRead,
  writer: impl AsyncWrite)` pumps newline-framed lines between a byte
  stream and `serve_channels`. The reader pump keeps `read_line` in its own
  task, as `read_stdin` does today, because `read_line` is not cancel
  safe. The writer pump appends the newline and flushes after each message.
  A write error ends the server rather than being ignored: the writer pump
  stops the input, the core closes the session and returns, the reader task
  is cancelled, and `serve_stream` returns that I/O error, so a peer that
  stopped reading never leaves the server waiting on an open input.
  `serve(params)` is `serve_stream` over stdin and stdout, so `maki acp` is
  unchanged.
- **In memory.** `memory_link() -> (Link, ServerLink)` makes two channel
  pairs: the client's `Link` (from `Link::new`), and a `ServerLink {
  input, output }` for `serve_channels`. No bytes and no framing cross
  them, but each message is still serialized JSON text, so the server
  validates exactly what it validates over stdio. The design requires this
  for an in-memory adapter. `AcpClient::shutdown` sends `END_OF_INPUT` on
  it, which ends the server's input, as stdin closing does.
- **Placement.** These live in `maki-acp` for now. Stage 3 moves the
  client half of `memory_link` with the client into `maki-wire`, and
  `ServerLink` stays with the server.

## Tests

One set of protocol cases runs over two links: the in-memory pair, and a
byte stream through `serve_stream` (a Unix socket pair from `smol::net`,
under `cfg(unix)`). Each case runs on isolated storage with explicit
model and trust fixtures, and checks that both links give the same
answers:

- `initialize`, through a real `AcpClient`, answers the agent
  capabilities and the agreed extensions;
- `session/new` opens a session;
- `session/list` lists a session saved beforehand in the isolated storage
  (opening one writes nothing until it has a transcript);
- raw cases, which write a line to the link directly and read the raw
  replies, since `AcpClient` drops a reply with no pending request: a
  method the server does not know answers `method_not_found` with the
  request's id, and a malformed line answers a parse error with a null id,
  after which a following request is still answered;
- `AcpClient::shutdown` ends the server: `serve_*` returns `Ok`, and the
  client sees its link close.

One case is for the byte stream only: a writer that fails while the input
stays open (the peer half of the socket pair kept open and silent) makes
`serve_stream` return the write error, with the session closed.

The server's existing unit tests, which drive `Server` directly, are
unchanged.

## Checks

Full `just ci`. A live run of `maki connect -- maki acp` behaves as
before.
