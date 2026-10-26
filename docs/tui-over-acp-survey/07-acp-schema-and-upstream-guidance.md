# Protocol and project constraints

Source snapshot and reading rules: [survey index](README.md).
Target contract: [ACP and transport](../tui-over-acp-design.md#acp-and-transport).

## Pinned protocol dependency

[`Cargo.toml`](../../Cargo.toml) declares
`agent-client-protocol-schema = "1.10"` without optional schema features.
[`Cargo.lock`](../../Cargo.lock) resolves it to `1.10.2`. The repository
uses the schema crate and its own adapter. This records the checkout's
choice, not the latest available ACP release.

The current adapter implements stdio. Its question path uses form
elicitation, and its permission path uses request/response option IDs.
[Document 04](04-acp-server.md) identifies those implementations.

This rewrite does not choose a dependency upgrade, a connection library or
exact extension method names. Check the selected schema and its primary
specification before designing those details. A carrier that exists in a
schema is not necessarily implemented by the Maki server or by a client.

## Protocol design constraints

| Concern | Requirement |
| --- | --- |
| Agent/client boundary | Operations and serializable data, including over in-memory links |
| Standard ACP | Reuse it where it preserves behavior and permissions |
| Extensions | Negotiate concrete agent capabilities and data versions |
| Optional UI plugins | Default content remains readable without a renderer |
| Lua domains | No remote callbacks, Lua evaluation, slots or shared state |
| Client independence | Another ACP client can use the supported standard subset |
| Transport independence | Equivalent request, stream, cancellation and error semantics |
| Remote access | Authentication and workspace authorization are separate requirements |
| Replay | Durable semantic data and explicit live-versus-replay behavior |

The former design used a private same-build protocol to remote all Lua UI
operations. That is no longer the target. A UI should be able to use an
agent without sharing its executable build or Lua VM. Compatibility is
expressed by supported operations and data versions.

## Core invariants

The repository's [instructions](../../AGENTS.md) define two relevant public
contracts:

- Provider requests remain append-only within a frame. Prompt or tool
  changes use a context update or an announced new frame. Plugin splitting
  and reload cannot bypass that invariant.
- `Message`, `ContentBlock`, `FactsUpdate` and `PromptFacts` serde is public
  API for sessions, exports and Lua. Fields and variants follow the existing
  compatibility rules, including required log-version changes.

New semantic result data must preserve readable fallback output and old
session data. UI presentation caches do not become required session content.
Any new persisted state needs an explicit schema and compatibility review.

## Plugin architecture guidance

The [maintainer guide](../upstream-maintainer-guide.md) supports a small
core, plugin-owned policy and native ownership mechanisms where required.
The [fork guide](../fork-maintenance.md) records current cancellation,
background work and request-frame contracts. Current repository instructions
and the user's accepted architecture take precedence over historical
inferences in those guides.

The current [Lua API instructions](../../maki-lua/src/api/AGENTS.md) describe
one shared Lua thread and Neovim-like APIs. The shared-thread statement is
current implementation evidence. The target has separate agent and UI
states. Keep familiar APIs where the owning domain supports them, and define
migration for mixed plugins. Do not retain an invalid boundary solely to
load an existing plugin unchanged.

The task plugin retains delegation policy. Runtime machinery owns lifetime,
cancellation and capability enforcement. Neither a picker nor an agent
result renderer becomes an alternate policy authority.

## Documentation and validation

The [documentation style](../../site/docs/AGENTS.md) requires concise prose
and a canonical home per topic. The design states decisions. The survey and
source index provide evidence and navigation.

This is documentation-only work. Applicable checks are link/anchor
validation, source-reference checks, Markdown consistency and whitespace.
Code checks, generated documentation builds and code-focused commit hooks
are not required for this rewrite.

Future implementation validation follows affected behavior and repository
rules. The design's acceptance criteria identify boundary cases. They do not
prescribe review counts, speculative performance claims or repeated full
workspace checks at every intermediate commit.
