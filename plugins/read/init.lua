local shorten_path = require("maki.shorten_path")
local output_limits = require("maki.output_limits")

local DESCRIPTION = [[Read a file. Returns contents with line numbers (1-indexed).

- Supports absolute, relative, and ~/ paths.
- **offset** and **limit** are required. Use offset=1 to read from the first line.
- Use limit=0 to read until the end of file (capped at 2000 lines).
- Use the **index** tool or **grep** tool first to find the offset and limit.
- Only read the sections you actually need.
- Use `wc -l` to check total number of lines before reading to decide a reasonable limit.
- Use truncation hints (e.g. "truncated lines X-Y") to continue with the correct offset.
- Do not reread the same range (same file and same offset).
- Prefer grep to locate content instead of scanning full files.
- Call in parallel when reading multiple files.
- Avoid tiny repeated slices - read a larger window if you need more context.]]

local opts = maki.api.register_options({
  max_line_bytes = { default = 500, min = 80, desc = "Truncate lines the model sees at this many bytes." },
  max_output_lines = output_limits.specs.max_output_lines,
})

local function read_file(path, offset, limit, ctx)
  local start = math.max(math.floor(offset), 1)
  local max_output_lines = output_limits.resolve(opts, ctx)
  local max_lines = limit > 0 and limit or nil
  if max_output_lines then
    max_lines = math.min(max_lines or max_output_lines, max_output_lines)
  end

  local window, err = maki.fs.read_lines(path, {
    offset = start,
    limit = max_lines,
    max_line_bytes = output_limits.line_bytes(opts, ctx),
    count = true,
  })
  if not window then
    return { llm_output = "read error: " .. tostring(err), is_error = true }
  end
  local lines, total_lines = window.lines, window.total

  ctx:record_read(path)

  local parts = {}
  local nr_fmt = "%" .. #tostring(start + #lines - 1) .. "d: %s"
  for i, line in ipairs(lines) do
    parts[#parts + 1] = string.format(nr_fmt, start + i - 1, line)
  end
  local llm_output = table.concat(parts, "\n")

  local trunc_start = start + #lines
  if trunc_start <= total_lines then
    llm_output = llm_output
      .. string.format(
        "\n\n...\n\nTruncated lines: %d-%d. Use offset=%d to read further.",
        trunc_start,
        total_lines,
        trunc_start
      )
  end

  local shown = #lines
  local annotation = shown < total_lines and string.format("%d of %d lines", shown, total_lines)
    or string.format("%d lines", shown)

  local basename = path:match("([^/]+)$")
  local parent = maki.fs.dirname(path)
  if parent and not ctx:is_instruction_file(basename) then
    ctx:load_instructions(parent)
  end

  return {
    llm_output = llm_output,
    annotation = annotation,
  }
end

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = [[
- When using the **read** tool, only read the sections you actually need.
- Use `wc -l` to check total number of lines before reading to decide a reasonable **read** tool limit.]],
})

maki.api.register_tool({
  name = "read",
  permission = "fs_read",
  kind = "read",
  description = DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Absolute path to the file",
        required = true,
        alias = "file_path",
      },
      offset = {
        type = "integer",
        description = "Line number to start from (1-indexed). Use 1 for the first line.",
        required = true,
      },
      limit = {
        type = "integer",
        description = "Max number of lines to read. Use 0 to read until end of file (capped at 2000 lines).",
        required = true,
      },
    },
  },

  summary = function(input)
    local path = shorten_path(input.path or input.file_path or "")
    local start = input.offset or 1
    if input.limit and input.limit > 0 then
      return path .. ":" .. start .. "-" .. (start + input.limit - 1)
    end
    return path .. ":" .. start
  end,

  handler = function(input, ctx)
    local raw = input.path
    if not raw then
      return { llm_output = "error: path is required", is_error = true }
    end
    local path = maki.fs.abspath(raw)
    local meta = maki.fs.metadata(path)
    if not meta then
      return { llm_output = "error: path not found: " .. path, is_error = true }
    end
    if meta.is_dir then
      return { llm_output = "error: path is a directory, use the list tool instead", is_error = true }
    end
    return read_file(path, input.offset, input.limit, ctx)
  end,
})
