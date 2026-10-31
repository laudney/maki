local truncate = require("maki.truncate")
local shorten_path = require("maki.shorten_path")
local output_limits = require("maki.output_limits")

local NO_FILES_FOUND = "No files found"

local opts = maki.api.register_options(output_limits.extend({
  search_result_limit = { default = 100, min = 10, desc = "Max files the model sees per search." },
}))

maki.api.register_tool({
  name = "glob",
  permission = "fs_read",
  kind = "search",
  description = [[Find files by glob pattern.

- Respects .gitignore.
- Returns absolute paths sorted by modification time (newest first).
- Prefer speculative parallel searches over sequential rounds of glob+grep.]],

  schema = {
    type = "object",
    properties = {
      pattern = { type = "string", description = "Glob pattern (e.g. **/*.rs, src/**/*.ts)", required = true },
      path = { type = "string", description = "Directory to search in (default: cwd)" },
    },
  },

  summary = function(input)
    local summary = shorten_path(input.pattern or "")
    if input.path then
      summary = summary .. " in " .. shorten_path(input.path)
    end
    return summary
  end,

  handler = function(input, ctx)
    local pattern = input.pattern
    if not pattern then
      return { llm_output = "error: pattern is required", is_error = true }
    end

    local max_lines, max_bytes = output_limits.resolve(opts, ctx)

    local files, err = maki.fs.glob(pattern, {
      path = input.path,
      gitignore = true,
      sort = "mtime",
      limit = ctx:output_mode() == "compact" and opts.search_result_limit or nil,
    })

    if not files then
      return { llm_output = "error: " .. err, is_error = true }
    end

    if #files == 0 then
      return { llm_output = NO_FILES_FOUND }
    end

    local lines = {}
    for i, f in ipairs(files) do
      lines[i] = shorten_path(f)
    end
    local text = table.concat(lines, "\n")
    local llm_output = truncate(text, max_lines, max_bytes)

    return { llm_output = llm_output }
  end,
})
