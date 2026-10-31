local helpers = require("memory_helpers")
local ListPicker = require("maki.list_picker")
local Toast = require("maki.toast")

local VIEW_PREF_FILE = "picker_view"

-- A toast and not a flash, because this feedback has to stay readable while
-- the picker is still covering the screen. Kept local on purpose: claiming the
-- global `maki.notify` slot here would put a "memory" toast in front of every
-- other plugin's notices too.
local function notify(msg)
  Toast.show(msg, { title = "memory" })
end

local function flat_rows(files)
  local items = {}
  for i, f in ipairs(files) do
    items[i] = { label = f.name, detail = helpers.detail_parts(f.size, f.tags) }
  end
  return items
end

-- No tags on the row: the section header names the tag, and the tint shows
-- which other sections hold the same file.
local function grouped_rows(files)
  local items = {}
  for _, g in ipairs(helpers.group_by_tag(files)) do
    for _, f in ipairs(g.files) do
      items[#items + 1] = {
        label = f.name,
        detail = helpers.format_size(f.size),
        section = g.tag,
        section_detail = "(" .. #g.files .. ")",
      }
    end
  end
  return items
end

local VIEWS = {
  { id = "flat", rows = flat_rows },
  { id = "grouped", rows = grouped_rows },
}

-- Remembered across runs, so /memory opens the way you left it. A preference
-- and not project data, so it lives with maki's other state and not in the
-- memories directory.
local function view_pref_path()
  local state = maki.env.state_dir()
  return state and maki.fs.joinpath(state, "memory", VIEW_PREF_FILE)
end

local function load_view()
  local path = view_pref_path()
  local saved = path and maki.fs.read(path)
  for i, v in ipairs(VIEWS) do
    if v.id == saved then
      return i
    end
  end
  return 1
end

local function save_view(view)
  local path = view_pref_path()
  if not path then
    return
  end
  local _, err = maki.fs.mkdir(maki.fs.dirname(path), { parents = true })
  if not err then
    _, err = maki.fs.write(path, VIEWS[view].id)
  end
  if err then
    maki.log.warn("memory: cannot save the picker view to " .. path .. ": " .. tostring(err))
  end
end

maki.api.register_command({
  name = "/memory",
  description = "View, edit, and delete memory files",
  handler = function()
    local dir = helpers.resolve_dir(true)
    if not dir then
      maki.ui.flash("Cannot resolve memory directory")
      return
    end

    local view = load_view()
    -- The only place rows are built, so the two views can never disagree about
    -- what is on disk.
    local function build()
      local files, warnings = helpers.files_with_tags(dir)
      if #warnings > 0 then
        notify(#warnings .. " unreadable memory file(s)")
      end
      return VIEWS[view].rows(files)
    end

    local items = build()
    if #items == 0 then
      maki.ui.flash("No memories yet")
      return
    end
    local last_cursor = 1
    while true do
      local event = ListPicker.open(items, {
        title = " Memory Files ",
        cursor = last_cursor,
        key = function(item)
          return item.label
        end,
        submit_keys = { "<C-o>" },
        live_keys = {
          ["<Tab>"] = function()
            view = view % #VIEWS + 1
            save_view(view)
            return build()
          end,
        },
        footer = {
          { "Enter", "open" },
          { "Ctrl+O", "edit" },
          { "Ctrl+D", "delete" },
          { "Tab", "switch view" },
        },
      })

      if event.type == "close" then
        break
      end

      last_cursor = event.index
      if event.type == "choice" then
        local path = maki.fs.joinpath(dir, event.item.label)
        if maki.ui.open_editor(path) == 0 then
          items = build()
        end
      elseif event.type == "delete" then
        local ok, err = maki.fs.rm(maki.fs.joinpath(dir, event.item.label))
        if ok then
          notify("Deleted " .. event.item.label)
          items = build()
          if #items == 0 then
            break
          end
        else
          notify("Delete failed: " .. tostring(err))
        end
      else
        break
      end
    end
  end,
})
