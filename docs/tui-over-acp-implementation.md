# TUI over ACP: source index

This file replaces the former implementation specification. The path is kept
for existing links. The [design](tui-over-acp-design.md) defines the target
architecture. There is no prescribed commit sequence or implementation plan.

The references below describe source at `c7554a36` on 2026-10-26. Find code by
symbol after a refactor. A location here does not mean its present ownership
is part of the target design.

## Agent loop and host responsibilities

| Topic | Source and symbols |
| --- | --- |
| Core loop | [`agent/run.rs`](../maki-agent/src/agent/run.rs), `Agent::run` |
| Frame and append-only requests | [`agent/frame.rs`](../maki-agent/src/agent/frame.rs), `context_update`, and `assert_append_only` in `agent/run.rs` |
| Core agent hooks | [`agent/hook.rs`](../maki-agent/src/agent/hook.rs), `AgentHook`, `AgentSlot` |
| Tool hooks and authority | [`tools/hook.rs`](../maki-agent/src/tools/hook.rs), `ToolHook`, `HookStage`, `Authority` |
| Tool registration | [`tools/registry.rs`](../maki-agent/src/tools/registry.rs), `ToolRegistry` |
| TUI driver | [`agent/agent_loop.rs`](../maki-ui/src/agent/agent_loop.rs), `AgentLoop` |
| Headless drivers | [`headless.rs`](../maki-agent/src/headless.rs), `spawn`, `spawn_interactive` |
| Generation and background work | [`live_session.rs`](../maki-agent/src/live_session.rs), `AgentSeed`, `SessionMailbox` |
| Interaction ownership | [`asks.rs`](../maki-agent/src/asks.rs), `OwnedAsks`, `PermissionRoute` |
| Persistence | [`session.rs`](../maki-agent/src/session.rs), `SessionTrack`, and [`storage_writer.rs`](../maki-ui/src/storage_writer.rs), `StorageWriter` |

## Lua domains

| Topic | Source and symbols |
| --- | --- |
| Bundled discovery and loading | [`loader.rs`](../maki-lua/src/loader.rs), `BUNDLED_PLUGINS`, `PluginHost` |
| Present single VM | [`runtime.rs`](../maki-lua/src/runtime.rs), `spawn`, `LuaRuntime` |
| Core hook adapter | [`hook.rs`](../maki-lua/src/hook.rs), `SlotHook` |
| Mixed event catalogue | [`agent_autocmd.rs`](../maki-lua/src/agent_autocmd.rs), `HostEvent`, `dispatch` |
| Slots and UI hooks | [`api/slot.rs`](../maki-lua/src/api/slot.rs), `SlotStore`, `PLAN_FORM_SLOT` |
| UI actions and round trips | [`api/util/command.rs`](../maki-lua/src/api/util/command.rs), `UiAction`, `ui_roundtrip` |
| Keymaps | [`api/keymap.rs`](../maki-lua/src/api/keymap.rs), `KeymapReader`, `KeybindTicket` |
| Buffers and windows | [`api/ui/`](../maki-lua/src/api/ui/) and [`lua_float.rs`](../maki-ui/src/components/lua_float.rs) |
| Permissions | [`plugin_permissions.rs`](../maki-lua/src/plugin_permissions.rs), `PluginPermissions`, `PluginReach` |

The [Lua survey](tui-over-acp-survey/03-tui-components-and-lua.md) maps
existing packages to the two domains, including mixed tools and UI features.

## ACP and frontends

| Topic | Source and symbols |
| --- | --- |
| Current adapter | [`server.rs`](../maki-acp/src/server.rs), `serve`, `Server`, `start_session`, `start_event_pump` |
| Tool data and replay | [`translate.rs`](../maki-acp/src/translate.rs), `tool_start`, `tool_done`, `replay_history` |
| Question interaction | [`elicitation.rs`](../maki-acp/src/elicitation.rs), `form_request`, and `question_tool` in `server.rs` |
| Permission options | [`permissions.rs`](../maki-acp/src/permissions.rs) |
| UI event processing | [`event_loop.rs`](../maki-ui/src/event_loop.rs), `EventLoopParams`, `handle_ui_action` |
| Tool presentation | [`tool_display.rs`](../maki-ui/src/components/tool_display.rs), `build_tool_lines`, and [`messages/mod.rs`](../maki-ui/src/components/messages/mod.rs) |
| History presentation | [`chat.rs`](../maki-ui/src/chat.rs), `history_to_display` |
| UI launch | [`cmd/tui.rs`](../src/cmd/tui.rs), `build_stack`, `run` |
| ACP launch | [`cmd/acp.rs`](../src/cmd/acp.rs), `run` |
| Shared plugin setup | [`cmd/mod.rs`](../src/cmd/mod.rs), `load_plugins`, `cli_stack` |
| Other frontends | [`print.rs`](../src/print.rs) and [`sdk_mode.rs`](../src/sdk_mode.rs) |

## How to use the references

Trace the behavior in its current owner, then apply the design's domain
boundary. For a mixed plugin, inspect execution, presentation, shared state
and hooks together. Keep useful existing behavior tests, but change tests
that assume the rejected shared runtime or remote windows.

Select source checks when implementation begins. Do not treat old phase
names, planned signatures, schema upgrades or review counts as requirements.
