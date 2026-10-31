local MAX_BATCH_SIZE = 25
local ERROR_PREFIX = "[ERROR] "
local EMPTY_ERROR = "provide at least one tool call"
local NESTED_ERROR = "cannot nest batch inside batch"
local CANCELLED_ERROR = "cancelled"
local DISCARDED_ERROR = string.format("maximum of %d tools per batch", MAX_BATCH_SIZE)
local WAITING_FMT = "batch is waiting on %s"
local NOTHING_RUNNING = "batch has no running tool left to move"
local MOVED_HEADER = "Still running in the background:"
local BACKGROUND_PARAM = "background"
local FOREGROUND_NOTE = "Could not start in the background (%s). Ran in the foreground.\n\n"
local MOVED_ITEM_FMT = "- %s"
local MOVED_ITEM_HEADER_FMT = ": %s"
local SECTION_FMT = "## %s\n"
local SUMMARY_MIXED_FMT = "Executed %d/%d successfully. %d failed."
local SUMMARY_ALL_OK_FMT = "All %d tools executed successfully."
local RESETTLE_FMT = "batch: child %s settled twice (%s -> %s)"
local STATUS = { PENDING = "pending", RUNNING = "running", SUCCESS = "success", ERROR = "error" }
local TERMINAL = { [STATUS.SUCCESS] = true, [STATUS.ERROR] = true }

local PART_STATUS = {
  [STATUS.PENDING] = "working",
  [STATUS.RUNNING] = "working",
  [STATUS.SUCCESS] = "done",
  [STATUS.ERROR] = "error",
}
-- A child the stop cut short did not fail on its own, so it reads cancelled.
local CANCELLED_PART = "cancelled"

local description = string.format(
  [[Executes multiple independent tool calls concurrently to reduce round-trips.

ALWAYS USE THE BATCH TOOL WHEN YOU HAVE MULTIPLE INDEPENDENT TOOL CALLS. This dramatically improves performance.

Rules:
- 1-%d tool calls per batch
- All calls run in parallel; order NOT guaranteed
- Partial failures do not stop other calls
- Do NOT nest batch inside batch
- Do NOT use for dependent operations or when filtering results (use code_execution)]],
  MAX_BATCH_SIZE
)

local schema = {
  type = "object",
  properties = {
    tool_calls = {
      type = "array",
      description = "Array of tool calls to execute in parallel",
      required = true,
      items = {
        description = "Tool invocation: { tool: string, parameters: object } or flat { tool: string, ...params }",
      },
    },
  },
}

local examples = {
  {
    tool_calls = {
      { tool = "glob", parameters = { pattern = "src/**/*.ts" } },
      { tool = "grep", parameters = { pattern = "import", include = "*.ts" } },
      { tool = "index", parameters = { path = "/project/index.ts" } },
    },
  },
}

-- Models send entries in two shapes, { tool, parameters } and flat
-- { tool, ...params }, so accept either, or even both merged, as long
-- as no key appears twice.
local function normalize_entry(entry)
  if type(entry) ~= "table" then
    return nil, "batch entry must be an object"
  end
  local tool = entry.tool
  if type(tool) ~= "string" then
    return nil, "batch entry missing 'tool'"
  end
  -- Lua twin of `canonical_tool_name` (streaming.rs): strip GPT's
  -- `functions.` prefix so headers and the nested-batch guard match.
  tool = tool:match("^functions%.(.+)") or tool
  local rest = {}
  local has_rest = false
  for k, v in pairs(entry) do
    if k ~= "tool" and k ~= "parameters" then
      rest[k] = v
      has_rest = true
    end
  end
  local nested = entry.parameters
  local params
  if nested == nil then
    if not has_rest then
      return nil, "batch entry missing 'parameters'"
    end
    params = rest
  elseif not has_rest then
    params = nested
  elseif type(nested) ~= "table" then
    return nil, "'parameters' must be an object when flat fields are also present"
  else
    params = rest
    for k, v in pairs(nested) do
      if params[k] ~= nil then
        return nil, "duplicate parameter '" .. k .. "' in both 'parameters' and flat fields"
      end
      params[k] = v
    end
  end
  return { tool = tool, params = params }
end

local function wants_background(tool, params)
  if params[BACKGROUND_PARAM] ~= true then
    return false
  end
  local t = maki.api.get_tool(tool)
  return t ~= nil and table.find(t.background_params or {}, BACKGROUND_PARAM) ~= nil
end

