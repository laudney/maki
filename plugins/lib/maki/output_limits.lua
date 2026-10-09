-- Shared per-tool output limit options, so the tools that support them
-- cannot drift apart.

local DEFAULT_MAX_OUTPUT_LINES = 2000
local DEFAULT_MAX_OUTPUT_BYTES = 50 * 1024
local DEFAULT_MAX_LINE_BYTES = 500
local NEWLINE_BYTE = string.byte("\n")

local M = {}

M.DEFAULT_MAX_LINE_BYTES = DEFAULT_MAX_LINE_BYTES
M.specs = {
  max_output_lines = { type = "integer", desc = "Override `agent.max_output_lines` for this tool's model output." },
  max_output_bytes = { type = "integer", desc = "Override `agent.max_output_bytes` for this tool's model output." },
}

function M.extend(spec)
  for name, s in pairs(M.specs) do
    spec[name] = s
  end
  return spec
end

--- Model presentation limits, or nil, nil for full results.
function M.resolve(opts, ctx)
  if ctx:output_mode() == "full" then
    return nil, nil
  end
  return opts.max_output_lines or ctx:config("max_output_lines", DEFAULT_MAX_OUTPUT_LINES),
    opts.max_output_bytes or ctx:config("max_output_bytes", DEFAULT_MAX_OUTPUT_BYTES)
end

--- Zero disables the line-byte presentation limit for full results.
function M.line_bytes(opts, ctx)
  return ctx:output_mode() == "full" and 0 or opts.max_line_bytes
end

--- Last {n} lines of {text}, or all of it when it has fewer. Newlines separate
--- lines here rather than terminate them, so a trailing one is an empty last
--- line and counts as one.
function M.tail(text, n)
  local seen = 0
  for i = #text, 1, -1 do
    if text:byte(i) == NEWLINE_BYTE then
      seen = seen + 1
      if seen == n then
        return text:sub(i + 1)
      end
    end
  end
  return text
end

return M
