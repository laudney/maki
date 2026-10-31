local path_text = require("maki.path_text")

-- `path` against the drawing process's own directories. A renderer uses
-- `maki.path_text` with its ctx instead, so it shortens against the session
-- the call ran in.
local function shorten_path(path)
  return path_text.shorten(path, maki.uv.cwd(), maki.uv.os_homedir())
end

return shorten_path
