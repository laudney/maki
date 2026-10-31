-- Shared by the tool in init.lua and the panel in ui.lua. A combined load
-- runs both entries in one env, so they get this same table.
local State = {}

local MAIN_TASK = "main"

local STATUS_MARKERS = {
  completed = { "[✓]", "todo_completed" },
  in_progress = { "[•]", "todo_in_progress" },
  pending = { "[ ]", "todo_pending" },
  cancelled = { "[x]", "todo_cancelled" },
}

-- `sessions[session_id]`, one per session that wrote todos or took focus:
--   lists[task_id]  the latest list of each task. A subagent shares its
--                   session id with the parent, only the task id tells them apart
--   task            the task whose chat the session's tab shows
--   hidden          Ctrl+T hid the panel, until the turn ends
--   buf, win        the session's own panel, which lives in its tab
State.sessions = {}
-- The session on screen, the one Ctrl+T acts on. It stays nil until the
-- first TaskFocusChanged.
State.focused = nil
-- `on_write(session_id, task_id, items)`, called by the tool on each write.
-- ui.lua sets it; without the panel nothing keeps the lists.
State.on_write = nil

function State.state_of(sid)
  State.sessions[sid] = State.sessions[sid] or { lists = {}, task = MAIN_TASK }
  return State.sessions[sid]
end

function State.shown_items(s)
  return s.lists[s.task] or {}
end

function State.build_lines(items)
  local lines = {}
  for _, item in ipairs(items) do
    local marker = STATUS_MARKERS[item.status] or STATUS_MARKERS.pending
    lines[#lines + 1] = {
      { marker[1] .. " " .. item.content, marker[2] },
    }
  end
  return lines
end

return State
