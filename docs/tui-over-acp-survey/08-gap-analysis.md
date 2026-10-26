# Feature inventory and design gaps

Source snapshot and reading rules: [survey index](README.md).
The [design](../tui-over-acp-design.md) is the canonical architecture.
This inventory maps existing features to owners without defining a wire
schema or implementation sequence.

## Main change from the former proposal

The existing source mixes tool execution, authoritative state and UI
presentation. The target assigns each responsibility to its owner and uses
ACP for agent operations and data. It does not require every current
shared-memory object or Lua UI action to acquire a wire representation.

In particular, windows, keymaps, input editing, focus, themes and Lua display
buffers stay local to the UI. Agent hooks, tool handlers and semantic state
stay with the agent. Mixed plugins need separate components.

## Conversation, runs and tasks

| Feature | Target owner and boundary | Current evidence or gap |
| --- | --- | --- |
| Text, thinking and prompt images | ACP data, rendered locally | Existing adapter covers basic conversation content |
| Assistant images and grouped history | Agent publishes content/group identity, UI draws it | TUI reads images from `TurnComplete.message`, adapter loses that path |
| Hidden and synthetic messages | Agent publishes only permitted display data | Replay must preserve current visibility rules |
| Queue, burst merge and interrupts | Host scheduling, ACP queue operations and updates | Current queue types live in `maki-ui` |
| Accepted-item bubble | Host acceptance identity, UI pending/display state | Queue identity precedes history after hooks |
| Run start/end, exit-on-done | Host lifecycle events, local exit policy | Engine-born runs need events without a prompt request |
| Foreground cancel | ACP operation, host validates owner | Do not use local focus or stale display IDs as authority |
| Background promotion | Host operation and task/result events | Preserve one result delivery and background lifetime |
| Subagent chats and saved history | Host child identity and content, local chat views | Adapter drops child display streams |
| Plugin-owned tasks | Agent Lua and host ownership | Current events can travel through `UiAction::PluginTaskEvent` |
| Child cancel, messages and remove | ACP operations against a named child | UI currently holds native cancel/inbox handles |
| `/tasks` and focus | Local UI plugin over published task state | Current `task` package mixes execution and picker |
| Notices, retries and prompt progress | Agent data with client display | Adapter omits several current event variants |
| Compaction and context usage | Host operation/data, UI display | Current TUI driver has behavior absent from ACP |
| Cost and per-model usage | Host accounting, published counters | Preserve child costs and usage-label placement |

## Tools, rendering and interaction

| Feature | Target owner and boundary | Current evidence or gap |
| --- | --- | --- |
| Tool schemas and execution | Agent Lua and tool interfaces | Current registration also includes presentation callbacks |
| Tool input/output hooks | Agent runtime | Preserve ordering before permissions and model history |
| Headers, annotations and result formatting | UI renderer over semantic input/output/status | UI currently parses native tool invocations during history load |
| Live bash and execution output | Agent streams data, UI builds buffers | Current live output can exist only in `SharedBuf` |
| Diffs, code, grep, instructions and images | Semantic result contract with readable fallback | Current adapter retains diffs but flattens much other structure |
| Batch child display | Agent publishes child results, UI renderer composes them | Do not execute child tools to rebuild a display |
| Tool clicks and expansion | UI plugin, with ACP only for requested agent actions | No remote Lua click callback |
| Theme and rebake | UI runtime | No theme transfer or agent-side buffer rebuild |
| Result restore | Local renderer from persisted semantic data | No remote `RestoreItem` or warm buffer dependency |
| Question form | Agent interaction request, UI form plugin | Replace handler-to-window call, reuse elicitation where suitable |
| Permissions and guidance | Agent enforcement, scoped ACP asks, local prompt | Preserve full answer choices and stale-reply rejection |
| Authentication retry | Agent/provider operation, local interaction | Credentials belong to the provider environment |
| Plan approval and actions | Agent plan/policy, local forms and commands | Current Lua slots mix policy and presentation |
| Todo panel | Agent todo data/lifecycle, local panel and keymap | Split shared Lua tables and distinguish replay from active state |
| Memory browsing and editing | Agent store operations, local picker/editor | `/memory` must work without direct access to an agent path |
| Missing UI renderer | Default local renderer over standard/fallback data | A UI package is optional for tool execution |