-- Parsing plus per-entry policy: entries past MAX_BATCH_SIZE and nested
-- batches are born terminal (error), so they render but never run. Only
-- a malformed entry fails the batch as a whole, and that happens before
-- anything runs.
local function prepare_children(tool_calls)
  if type(tool_calls) ~= "table" then
    return nil, "tool_calls must be an array"
  end
  local children = {}
  for i, entry in ipairs(tool_calls) do
    local c, err = normalize_entry(entry)
    if not c then
      return nil, err
    end
    c.status = STATUS.PENDING
    if i > MAX_BATCH_SIZE then
      c.status, c.output = STATUS.ERROR, DISCARDED_ERROR
    elseif c.tool == "batch" then
      c.status, c.output = STATUS.ERROR, NESTED_ERROR
    end
    children[i] = c
  end
  return children
end

-- batch_policy.rs pins this exact llm format; keep it byte-identical.
-- Returns the sections and how many of them failed, so the move receipt
-- can reuse them without the summary line.
local function render_sections(children)
  local out = {}
  local failed = 0
  for _, c in ipairs(children) do
    out[#out + 1] = string.format(SECTION_FMT, c.tool)
    if c.status == STATUS.SUCCESS then
      out[#out + 1] = c.output or ""
    else
      failed = failed + 1
      out[#out + 1] = ERROR_PREFIX .. (c.output or "")
    end
    out[#out + 1] = "\n\n"
  end
  return table.concat(out), failed
end

local function render_llm(children)
  local total = #children
  local sections, failed = render_sections(children)
  if failed > 0 then
    return sections .. string.format(SUMMARY_MIXED_FMT, total - failed, total, failed)
  end
  return sections .. string.format(SUMMARY_ALL_OK_FMT, total)
end

local Batch = {}
Batch.__index = Batch

function Batch:state()
  local children = {}
  for i, child in ipairs(self.children) do
    children[i] = {
      tool = child.tool,
      status = child.status,
      index = child.index,
      output = (not child.index or child.status == STATUS.ERROR) and child.output or nil,
    }
  end
  return { children = children }
end

function Batch:publish()
  self.ctx:set_state(self:state())
end

function Batch:settle(c, status, output)
  if TERMINAL[c.status] and not c.swept then
    error(string.format(RESETTLE_FMT, tostring(c.tool), tostring(c.status), tostring(status)))
  end
  c.swept = nil
  c.status, c.output = status, output
  c.stopped = status == STATUS.ERROR and self.ctx:cancelled()
  self:publish()
  self:publish_parts()
  self:sync_movable()
end

function Batch:reply()
  local note = self.foreground_reason and string.format(FOREGROUND_NOTE, tostring(self.foreground_reason)) or ""
  return {
    llm_output = note .. render_llm(self:unreported()),
    is_error = self.cancelled,
    state = self:state(),
  }
end

function Batch:sweep_cancelled(reason)
  for _, c in ipairs(self.children) do
    if not TERMINAL[c.status] then
      self:settle(c, STATUS.ERROR, reason)
      c.swept = true
    end
  end
end

function Batch:run_child(c, ctx)
  -- `gather` runs every fun it was handed, cancel or not, so a child the
  -- sweep already settled would go back to running and dispatch anyway.
  if TERMINAL[c.status] then
    return
  end
  c.status = STATUS.RUNNING
  self:publish()
  local text, err = maki.agent.call_tool(ctx, c.tool, c.params, {
    output_mode = ctx:output_mode(),
    on_progress = function(update)
      if #update.path == 0 and update.child then
        c.summary = update.child.summary
        if not c.index then
          c.index = update.child.index
          self:publish()
        end
      end
    end,
    on_movable = function(movable)
      c.movable = movable
      self:sync_movable()
    end,
    on_task = function()
      c.subagent = true
      self:publish_parts()
    end,
  })
  -- The sweep may have settled this child mid-call, and the call knows
  -- nothing about that, so its result is moot. Unless it came back with
  -- more than the sweep's bare reason: that partial output is worth
  -- keeping.
  if TERMINAL[c.status] then
    if err and c.swept and err ~= c.output then
      self:settle(c, STATUS.ERROR, err)
    end
    return
  end
  if err then
    self:settle(c, STATUS.ERROR, err)
  else
    self:settle(c, STATUS.SUCCESS, text)
  end
end

local function header_text(c)
  return c.summary or c.tool
end

function Batch:unreported()
  local moved = self.ctx:backgrounded()
  local out = {}
  for _, c in ipairs(self.children) do
    if not (moved and c.reported) then
      out[#out + 1] = c
    end
  end
  return out
end

-- Only advertise a move while every running child can survive it.
function Batch:movable_now()
  local running = false
  for _, c in ipairs(self.children) do
    if not TERMINAL[c.status] then
      if not c.movable then
        return false
      end
      running = true
    end
  end
  return running
end

function Batch:sync_movable()
  if not self.hooked then
    return
  end
  local movable = self:movable_now()
  if movable ~= self.shown_movable then
    self.shown_movable = movable
    self.ctx:set_movable(movable)
  end
  self:background_if_due()
end

-- A child cannot move on its own inside a batch. So when only children that
-- asked for the background are still running, the batch moves itself for
-- them, and the model gets the other results right away, just as if it had
-- made those calls alone. It tries once. A batch that cannot move tells the
-- model why in its reply.

function Batch:background_if_due()
  if self.move_tried or self.ctx:backgrounded() then
    return
  end
  local asked = false
  for _, c in ipairs(self.children) do
    if not TERMINAL[c.status] then
      if not (c.background and c.movable) then
        return
      end
      asked = true
    end
  end
  if not asked then
    return
  end
  self.move_tried = true
  maki.async.run(function()
    local moved, err = self.ctx:background()
    -- Everything finished in the meantime, so there is nothing to explain.
    if not moved and err ~= NOTHING_RUNNING then
      self.foreground_reason = err
    end
  end)
end

function Batch:publish_parts()
  if not self.hooked then
    return
  end
  local parts = {}
  for i, c in ipairs(self.children) do
    if not c.subagent then
      -- A bash call without a description has an empty title.
      local header = header_text(c)
      parts[#parts + 1] = {
        id = tostring(i),
        name = header ~= "" and header or c.tool,
        tool = c.tool,
        status = c.stopped and CANCELLED_PART or PART_STATUS[c.status],
      }
    end
  end
  self.ctx:set_parts(parts)
end

-- Build the receipt before marking finished children as reported.
function Batch:hand_over()
  local finished, running = {}, {}
  for _, c in ipairs(self.children) do
    if TERMINAL[c.status] then
      finished[#finished + 1] = c
    elseif c.movable then
      running[#running + 1] = c
    else
      return nil, string.format(WAITING_FMT, c.tool)
    end
  end
  if #running == 0 then
    return nil, NOTHING_RUNNING
  end
  local items = {}
  for i, c in ipairs(running) do
    local header = header_text(c)
    items[i] = string.format(MOVED_ITEM_FMT, c.tool)
      .. (header ~= "" and string.format(MOVED_ITEM_HEADER_FMT, header) or "")
  end
  local sections = render_sections(finished)
  for _, c in ipairs(finished) do
    c.reported = true
  end
  return sections .. MOVED_HEADER .. "\n" .. table.concat(items, "\n")
end

function Batch:run(ctx)
  local funs = {}
  for _, c in ipairs(self.children) do
    if c.status == STATUS.PENDING then
      funs[#funs + 1] = function()
        self:run_child(c, ctx)
      end
    end
  end
  -- The cancel reaches neither `call_tool` nor `gather`, so a child parked
  -- in a request would stay drawn as running until the host gives up on
  -- the whole handler, seconds later. The finish covers the case where the
  -- host gives up on us anyway: the model still gets per-child results
  -- rather than a bare "cancelled".
  -- The reason is the model's only clue why the children stopped: a
  -- deadline must not read as an Esc it can never retry its way out of.
  maki.async.on_cancel(function(reason)
    self.cancelled = true
    self.cut_reason = reason
    self:sweep_cancelled(reason)
    ctx:finish(self:reply())
  end)
  maki.async.gather(funs)
  self:sweep_cancelled(self.cut_reason or CANCELLED_ERROR)
end

local function handler(input, ctx)
  local children, err = prepare_children(input.tool_calls)
  if not children then
    return { llm_output = err, is_error = true }
  end
  if #children == 0 then
    return { llm_output = EMPTY_ERROR, is_error = true }
  end
  local batch = setmetatable({ children = children, ctx = ctx }, Batch)
  batch:publish()
  -- Registered up front, so a press that cannot move the batch is answered
  -- with the blocking child's name, while `sync_movable` hides the hint.
  -- Only a model's call can register. A nested batch simply never moves.
  -- Registering shows the hint, and no child is running yet to back it.
  batch.hooked = ctx:on_background(function()
    return batch:hand_over()
  end) == true
  batch.shown_movable = batch.hooked
  -- A batch that cannot move leaves the ask with the child, which then
  -- explains for itself why it ran in the foreground.
  if batch.hooked then
    for _, c in ipairs(children) do
      if wants_background(c.tool, c.params) then
        c.background = true
        c.params[BACKGROUND_PARAM] = nil
      end
    end
  end
  batch:publish_parts()
  batch:sync_movable()
  batch:run(ctx)
  return batch:reply()
end

maki.api.register_tool({
  name = "batch",
  description = description,
  kind = "execute",
  audiences = { "main", "research_sub", "general_sub" },
  schema = schema,
  examples = examples,
  summary = function(input)
    return #(input.tool_calls or {}) .. " tools"
  end,
  handler = handler,
})
