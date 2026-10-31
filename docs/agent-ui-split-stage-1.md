# Agent and UI split: stage 1, shared types

Status: proposed. Stage 1 of the [implementation
design](agent-ui-split-implementation.md): a new crate, `maki-types`,
holds the data a display path reads, below both `maki-agent` and
`maki-providers`. Behavior does not change. Every serde format stays as it
is.

## Dependencies

`maki-types` may depend on `maki-storage` and `maki-config`, plus `serde`,
`serde_json`, `flume`, `arc-swap`, `strum`, `thiserror`, `tracing`, `jiff`,
`similar`, `event-listener` and `futures-lite`. `maki-storage` has no workspace dependency,
and `maki-config` depends only on `maki-storage` and its macro crate, so
neither can form a cycle. This refines the implementation design, which
named only `maki-storage`: `ToolKey` and `SessionDefaults` live in
`maki-config`, and the client reads its UI part anyway.

A `just deps-check` recipe, run by `just ci`, fails when `cargo tree -p
maki-types -e normal` lists `maki-agent` or `maki-providers`.

## Moves

Each moved item keeps a `pub use` at its old path, so its users compile
unchanged. Users inside its old crate use the re-export too. Tests move
with their code. The commits below follow one another. Each set is closed
under its fields, and `just ci` passes after each commit.

| Commit | Moves | From |
| --- | --- | --- |
| 1. Snapshots | `SpanColor`, `DefaultColor`, `InlineStyle`, `SpanStyle`, `SnapshotSpan`, `SnapshotLine`, `BufferSnapshot`, `UiWaker`, `SharedBuf` | `maki-agent/src/types.rs` |
| 2. Messages | `ImageMediaType`, `ImageSource`, `Role`, `ContentBlock` (with `KNOWN_BLOCK_TYPES` and its manual serde), `MessageKind`, `Message` (with `RawMessage`, `RawKind`, `ContextUpdateTag`, its manual serde and `impl TitleSource for Message`), `IMAGE_PLACEHOLDER`, `EMPTY_RESPONSE_MARKER`, `estimate_message_tokens`, `estimate_prompt_tokens`, `is_deferred_tool`, `DisplayItem`, `display_order`, `observation_line`, `HistorySnapshot`, `SharedMessages` | `maki-providers/src/{types,tokens}.rs`, `maki-agent/src/{display_order,history}.rs` |
| 3. Tool data | `GrepFileEntry`, `GrepMatchGroup`, `GrepLine`, `TodoItem`, `TodoStatus`, `TodoPriority`, `ToolInput`, `InstructionBlock`, `TextOutput`, `ToolOutput`, the `diff` module, `relative_path` with its `CWD` and `HOME` statics, `compact_duration`, `NO_FILES_FOUND` | `maki-agent/src/{types,diff}.rs`, `maki-agent/src/tools/mod.rs` |
| 4. Usage | `TokenUsage`, `ModelPricing`, `FastPricing`, `ProviderUsage`, `UsageLimit` (with `deserialize_reset_at`, `rfc3339_millis` and their constants), `ModelUsageRow`, `format_tokens`, `ModelFamily`, `ThinkingSupport`, `FastSupport` | `maki-providers/src/{model,types}.rs` |
| 5. Thinking | `ThinkingConfig` (pure methods and its `StoredThinking` conversions), `RequestOptions`, `StopReason`, `FALLBACK_MAX_THINKING_BUDGET` | `maki-providers/src/types.rs` |
| 6. Events | `DoneReason`, `SteerKind`, `PermissionChoice`, `SessionEndReason`, `ToolStartEvent`, `ToolDoneEvent`, `CallRecord`, `TurnCompleteEvent`, `tool_results`, `AgentEvent`, `Envelope`, `SubagentInfo`, `EventSender`, `event_stream`, `EventStreamGuard`, `SessionEvents`, `ALLOWED_BY`, `AgentMode`, `InputSource`, `McpPromptRef`, `EarlierInput`, `AgentInput`, `SubagentInbox`, `CancelToken`, `CancelTrigger` | `maki-agent/src/{types,lib,inbox}.rs` |

Not moved in stage 1:
- `Model` and `ModelInfo`. `Model`'s capability methods (`supports_*`,
  `thinking_options`, `from_spec`, `is_free`, `provider_display_name`) are
  registry lookups. `ModelInfo` carries a provider-private stash. A
  remote display needs a capability snapshot from the agent, and the
  stage that gives `maki-ui` one moves them.
