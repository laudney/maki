local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")

local MAX_SCRIPT_LINES = 2000

local M = {}

function M.draw(data, ctx, language)
  local state = data.state or {}
  local command = state.command or data.input.command or data.input.code or ""
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, {
    max_lines = ctx.tool_output_lines[data.tool] or ctx.tool_output_lines.other,
    keep = "tail",
    max_line_bytes = output_limits.DEFAULT_MAX_LINE_BYTES,
  })
  local lines = maki.split(command:gsub("\n+$", ""), "\n")
  local shown = data.expanded and #lines or math.min(#lines, MAX_SCRIPT_LINES)
  local script = table.concat(lines, "\n", 1, shown)
  local header = maki.ui.highlight(script, language) or maki.split(script, "\n")
  if language == "py" then
    local fmt = ToolView.line_nr_fmt(shown) .. " "
    for i, row in ipairs(header) do
      row = type(row) == "string" and { { row } } or row
      table.insert(row, 1, { string.format(fmt, i), "line_nr" })
      header[i] = row
    end
  end
  if shown < #lines then
    header[#header + 1] = { { "... (" .. (#lines - shown) .. " lines) (click to expand)", "dim" } }
  end
  view:set_header(header)
  local text
  if data.status == "running" then
    text = state.text or data.stream or ""
    if text == "" then
      text = "Waiting for output..."
    end
  else
    local output = data.output or {}
    text = output.Plain and output.Plain.text or ""
    if (text == "cancelled" or text == "timeout") and state.text and state.text ~= "" then
      text = state.text .. "\n" .. text
    end
    if text == "" or text == "Exit code: 0" or text == "(no output)" then
      text = "No output"
    end
  end
  view:append_text(text)
  view:finish()
  if data.expanded then
    view:toggle()
  end
  return buf
end

return M
