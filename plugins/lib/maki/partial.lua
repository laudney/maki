-- When a tool is cut short, it still hands back what it printed. The marker
-- tells the model that output is real but unfinished. One home for the
-- wording, so every tool says it the same way.
local M = {}

local CANCELLED_FMT = "[cancelled by user after %s, %s]"
local STOPPED_FMT = "[stopped by task_stop after %s, %s]"
local TIMEOUT_FMT = "[timed out after %ds, %s]"
-- Never claim output the model cannot see above the marker: it would go
-- looking for it, or invent it.
local SOME_OUTPUT = "output above is partial"
local NO_OUTPUT = "no output before the cut"

--- The marker that closes {out}, everything the tool streamed, already
--- truncated. {reason} is a cancel-hook reason ("cancelled" | "timeout" | "stopped").
--- {started_at} is the `os.time()` the tool began at.
function M.marker(out, reason, timeout_secs, started_at)
  local tail = out ~= "" and SOME_OUTPUT or NO_OUTPUT
  if reason == "timeout" then
    return TIMEOUT_FMT:format(timeout_secs, tail)
  end
  return (reason == "stopped" and STOPPED_FMT or CANCELLED_FMT):format(
    maki.text.humantime(math.max(os.time() - started_at, 0)),
    tail
  )
end

--- The tool reply that hands the model {out} closed on {marker}.
function M.reply(out, marker)
  return {
    llm_output = (out ~= "" and out .. "\n" or "") .. marker,
    is_error = true,
  }
end

return M
