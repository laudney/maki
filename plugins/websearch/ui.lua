local web_view = require("maki.web_view")

maki.ui.register_renderer("websearch", {

  header = function(data)
    return data.input.query
  end,

  body = web_view.body,
  click = web_view.click,
})
