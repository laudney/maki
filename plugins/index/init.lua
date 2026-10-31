local dir_listing = require("maki.dir_listing")
local indexer = require("indexer")
local shorten_path = require("maki.shorten_path")

local opts = maki.api.register_options({
  max_file_size_mb = { default = 2, min = 1, desc = "Refuse to index files larger than this many MB." },
})

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Use the **index** tool first on individual files to get their skeleton, then use the **read** tool with offset/limit for the specific section you need.",
})

maki.api.register_prompt_hint({
  slot = "efficient_tools",
  content = "index",
})

maki.api.register_tool({
  name = "index",
  permission = "fs_read",
  kind = "read",
  description = [[
Return a compact overview of a source file: imports, type definitions, function signatures, and structure with their line numbers surrounded by []. ~70-90% more efficient than reading the full file.

- Use this FIRST to understand file structure before using read with offset/limit.
- Supports source files in different programming languages and markdown.
- Falls back with an error on unsupported languages. Use read instead.]],

  schema = {
    type = "object",
    properties = {
      path = { type = "string", description = "Absolute path to the file", required = true },
    },
  },
  summary = function(input)
    return shorten_path(input.path or "")
  end,
  handler = function(input, ctx)
    local path = input.path
    if not path then
      return { llm_output = "error: path is required", is_error = true }
    end

    local meta = maki.fs.metadata(path)
    if not meta then
      return { llm_output = "error: path not found: " .. path, is_error = true }
    end
    if meta.is_dir then
      local listing, err = dir_listing.list(path, ctx)
      if not listing then
        return { llm_output = "error: " .. tostring(err), is_error = true }
      end
      return {
        llm_output = listing.text,
        annotation = listing.count .. " entries",
      }
    end

    local filename = path:match("([^/]+)$")
    local lang = indexer.FILENAME_TO_LANG[filename]

    if not lang then
      local ext = path:match("%.([^%.]+)$")
      if not ext then
        return { llm_output = "Unsupported file type: (no extension). Use the read tool instead.", is_error = true }
      end

      lang = indexer.EXT_TO_LANG[ext]
      if not lang then
        return { llm_output = "Unsupported file type: ." .. ext .. ". Use the read tool instead.", is_error = true }
      end
    end

    local max_file_size = opts.max_file_size_mb * 1024 * 1024
    if meta and meta.size > max_file_size then
      return {
        llm_output = "error: File too large ("
          .. meta.size
          .. " bytes, max "
          .. max_file_size
          .. "). Use read with offset/limit instead.",
        is_error = true,
      }
    end

    local source, err = maki.fs.read(path)
    if not source then
      return { llm_output = "error: " .. err, is_error = true }
    end

    local skeleton, line_meta = indexer.index_source(source, lang)
    if not skeleton then
      return { llm_output = "error: " .. tostring(line_meta), is_error = true }
    end

    return {
      llm_output = skeleton:gsub("\n+$", ""),
    }
  end,
})
