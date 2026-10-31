local execution_view = require("maki.execution_view")

maki.ui.register_renderer("code_execution", {
  body = function(data, ctx)
    return execution_view.draw(data, ctx, "py")
  end,
  click = function(data)
    data.expanded = not data.expanded
  end,
})
