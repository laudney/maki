# Agent and UI split: stage 3, the client crate

Status: proposed. Stage 3 of the [implementation
design](agent-ui-split-implementation.md): a new crate, `maki-wire`, holds
the ACP client, the Maki extensions' wire types, the display mapping and the
client links. `maki-acp` keeps the server and the agent host, and depends on
`maki-wire` for the wire types both sides share. `maki-ui` depends on
`maki-wire` instead of `maki-acp`. Behavior does not change, and no wire
format changes.

## Today

`maki-acp` holds both halves. By module:

| Module | Half | Agent edge |
| --- | --- | --- |
| `client`, `websocket` | client | `maki_agent::btw::SideAnswer` (client) |
| `display` | client | only items stage 1 moved to `maki-types`, plus `translate::{NOTICE_ID_PREFIX, unfenced}` |
| `btw`, `commands`, `elicitation`, `extensions`, `login`, `methods`, `plan`, `steer`, `usage` | shared wire types | `SideAnswer` (btw); the rest only `maki-types` and `maki-storage` items |
| `render` | shared wire types, plus the host's `LiveBodies`, `Next`, `TickSource`, `timer_ticks` and `LIVE_BODY_POLL` | only `maki-types` items |
| `settings` | shared wire types, plus the host's `options` | `options` reads `Model` and `SessionSettings` |
| `translate`, `permissions`, `server`, `lib` (`AcpParams`, `run`) | host | `ToolRegistry`, `PermissionAnswer`, the agent loop |

`maki-ui` reads only client and shared modules. Its tests, and the
display tests, build their fixtures with the host's `translate`, so each
case checks that what the host writes, the display reads back.

## Design

- **The crate.** `maki-wire` takes the client, shared and display modules
  above, with their module names and public items unchanged, and their
  tests. Its normal dependencies are `maki-types`, `maki-storage`,
  `agent-client-protocol-schema`, `async-tungstenite`, `futures`, `url`,
  `smol`, `flume`, `serde`, `serde_json`, `tracing` and `thiserror`, with
  `test-case` and `tempfile` as dev-dependencies. A moving module that
  names a stage 1 item through `maki_agent` or `maki_providers`, in a
  `use`, an inline path or a test (`AgentMode` in `methods`, `Message` in a
  `usage` test, the span types in the `display` and `render` tests,
  `ThinkingConfig` in `settings`, `add_cost` in `usage`), names it in
  `maki_types` or `maki_storage` instead. `maki-acp` re-exports
  none of it: every user imports from `maki_wire`. `maki-acp` drops each
  dependency only the moved modules used, which `cargo machete` checks.
- **`SideAnswer`.** It is a two-variant data enum, so it moves to
  `maki-types` with a re-export at `maki_agent::btw::SideAnswer`, as in
  stage 1.
- **Fences and notices.** `fenced`, `unfenced`, `longest_backtick_run`,
  `MIN_FENCE_LEN` and `NOTICE_ID_PREFIX` are a convention the host writes
  and the display reads, so they move to a new `maki_wire::format` module.
  `fenced`, `unfenced` and `NOTICE_ID_PREFIX` become `pub`. Their tests
  move with them.
- **The shutdown sentinel.** `END_OF_INPUT` is part of the link
  contract, and the host's `forward_input` and `memory_link` users read
  it, so it becomes `pub` in `maki_wire::client`. `Pumps`, `logged` and
  `Link::with_pumps` stay crate-private, since only `websocket` uses them.
- **Host-only parts of shared modules.** They stay in `maki-acp`:
  `LiveBodies`, `Next`, `TickSource`, `timer_ticks` and `LIVE_BODY_POLL`
  move from `render` to a new `maki_acp::live` module, and `options` with
  its helper moves from `settings` to a new `maki_acp::config_options`
  module. What is left of `render` and `settings` is wire only, and moves
  to `maki-wire` whole.
- **Round-trip tests.** A display test that builds its input with
  `translate` checks both halves, so it moves to `maki-acp`, which has
  both: a `display` module inside `translate.rs`'s tests, with the
  fixtures it needs. Such a test reaches `DisplayMapper` only through its
  public interface: the test-only `with_render` becomes a helper in the
  moved module over `DisplayMapper::new`, and the `done_reason` round trip
  goes through `DisplayMapper::done`. A display test that builds ACP
  updates directly stays in `maki-wire`. `maki-ui` keeps `maki-acp` as a dev-dependency for its
  own round-trip tests, and drops it as a normal dependency. Dev-
  dependencies do not reach a binary, and both crates then share one
  `maki-wire`, so the types match.
- **`memory_link`.** It stays in `maki-acp`, because it builds the
  `ServerLink` that `serve_channels` reads. The client half, `Link`, moves
  with the client. This refines the stage 2 text.
- **The gate.** `just deps-check` checks `maki-wire` as it checks
  `maki-types`: `cargo tree -p maki-wire -e normal` must list neither
  `maki-agent` nor `maki-providers`.

## Commits

Each commit passes `just ci`.

1. `SideAnswer` moves to `maki-types`.
2. Inside `maki-acp`, the host-only parts leave the shared modules: the
   `format` module, the `live` module, the `config_options` module, the
   round-trip tests moved into `translate.rs`,
   `END_OF_INPUT` made `pub`, and every stage 1 item that a moving module
   names through `maki_agent` or `maki_providers` renamed to its
   `maki_types` or `maki_storage` path, with `maki-types` as a direct
   dependency of `maki-acp`. After this commit every module that moves
   next has no `maki_agent`, `maki_providers` or host import.
3. `maki-wire` is created with `git mv` of the client, shared and display
   modules (with the wire halves of `render` and `settings`), imports are
   rewritten in `maki-acp`, `maki-ui` and the root crate, `maki-ui`
   switches its dependency, and `deps-check` covers `maki-wire`.

## Tests

No test is added or removed. Every moved test passes in its new crate with
only its import paths changed, apart from the round-trip tests, which also
change to `DisplayMapper`'s public interface as above. The serde tests of the wire types prove
the formats unchanged.

## Checks

Full `just ci` after each commit. `cargo tree -p maki-wire -e normal` shows
no edge to `maki-agent` or `maki-providers`, and `cargo tree -p maki-ui -e
normal` shows no edge to `maki-acp`. A live run of `maki connect -- maki
acp` behaves as before.