- `byte_highlights` (which pulls in `nucleo`), `generalized_scopes` (which
  reads the file system), and the logic types that the survey lists as
  staying.

## Blockers and their resolution

| Commit | Blocker | Resolution |
| --- | --- | --- |
| 2 | `ImageSource.verdict` is `pub(crate)` and holds the provider's private `Fix`, which `image.rs` reads and writes | `Fix` moves down as a public, `#[doc(hidden)]` `WireVerdict` with an accessor, `ImageSource::wire_verdict(&self) -> &OnceLock<WireVerdict>`. `image.rs` uses the accessor. `PartialEq` keeps ignoring the verdict. |
| 2 | `impl TitleSource for Message`, where the trait is in `maki-storage` | the impl moves with `Message`, with `Role::is_user` |
| 3 | `ToolOutput::as_display_text` calls agent `diff::unified_text` and `tools::relative_path`; `relative_path` is `pub(crate)` and `HOME` private | both are pure, so both move. `relative_path`, `CWD` and `HOME` become `pub` and `#[doc(hidden)]` in `maki-types`, since the agent's `resolve_path` and tools read them across the crate boundary; the agent imports them at the new path. |
| 4 | `TokenUsage::estimate` is `pub(crate)` and calls the private `ModelPricing::rates`, and `Model::list_cost` in providers calls it | both move. `estimate` becomes `pub` and `#[doc(hidden)]`, with a comment that it serves pricing. |
| 5 | eleven `ThinkingConfig` methods and `RequestOptions::clamped` take `&Model` and read provider registries; they call the private `budget` and match its private `Budgeted`; provider code reads the crate-private `FALLBACK_MAX_THINKING_BUDGET` | the eleven move to an extension trait in providers, `ModelThinking` (`clamped`, `shown`, `effort_str`, the `apply_*` family, `reserved_thinking`, `request_thinking`, `request_budget`) and `ModelRequestOptions` (`clamped`). Each caller adds a `use`. The pure methods (`is_enabled`, `parse`, `clamp_to`, `status_label`, `budget`) stay on the type. `budget`, `Budgeted` and `FALLBACK_MAX_THINKING_BUDGET` become `pub` and `#[doc(hidden)]`, the interface the extension traits and `model.rs` use. |
| 6 | `impl From<Option<StopReason>> for DoneReason` | it moves with both types, which is why `StopReason` moves in commit 5 |
| 6 | `AgentEvent::error(&AgentError)` reads `AgentError`, which depends on isahc | it becomes `impl From<&AgentError> for AgentEvent` in providers (`&AgentError` is local there), and its five callers call `AgentEvent::from(&error)` |
| 6 | `EventSender::send` and `send_envelope` return `AgentError` | they return a local `ChannelClosed` error. Providers add `impl From<ChannelClosed> for AgentError`, so the agent's `?` sites compile unchanged. A method that returns a `send` result directly (for example in `agent/run.rs`) converts it explicitly, with `.map_err(AgentError::from)`. |
| 6 | `AgentEvent::PermissionRequest.tool` is a `maki-config` `ToolKey` | allowed by the dependency rule |
| 6 | `SubagentInfo.inbox` and `answer_tx` | these are a queue and a channel, not agent logic, so `SubagentInbox` and `AgentInput` move with them. `impl InterruptSource for SubagentInbox` stays in the agent, where the trait is local. It drains the inbox through a new public `drain_unless_cancelled(&CancelToken)`, which checks the cancel under the inbox lock and drains in the same critical section, as `poll` does today, so a cancelled run never takes input pushed for the next one; `CancelToken` and `CancelTrigger` move with it. `AgentInput::from_defaults` moves too, since `SessionDefaults` is in `maki-config`. |

An ACP link never carries the inbox or the answer channel. In a remote
tab they stay `None`, and stage 8 replaces their use with host task
operations.

## Tests

- `just ci` after each commit, with `deps-check` from commit 1.
- The serde tests of every moved type move with it unchanged. That
  covers stored sessions, `maki session export` and the ACP wire shapes.
  No snapshot of a stored format changes.
- `drain_unless_cancelled` on a cancelled token leaves every queued input in
  the inbox, and on a live one takes them all.
- The new accessor, `ChannelClosed` and the extension traits gain no tests
  of their own. The callers' existing tests cover them, and a test for a
  re-export or a delegation would be a tautology.

## Checks

Full `just ci` after each commit. `cargo tree -p maki-types -e normal`
shows no edge to `maki-agent` or `maki-providers`.
