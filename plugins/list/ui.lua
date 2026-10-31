local dir_listing_view = require("maki.dir_listing_view")
local path_text = require("maki.path_text")

maki.ui.register_renderer("list", {

  header = function(data, ctx)
    local buf = maki.ui.buf()
    buf:line({ { path_text.shorten(data.input.path or "", ctx.cwd, ctx.home), "path" } })
    return buf
  end,

  -- A failed call draws its error as the default does, as it did live.
  body = function(data, ctx)
    if data.status ~= "done" then
      return nil
    end
    return dir_listing_view.draw(data.output.Plain.text, ctx, data.expanded)
  end,

  click = function(data)
    data.expanded = not data.expanded
  end,
})
