local State = require("todo_state")

local sessions = State.sessions
local state_of, shown_items = State.state_of, State.shown_items

local function count_done(items)
  local n = 0
  for _, item in ipairs(items) do
    if item.status == "completed" then
      n = n + 1
    end
  end
  return n
end

-- A plain buf made during a tool call becomes that call's chat body, and it
-- would mirror every later list from then on. A scratch buf stays ours.
local function open_panel(sid)
  local buf = maki.ui.buf()
  local win = maki.ui.open_win(buf, {
    split = "panel",
    height = 4,
    order = 10,
    title = " Todos ",
    border = "rounded",
    focus = false,
    visible = false,
    session = sid,
    footer = {
      { "Ctrl+T", "to hide" },
    },
  })
  return buf, win
end

local function collapsed_hint(items)
  return {
    { string.format(" %d/%d ", count_done(items), #items), "foreground" },
    { "Ctrl+T", "keybind_key" },
    { " ", "" },
  }
end

-- The one place that draws a session. The panel shows the list, and while
-- Ctrl+T hides it the hint shows the count instead. Both live in the
-- session's tab, so a background session keeps them up to date and they are
-- right when you switch back. A hidden window still counts as open, so an
-- "open it unless it is" shortcut would keep the panel hidden for good.
local function sync(sid)
  local s = state_of(sid)
  local items = shown_items(s)
  local opts = { session = sid }
  if #items == 0 then
    if s.win then
      s.win:hide()
    end
    maki.ui.set_status_hint(nil, opts)
    return
  end
  if not (s.win and s.win:is_open()) then
    s.buf, s.win = open_panel(sid)
  end
  s.buf:set_lines(State.build_lines(items))
  s.win:set_config({ height = #items + 2 })
  if s.hidden then
    s.win:hide()
    maki.ui.set_status_hint(collapsed_hint(items), opts)
  else
    s.win:show()
    maki.ui.set_status_hint(nil, opts)
  end
end

State.on_write = function(sid, task, items)
  local s = state_of(sid)
  s.lists[task] = items
  if task == s.task then
    sync(sid)
  end
end

local function toggle()
  local s = sessions[State.focused]
  if not s or #shown_items(s) == 0 then
    return
  end
  s.hidden = not s.hidden
  sync(State.focused)
end

maki.keymap.set({ "n", "i" }, "<C-t>", toggle, { desc = "Toggle todo panel" })

-- Subagents run inside the parent's turn, so its end clears their lists too.
maki.api.create_autocmd("TurnEnd", {
  callback = function(ev)
    local sid = ev.data.session_id
    local s = sessions[sid]
    if s then
      s.lists, s.hidden = {}, nil
      sync(sid)
    end
  end,
})

-- The host already closes the panel and drops the hint when a session leaves
-- its tab, so forgetting the state is all that is left to do.
maki.api.create_autocmd("SessionEnd", {
  callback = function(ev)
    sessions[ev.data.session_id] = nil
  end,
})

-- Also fires on a tab switch, which is how Ctrl+T learns the tab on screen.
-- Each tab has its own panel, so all we track per tab is whose chat it shows.
maki.api.create_autocmd("TaskFocusChanged", {
  callback = function(ev)
    State.focused = ev.data.session_id
    state_of(State.focused).task = ev.data.id
    sync(State.focused)
  end,
})

maki.ui.register_renderer("todo_write", {
  body = function(data)
    if data.status ~= "done" then
      return nil
    end
    local buf = maki.ui.buf()
    buf:set_lines(State.build_lines(data.input.todos or {}))
    return buf
  end,
})
