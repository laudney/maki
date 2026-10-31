local helpers = require("memory_helpers")

local WRITE_TOOLS = { "write", "edit", "multiedit", "edit_lines", "insert_lines" }

-- Notes live outside cwd, where file-write tools normally prompt; pre-allow
-- them here so the agent can edit notes directly. Reads may come from the
-- legacy dir while writes go to the state dir, so cover both.
local function register_write_rules()
  local suffix = helpers.memories_path_suffix()
  local dirs = { helpers.legacy_dir_if_exists(suffix) }
  local state = maki.env.state_dir()
  if state then
    dirs[#dirs + 1] = maki.fs.joinpath(state, suffix)
  end
  for _, dir in ipairs(dirs) do
    for _, tool in ipairs(WRITE_TOOLS) do
      -- The edit sub-tools are opt-in, and a rule naming an unregistered tool
      -- is dropped with a warning. Ask first, or a default config logs that
      -- warning at every startup.
      if maki.api.get_tool(tool) then
        maki.api.register_permission_rule({ tool = tool, scope = dir .. "/**" })
      end
    end
  end
end
register_write_rules()

maki.api.register_prompt_hint({
  prompt = "system",
  slot = "after_instructions",
  content = function()
    local dir = helpers.resolve_dir(true)
    if not dir then
      return nil
    end
    local tag_line = helpers.format_tag_line(dir, helpers.MAX_TAGS)
    if not tag_line then
      return nil
    end
    return "\n\nMemory tags (memory tool, `read tags=[...]`): " .. tag_line .. "\n"
  end,
})

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Proactively save non-obvious project gotchas and architecture decisions to **memory**.",
})

local function cmd_read(path, dir, ctx)
  local file_path, err = helpers.safe_resolve(dir, path)
  if not file_path then
    return nil, err
  end
  local content, err = maki.fs.read(file_path)
  if not content then
    return nil, "read error: " .. err
  end
  local formatted =
    helpers.cap_read_output(helpers.format_read_entry(path, #content, content), helpers.CAP_HINT_REWRITE, ctx)
  return {
    llm_output = formatted,
  }
end

local function cmd_write(path, content, tags, dir)
  local file_path, err = helpers.safe_resolve(dir, path)
  if not file_path then
    return nil, err
  end

  local size_err = helpers.validate_write_size(content)
  if size_err then
    return nil, size_err
  end
  local normalized, tag_err, note = helpers.validate_write_tags(tags or {})
  if tag_err then
    return nil, tag_err
  end
  local full = helpers.encode_frontmatter(normalized) .. content
  maki.fs.mkdir(dir, { parents = true })
  local ok, write_err = maki.fs.write(file_path, full)
  if not ok then
    return nil, "write error: " .. tostring(write_err)
  end
  return {
    llm_output = "wrote "
      .. path
      .. " (tags: "
      .. (#normalized > 0 and table.concat(normalized, ", ") or "none")
      .. ")"
      .. (note and ("; " .. note) or ""),
  }
end

local function cmd_delete(path, dir)
  local file_path, err = helpers.safe_resolve(dir, path)
  if not file_path then
    return nil, err
  end
  if not maki.fs.metadata(file_path) then
    return nil, "'" .. path .. "' does not exist"
  end
  local ok, rm_err = maki.fs.rm(file_path)
  if not ok then
    return nil, "delete error: " .. tostring(rm_err)
  end
  return "deleted " .. path
end

local function with_dir(res, dir)
  local prefix = "dir: " .. dir .. "\n\n"
  if type(res) == "string" then
    return prefix .. res
  end
  res.llm_output = prefix .. res.llm_output
  return res
end

maki.api.register_tool({
  name = "memory",
  description = "Persistent, project-scoped scratchpad for learnings, patterns, decisions, and gotchas across sessions.\n\n"
    .. "- Notes are retrieved by tag; reuse the tags from your system prompt when they fit.\n"
    .. "- Save important context before compaction or to build up project knowledge.\n"
    .. "- Keep entries concise and current. Delete outdated information.\n"
    .. "- Notes are plain files; `list` and `read` report the dir, so use the edit tool on `<dir>/<name>` for targeted changes.",

  schema = {
    type = "object",
    properties = {
      command = {
        type = "string",
        enum = { "list", "read", "write", "delete" },
        description = "- `list [tags]`: tag-grouped index, no bodies.\n"
          .. "- `read path|tags`: one body (path) or collated bodies (tags).\n"
          .. "- `write path tags content`: create or overwrite a note.\n"
          .. "- `delete path`",
        required = true,
      },
      path = {
        type = "string",
        description = "Relative path, e.g. 'architecture.md'.",
      },
      content = { type = "string", description = "Body for write (frontmatter added automatically)." },
      tags = {
        type = "array",
        items = { type = "string" },
        description = "snake_case tags. Filter for list/read; assigned on write (defaults to filename stem).",
      },
    },
  },

  summary = function(input)
    local parts = { input.command or "" }
    if input.path then
      parts[#parts + 1] = input.path
    elseif input.tags then
      parts[#parts + 1] = table.concat(input.tags, ",")
    end
    return table.concat(parts, " ")
  end,

  handler = function(input, ctx)
    if type(input.tags) == "string" then
      input.tags = { input.tags }
    end
    local cmd = input.command
    local dir, dir_err = helpers.resolve_dir(cmd ~= "write" and cmd ~= "delete")
    local verr = helpers.validate_input(input, dir)
    if verr then
      return { llm_output = "error: " .. verr, is_error = true }
    end
    if not dir then
      return { llm_output = "error: " .. dir_err, is_error = true }
    end

    local result, err
    if cmd == "list" then
      result, err = helpers.format_list(dir, input.tags, ctx)
    elseif cmd == "read" then
      if input.tags and #input.tags > 0 then
        result, err = helpers.format_read(dir, input.tags, ctx)
      else
        result, err = cmd_read(input.path, dir, ctx)
      end
    elseif cmd == "write" then
      result, err = cmd_write(input.path, input.content, input.tags, dir)
    elseif cmd == "delete" then
      result, err = cmd_delete(input.path, dir)
    end
    if err then
      return { llm_output = "error: " .. err, is_error = true }
    end
    if cmd == "list" or cmd == "read" then
      return with_dir(result, dir)
    end
    return result
  end,
})
