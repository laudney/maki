local ToolView = require("maki.tool_view")
local path_text = require("maki.path_text")
local parse_output = require("skill_helpers").parse_output

local DEFAULT_MAX_LINES = 20
local DEFAULT_EXT = "md"

local function view_opts(ctx)
  return { max_lines = ctx.tool_output_lines.other or DEFAULT_MAX_LINES, keep = "head" }
end

-- The loaded skill's location and content from the state, else parsed from
-- the model's text: a call from before the state, or one too big to keep.
-- Nil until it is done, when it failed, or when the text is no skill.
local function loaded_skill(data)
  if data.status ~= "done" then
    return nil
  end
  local state = data.state
  if state and state.location then
    return state.location, state.content
  end
  return parse_output(data.output.Plain.text)
end

-- The content with line numbers, highlighted for the location's extension.
local function skill_view(location, content, ctx, expanded)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, view_opts(ctx))
  view:set_highlight(content, location:match("%.([^%.]+)$") or DEFAULT_EXT)
  view:finish()
  if expanded then
    view:toggle()
  end
  return buf
end

maki.ui.register_renderer("skill", {

  header = function(data, ctx)
    local location = loaded_skill(data)
    if not location then
      return data.input.name
    end
    local buf = maki.ui.buf()
    buf:line({ { path_text.shorten(location, ctx.cwd, ctx.home), "path" } })
    return buf
  end,

  -- A failed call draws its error as the default does, as it did live. A
  -- done call whose text is no skill draws that text, as the old restore did.
  body = function(data, ctx)
    if data.status ~= "done" then
      return nil
    end
    local location, content = loaded_skill(data)
    if not location then
      return ToolView.draw(data.output.Plain.text, view_opts(ctx), data.expanded)
    end
    return skill_view(location, content, ctx, data.expanded)
  end,

  click = function(data)
    data.expanded = not data.expanded
  end,
})
