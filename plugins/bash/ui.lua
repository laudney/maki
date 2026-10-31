local execution_view = require("maki.execution_view")
local path_text = require("maki.path_text")

local function command_and_dir(input)
  if input.workdir then
    return input.command, input.workdir
  end
  local dir, command = (input.command or ""):match("^cd%s+(.-)%s+&&%s+(.+)$")
  if dir then
    local quote = dir:sub(1, 1)
    if (quote == "'" or quote == '"') and dir:sub(-1) == quote then
      dir = dir:sub(2, -2)
    end
    return command, dir
  end
  return input.command, nil
end

maki.ui.register_renderer("bash", {
  header = function(data, ctx)
    local command, dir = command_and_dir(data.input)
    local title = data.input.description
    if not title or not title:find("%S") or title == data.input.command or title == command then
      title = ""
    end
    if dir and ctx.cwd and dir:gsub("\\", "/") == ctx.cwd:gsub("\\", "/") then
      dir = nil
    end
    dir = dir and path_text.shorten(dir, ctx.cwd, ctx.home)
    if dir and dir ~= "." then
      title = title == "" and "in " .. dir or title .. " in " .. dir
    end
    return title
  end,
  body = function(data, ctx)
    if type(data.state) ~= "table" then
      local command = command_and_dir(data.input)
      data.state = { command = command }
    end
    return execution_view.draw(data, ctx, "bash")
  end,
  click = function(data)
    data.expanded = not data.expanded
  end,
})
