-- Shared view of a `maki.dir_listing` listing, so every caller shows a
-- directory the same way.

local ToolView = require("maki.tool_view")

local DEFAULT_MAX_LINES = 10

local M = {}

local function view_opts(tool_output_lines)
  return { max_lines = (tool_output_lines and tool_output_lines.read) or DEFAULT_MAX_LINES, keep = "head" }
end

-- The view for a handler or a `restore`, which a click toggles.
function M.view(text, ctx)
  return ToolView.restore(text, view_opts(ctx:tool_output_lines()))
end

-- The same view for a renderer and its `ctx`, collapsed or {expanded}.
function M.draw(text, ctx, expanded)
  return ToolView.draw(text, view_opts(ctx.tool_output_lines), expanded)
end

return M
