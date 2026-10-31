local web_view = require("maki.web_view")

local DEFAULT_FORMAT = "markdown"

maki.ui.register_renderer("webfetch", {

  header = function(data)
    local input = data.input
    local fmt = input.format
    if fmt and fmt ~= DEFAULT_FORMAT then
      return input.url .. " [" .. fmt .. "]"
    end
    return input.url
  end,

  body = web_view.body,
  click = web_view.click,
})
