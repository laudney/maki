# Lua plugins and UI presentation

Source snapshot and reading rules: [survey index](README.md).
Canonical rules: [two Lua runtimes](../tui-over-acp-design.md#two-lua-runtimes)
and [data and presentation](../tui-over-acp-design.md#data-and-presentation).

## Current framework

[`PluginHost`](../../maki-lua/src/loader.rs) loads bundled and external
plugins into one VM. [`runtime::spawn`](../../maki-lua/src/runtime.rs) creates
the `maki-lua` thread. The TUI and agent use that same host for tools,
prompts, commands, hooks, keymaps, windows and background plugin work.

The runtime installs a UI sender only for `Interaction::Tty`. ACP startup
uses `Interaction::None`, so UI operations lack a receiver. This is a
current construction choice. It does not require a remote UI sender in the
new design.

[`maki-lua/Cargo.toml`](../../maki-lua/Cargo.toml) depends on `maki-agent`,
providers and terminal support. Reusing the whole current crate as the UI
runtime would preserve the dependency that the design removes. Shared Lua
machinery needs a domain-neutral boundary and separate host API registration.

## Current coupling surfaces

| Surface | Source | Target |
| --- | --- | --- |
| `PluginHost`, bundled discovery, package loading | [`loader.rs`](../../maki-lua/src/loader.rs) | Separate domain entry points and grants |
| `SlotHook`, agent/tool interception | [`hook.rs`](../../maki-lua/src/hook.rs) | Agent runtime |
| `HostEvent`, `dispatch` | [`agent_autocmd.rs`](../../maki-lua/src/agent_autocmd.rs) | Split authoritative agent lifecycle from local UI hooks |
| `ui.plan_form`, plugin-declared slots | [`api/slot.rs`](../../maki-lua/src/api/slot.rs) | A slot belongs to one runtime |
| Commands, keymaps, hints | [`api/util/command.rs`](../../maki-lua/src/api/util/command.rs), [`api/keymap.rs`](../../maki-lua/src/api/keymap.rs) | UI runtime for local commands and callbacks |
| Buffers, floats and window events | [`api/ui/`](../../maki-lua/src/api/ui/), [`lua_float.rs`](../../maki-ui/src/components/lua_float.rs) | Local UI mechanisms |
| Model, session, task and plan APIs | `*Request` enums in `api/util/command.rs` | UI facade over ACP operations, with explicit targets |
| Tool headers, bodies and restore callbacks | [`runtime.rs`](../../maki-lua/src/runtime.rs), [`chat.rs`](../../maki-ui/src/chat.rs) | Local UI renderers over semantic agent data |

Existing `UiAction` variants mix local actions and agent operations. They
must be separated by owner rather than serialized as one remote UI API.
`EventHandle`, `KeybindTicket`, channels and Lua registry keys stay local.

## Plugins that require a split

The design contains the [complete placement table](../tui-over-acp-design.md#plugin-placement).
These examples show why the existing API cannot simply load unchanged twice.

### Tool handlers and renderers

[`list`](../../plugins/list/init.lua) registers a handler, a buffer-producing
header and a restore function together. [`bash`](../../plugins/bash/init.lua)
appends stdout and stderr directly to a tool-view buffer.
[`code_execution`](../../plugins/code_execution/init.lua) and
[`batch`](../../plugins/batch/init.lua) also combine execution with views.

Execution and output hooks stay in agent Lua. Display code moves to UI Lua.
The new result path carries text and structured data, including child-call
results where needed. Streaming output must no longer depend on changes to
`SharedBuf` reaching a client.

### Questions and tasks

[`question`](../../plugins/question/init.lua) currently opens
[`question_form`](../../plugins/question/question_form.lua) from its handler
and returns model output plus a rendered card. Its agent component needs a
semantic interaction request. Its UI component owns the form and card.

[`task`](../../plugins/task/init.lua) loads its picker alongside execution.
[`runner.lua`](../../plugins/task/runner.lua) and
[`background.lua`](../../plugins/task/background.lua) contain agent policy.
[`picker.lua`](../../plugins/task/picker.lua) and picker row presentation
belong to the UI. Stopping a task is an ACP operation. Focus is local.

### Memory and todos

[`memory`](../../plugins/memory/init.lua) combines a tool, prompt hints,
write rules, a `/memory` picker and editor actions. Memory files and policy
stay with the agent. The UI picker requests data and operations for the
agent's memory store. A remote path cannot be opened as a local file by
assumption.

[`todo_write`](../../plugins/todo_write/init.lua) combines a tool with a
panel, a keymap and shared `todos`, `hidden` and `focused` tables. The agent
publishes todo data scoped to session/task and defines its active lifecycle.
The UI owns visibility and focus. Preserve the existing distinction between
an active panel and a historical tool card: replay must not reopen an old
finished todo panel.

### Completion and pickers

[`completion`](../../plugins/completion/ui.lua) watches local input and
uses [`completion.sources`](../../plugins/completion/sources.lua) callbacks
from other plugins. That slot and all input/window handling stay in UI Lua.
Remote workspace sources request data and discard stale responses. A UI
source cannot hold a callback from the agent runtime.

[`thinking`](../../plugins/thinking/ui.lua) and
[`sessions`](../../plugins/sessions/ui.lua) are UI components. They call
agent operations through the ACP facade. Model capabilities and stored
sessions remain agent-owned. The agent does not need the UI's focused tab.

## Hook split

Current `HostEvent` mixes `ToolStart`, `TurnEnd`, `InputChanged`, focus,
status and session events. Agent hooks run at the authoritative source.
UI observers receive published events and local hooks. An observed turn end
can update a panel without executing agent policy again. Replay and live
observations must be distinguishable.

Permission interception belongs to the agent runtime. Permission prompt
presentation belongs to the UI runtime. Both retain their domain's grants.
There is no cross-runtime slot chain and no automatic transfer of authority.

## Presentation checks

Theme changes, window resizing, key dispatch and tool expansion work with no
agent Lua callback. Removing a UI renderer leaves readable output. A crash
in a renderer cannot repeat a tool. Agent hooks and tools work when no UI
runtime has been created.
