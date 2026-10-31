local path_text = require("maki.path_text")

-- No body: the TUI draws the output image itself.
maki.ui.register_renderer("view_image", {

  -- The input at start and in an old session is the model's, so the
  -- schema's `file_path` alias may stand for `path`.
  header = function(data, ctx)
    local path = data.input.path or data.input.file_path or ""
    local buf = maki.ui.buf()
    buf:line({ { path_text.shorten(path, ctx.cwd, ctx.home), "path" } })
    return buf
  end,
})
