-- Paths as text against a place given in full, so it reads no directory
-- and needs no permission. A renderer passes its `ctx.cwd` and `ctx.home`.

local M = {}

local function normalize_sep(s)
  return s:gsub("\\", "/")
end

-- {path} relative to {cwd}, else under {home} as `~/...`. Either can be nil.
function M.shorten(path, cwd, home)
  local p = normalize_sep(path)
  if cwd then
    cwd = normalize_sep(cwd)
    if p:sub(1, #cwd + 1) == cwd .. "/" then
      local rel = p:sub(#cwd + 2)
      return rel == "" and "." or rel
    end
  end
  if home then
    home = normalize_sep(home)
    if p:sub(1, #home + 1) == home .. "/" then
      local rel = p:sub(#home + 2)
      return rel == "" and "~" or "~/" .. rel
    end
  end
  return path
end

return M
