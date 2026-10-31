-- Shared renderer body and click of a web tool's call, so every web tool
-- shows its text the same way. The tool's reply keeps its whole text in
-- `state.text` when the model got it cut.

local ToolView = require("maki.tool_view")

local DEFAULT_MAX_LINES = 3

local M = {}

local function view_opts(ctx)
  return { max_lines = ctx.tool_output_lines.web or DEFAULT_MAX_LINES, keep = "head" }
end

-- The whole text the state keeps when the model got it cut, else the
-- model's text: all of it, a cut too big to keep, or a call from before the
-- state, as the old restore drew. A failed call draws its error as the
-- default does, as it did live.
local function shown_text(data)
  if data.status ~= "done" then
    return nil
  end
  return data.state and data.state.text or data.output.Plain.text
end

-- A renderer's `body`: the text in the web view, collapsed or expanded.
function M.body(data, ctx)
  local text = shown_text(data)
  return text and ToolView.draw(text, view_opts(ctx), data.expanded)
end

-- A renderer's `click`: toggles the view between collapsed and expanded.
function M.click(data)
  data.expanded = not data.expanded
end

return M