## Sessions and services

| Feature | Target owner and boundary | Current evidence or gap |
| --- | --- | --- |
| New/load/list/rename/delete/close | Host operations, UI tab decisions | Current server has one active session |
| Fork, rewind and reset | Host history/generation operations | Current UI mutates stored sessions and respawns agents |
| Authoritative persistence | Agent host, sole session writer | Current TUI saves more state than headless `SessionTrack` |
| Export | Host operation | UI currently reads shared live history |
| Input draft and local history | Client storage | Migrate old session metadata without losing drafts |
| Accepted queued messages | Host storage | Keep distinct from unsent drafts |
| Model, thinking, fast and workflow | Host settings/capabilities, UI controls | Current UI builds providers and maintains shared model cells |
| Model discovery and tiers | Host service, local picker preferences | Publish capability/pricing data needed by UI |
| `/btw` | Agent operation over agent history, local presentation | Current implementation calls a provider from `maki-ui` |
| MCP status and management | Host service, local picker | Current UI reads native MCP snapshots |
| Agent trust, yolo and saved rules | Agent enforcement and explicit operations | No shared `PermissionManager` in the client |
| Package administration | Each domain owns packages and approvals | UI package install cannot imply remote agent code approval |
| `/reload` | Explicit UI or agent runtime lifecycle | Separate client reload from changes to active agent frames |
| Plans and remote files | Agent data/services | Current UI reads plan files directly |
| Workspace cwd | Explicit agent request context | Current process globals cannot define remote UI semantics |

## Local UI features

| Feature | Target behavior | Data dependency |
| --- | --- | --- |
| `completion` | Local trigger, slots, ranking, menu and input edit | Remote sources request workspace data as needed |
| `thinking` | Local command and picker | Agent model capabilities and confirmed settings |
| `sessions` | Local commands, keymaps and picker | Agent session service |
| UI custom commands | Local handlers | ACP operations when they affect agent state |
| Agent/MCP prompt commands | Advertised agent operations | Prompt expansion in the owning agent environment |
| Keymaps and status hints | Local UI runtime | Optional observations of agent state |
| Floats, panels and forms | Local UI runtime | Semantic data only |
| Input reads, edits and version checks | Local UI runtime | No remote input-buffer API |
| Session/task focus | Local UI state | Explicit target IDs on agent operations |
| File picker | Local interaction | Data source identifies local or remote workspace |
| Image paste and clipboard | Local UI | Explicit content transfer to agent |
| Editor and shell actions | Local when identified as client actions | Remote edits require an agent operation or content transfer |
| Desktop notifications | Local UI | Published run/session events |
| Theme, recents, collapsed lists and scroll | Client preferences | No agent execution dependency |
| Tool output line limits | Local renderer preference | Must not truncate the model's tool result |

## Risks the new boundary must address

### Mixed plugin state

Splitting files alone does not split ownership. `task`, `question`,
`todo_write`, `memory` and custom plugins can share mutable state between a
handler and UI callbacks. Define the required semantic data and operations
before moving their presentation. Keep slot chains within one runtime.

### Lifecycle and replay

Background tasks and pending asks can outlive a foreground run. Session
generations and call identities must distinguish them from obsolete work.
Replay cannot run tool handlers, reopen active-only panels or fire agent
policy hooks. The UI marks connection loss and reconciles state before
accepting that an uncertain operation completed.

### Remote environments

A path, credential or package revision on the agent is not a local UI
resource. Completion, memory, plans, images and editors need explicit data
access. Configuration and project trust remain separate. Avoid implicit
package installation from agent-provided renderer metadata.

### Compatibility and scope

Separating current mixed plugins changes their host API. Preserve readable
results and stored sessions, and provide a migration path. Keeping the old
single VM as a second execution path would retain the ownership problem.

The [design acceptance criteria](../tui-over-acp-design.md#acceptance-criteria)
cover required behavior. Exact extension schemas, package entry points,
crate names and remote adapter details remain implementation-design work.
