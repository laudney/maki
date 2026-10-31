local SKILL_FILE = "SKILL.md"
local NOT_FOUND = "skill not found: "
local REFERENCE_FILE = "lua-api.md"
local REFERENCE_UNAVAILABLE = "(unavailable; full reference inlined below)"
local output_limits = require("maki.output_limits")
local helpers = require("skill_helpers")
local parse_frontmatter = helpers.parse_frontmatter
local build_skill_list = helpers.build_skill_list
local format_output = helpers.format_output

local PROJECT_SKILL_DIRS = {
  ".maki/skills",
  ".claude/skills",
  ".opencode/skills",
  ".agents/skills",
}
local GLOBAL_SKILL_DIRS = {
  ".claude/skills",
  ".config/opencode/skills",
  ".agents/skills",
}

local function scan_skill_dir(dir, skills)
  local entries = maki.fs.dir(dir)
  if not entries then
    return
  end
  for _, entry in ipairs(entries) do
    if entry[2] == "directory" then
      local skill_path = maki.fs.joinpath(dir, entry[1], SKILL_FILE)
      local content = maki.fs.read(skill_path)
      if content then
        local fm, body = parse_frontmatter(content)
        if body and #body > 0 then
          local name = (fm and fm.name) or entry[1]
          skills[name] = {
            name = name,
            description = (fm and fm.description) or "",
            content = body,
            location = skill_path,
          }
        end
      end
    end
  end
end

local function find_project_ancestors(cwd)
  if not cwd then
    return {}
  end
  local dirs = { cwd }
  if maki.fs.metadata(maki.fs.joinpath(cwd, ".git")) then
    return dirs
  end
  for _, parent in ipairs(maki.fs.parents(cwd)) do
    dirs[#dirs + 1] = parent
    if maki.fs.metadata(maki.fs.joinpath(parent, ".git")) then
      break
    end
  end
  return dirs
end

local opts = maki.api.register_options({
  plugin_dev = { default = true, desc = "Offer the builtin maki-plugin-dev skill for writing maki plugins." },
})

local ok, builtin, reference = pcall(function()
  return require("plugin_dev"), require("plugin_dev_reference")
end)
if not ok then
  maki.log.warn("builtin plugin_dev skill unavailable: " .. tostring(builtin))
  builtin = nil
end

local function resolve_builtin_content()
  local state = maki.env.state_dir()
  if state then
    local dir = maki.fs.joinpath(state, "docs")
    local path = maki.fs.joinpath(dir, REFERENCE_FILE)
    local _, err = maki.fs.mkdir(dir, { parents = true })
    if not err then
      _, err = maki.fs.write(path, reference.content)
    end
    if not err then
      return (builtin.content:gsub(builtin.reference_placeholder, function()
        return path
      end))
    end
    maki.log.warn("failed to write lua api reference to " .. path .. ": " .. tostring(err))
  end
  local content = builtin.content:gsub(builtin.reference_placeholder, REFERENCE_UNAVAILABLE)
  return content .. "\n---\n\n" .. reference.content
end

-- Project skills come from {cwd} and its parents up to the repository root.
local function discover_skills(cwd)
  local skills = {}

  if builtin and opts.plugin_dev then
    skills[builtin.name] = {
      name = builtin.name,
      description = builtin.description,
      content = builtin.content,
      location = "builtin:" .. builtin.name,
      resolve = resolve_builtin_content,
    }
  end

  local config = maki.env.config_dir()
  if config then
    scan_skill_dir(maki.fs.joinpath(config, "skills"), skills)
  end

  local home = maki.uv.os_homedir()
  if home then
    for _, rel in ipairs(GLOBAL_SKILL_DIRS) do
      scan_skill_dir(maki.fs.joinpath(home, rel), skills)
    end
  end

  for _, ancestor in ipairs(find_project_ancestors(cwd)) do
    for _, rel in ipairs(PROJECT_SKILL_DIRS) do
      scan_skill_dir(maki.fs.joinpath(ancestor, rel), skills)
    end
  end

  return skills
end

local boot_skills = discover_skills(maki.uv.cwd())
local description = "Load a skill that provides instructions and workflows for specific tasks."
  .. build_skill_list(boot_skills)

maki.api.register_tool({
  name = "skill",
  kind = "read",
  description = description,

  schema = {
    type = "object",
    properties = {
      name = { type = "string", description = "Name of the skill to load", required = true },
    },
  },

  -- The state keeps the skill's location and content, so its renderer
  -- reads them without parsing the model's text.

  summary = function(input)
    return input.name
  end,

  handler = function(input, ctx)
    if not input.name then
      return { llm_output = "error: name is required", is_error = true }
    end

    -- The session's working directory, which an ACP session may set apart
    -- from the agent process's.
    local place = ctx:place()
    local skills = discover_skills(place and place.cwd or maki.uv.cwd())
    local skill = skills[input.name]
    if not skill then
      local available = build_skill_list(skills)
      return { llm_output = NOT_FOUND .. input.name .. available, is_error = true }
    end
    if skill.resolve then
      skill.content = skill.resolve()
    end

    local kept = #skill.location + #skill.content <= output_limits.STATE_TEXT_MAX_BYTES
    return {
      llm_output = format_output(skill.location, skill.content),
      state = kept and { location = skill.location, content = skill.content } or nil,
    }
  end,
})
