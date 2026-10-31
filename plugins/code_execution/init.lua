-- Policy for the Python interpreter: which tools it may call, what the model
-- sees (via the `describe(dctx)` callback), and the preamble. The sandbox and
-- dispatch live in Rust, which exposes primitives only (`maki.api.get_tools`,
-- `maki.agent.call_tool`); orchestration policy is here.

local output_limits = require("maki.output_limits")
local partial = require("maki.partial")

local DEFAULT_MAX_OUTPUT_LINES = 2000
local DEFAULT_MAX_OUTPUT_BYTES = 50 * 1024
local OPEN_READ_CHECK_LINES = 1
local READ_INPUT_SLOTS = { "tool.read.input", "tool.*.input" }
local LIST_INPUT_SLOTS = { "tool.list.input", "tool.*.input" }
local OPEN_READ_HOOKED_ERR = "open() cannot read while a plugin hooks the read tool, call read() instead"
local INSPECT_HOOKED_ERR = "os and pathlib cannot look at files while a plugin hooks the list tool, call list() instead"
local MKDIR_BASH_HINT = ", create the directory with the bash tool instead"
local OPEN_APPEND_UNSUPPORTED_ERR = "open() cannot append because the write tool has no append option"
local NO_OUTPUT = "(no output)"
local CANCELLED_ERR = "cancelled"
local TIME_LIMIT_SUBSTR = "time limit exceeded"
-- Same marker the batch tool uses for a failed child, so a failure reads the
-- same wherever the model meets it.
local ERROR_PREFIX = "[ERROR] "
local ASYNCIO_GATHER = "asyncio.gather"
local GATHER_HINT = "\n\nHint: `gather(...)` keeps the other results, returning `"
  .. ERROR_PREFIX
  .. "...` for the failed call."
-- `asyncio.gather` cancels its siblings the moment one call raises, throwing away
-- results the model already paid for, so `gather` awaits each call in its own
-- `try` instead. Awaiting one at a time is still concurrent: every call was made
-- before the first await, so they all sit pending and the host dispatches them in
-- one batch. Tasks look like the obvious fix, but monty 0.0.21 fails the whole
-- gather on an external call error rather than raising inside the awaiting task,
-- and a coroutine wrapper is lazy, so a call handed over that way would run alone.
-- Only `RuntimeError` is caught, the shape a failed tool call arrives in; a
-- TypeError from the script itself must still stop the run.
local PREAMBLE = ([[
import re
import asyncio
import sys
import os
import json
async def gather(*calls):
    if len(calls) == 1 and isinstance(calls[0], list):
        calls = calls[0]
    results = []
    for c in calls:
        try:
            results.append(await c)
        except RuntimeError as e:
            results.append('%s' + str(e))
    return results
]]):format(ERROR_PREFIX)
local TOOLS_HEADER = "\n\nAvailable tools (async Python functions, keyword args only):\n"
local WORKFLOW_TOOLS_NOTE =
  "\nWorkflow mode: orchestrate subagents from this script. Await every `task(...)` call and use `gather(task(...), task(...))` for parallel fan-out. Pass `output_schema` to task for machine-readable results (a JSON string, parse with `json.loads`).\n"
local WORKFLOW_OFF_NOTE = "\nNot callable: %s\n"
-- MCP names and schemas already sit in the tool array (or the tool_search
-- catalog), so point at those instead of repeating them here.
local MCP_NOTE =
  "\nMCP tools are callable too, with the arguments their definitions declare. Hyphens become underscores (`srv__get-docs` is `srv__get_docs`).\n"
local CALLABLE_TOOLS_ERR = "cannot list callable tools: "
-- `alias` covers hyphens; what is left is a name no substitution can fix, like
-- a leading digit. Binding it would raise a SyntaxError the model cannot act on.
local PY_IDENTIFIER = "^[%a_][%w_]*$"
local PY_TYPES = { string = "str", integer = "int", boolean = "bool", array = "list" }

local opts = maki.api.register_options(output_limits.extend({
  timeout_secs = {
    default = 30,
    min = 5,
    desc = "Script execution time budget in seconds; waiting on tool calls does not count. A call's `timeout` param overrides it.",
  },
  max_memory_mb = { default = 50, min = 10, desc = "Memory limit for the Python sandbox (MB)." },
}))

local description = [[Execute a Python subset (Monty) in a sandbox where every tool is an async function.

Use for chained/dependent tool calls and filtering results before they enter context.

- Tools are globals: `result = await read(path='file.txt', offset=1, limit=0)`. No `tools` module. They return strings; parse output yourself.
- Tool results are complete. Only what you print/return is capped.
- Concurrency: `a, b = await gather(read(path='a.py', offset=1, limit=0), grep(pattern='x'))`. Pass calls directly, never wrapped in `async def`.
- Preloaded: re, asyncio, sys, os, json; import others such as pathlib. No network. `open()` works on text files, and `os`/`pathlib` can stat, list, walk, glob and mkdir. No `sys.exit` or `asyncio.create_task`.
- Use `bash` for shell commands or full Python.
- Fresh sandbox each run: no state persists between executions.
- 30s script timeout (`timeout` param); time awaiting tool calls doesn't count.
- Skip it when a single tool call needs no transformation.
- NOT a thinking scratchpad. Reason in your response text.
]]

local schema = {
  type = "object",
  required = { "code" },
  additionalProperties = false,
  properties = {
    code = {
      type = "string",
      description = "Python code. Tools return strings, not objects, and you MUST await every call: `result = await read(path='/file', offset=1, limit=0)`.",
    },
    timeout = {
      type = "integer",
      description = "Script execution timeout in seconds (default 30)",
    },
  },
}

local examples = {
  {
    code = [[files = (await glob(pattern='**/*.rs')).strip().split('\n')
results = await gather(*[read(path=f, offset=1, limit=0) for f in files if f.strip()])
for f, c in zip(files, results):
    if 'fn main' in c: print(f)]],
  },
  {
    code = [[result = await grep(pattern='TODO', include='*.rs')
print(f"{len(result.strip().splitlines())} TODOs found")]],
  },
  {
    code = [[content = await webfetch(url='https://example.com/docs')
for line in content.splitlines():
    if 'auth' in line.lower(): print(line)]],
  },
}

-- Shared predicate for describe and handler so advertised == callable.
-- The interpreter is a calling convention, not a capability grant: a read-only
-- subagent must not reach edit/write through Python.
local function interpreter_tools(tools, audience, workflow)
  local out = {}
  for _, t in ipairs(tools) do
    local aud = {}
    for _, a in ipairs(t.audiences) do
      aud[a] = true
    end
    if aud[audience] and (aud.interpreter or (workflow and aud.workflow)) then
      t.workflow_only = not aud.interpreter
      out[#out + 1] = t
    end
  end
  return out
end

local function matches_filter(name, dctx)
  if dctx.only then
    for _, n in ipairs(dctx.only) do
      if n == name then
        return true
      end
    end
    return false
  end
  if dctx.except then
    for _, n in ipairs(dctx.except) do
      if n == name then
        return false
      end
    end
  end
  return true
end

local function signature(t)
  local schema_props = (t.schema and t.schema.properties) or {}
  local required = {}
  for _, r in ipairs((t.schema and t.schema.required) or {}) do
    required[r] = true
  end
  local names = {}
  for pname in pairs(schema_props) do
    names[#names + 1] = pname
  end
  table.sort(names, function(a, b)
    local ra, rb = required[a] or false, required[b] or false
    if ra ~= rb then
      return ra
    end
    return a < b
  end)
  local params = {}
  for _, pname in ipairs(names) do
    local ptype = PY_TYPES[schema_props[pname].type] or "any"
    params[#params + 1] = required[pname] and (pname .. ": " .. ptype) or (pname .. ": " .. ptype .. " = None")
  end
  return "- " .. t.name .. "(" .. table.concat(params, ", ") .. ") -> str"
end

-- Keep cheap: runs on every request build. get_tools skips descriptions
-- to avoid recursion from describe callbacks.
local function describe(dctx)
  local parts = { description, TOOLS_HEADER }
  local has_workflow_only, gated = false, {}
  -- Ask as if workflow were on, then hold back what it would unlock: those
  -- names are listed as not callable, so the model stops trying them.
  for _, t in ipairs(interpreter_tools(maki.api.get_tools(), dctx.audience, true)) do
    if matches_filter(t.name, dctx) then
      if t.workflow_only and not dctx.workflow then
        gated[#gated + 1] = t.name
      else
        has_workflow_only = has_workflow_only or t.workflow_only
        parts[#parts + 1] = signature(t) .. "\n"
      end
    end
  end
  if has_workflow_only then
    parts[#parts + 1] = WORKFLOW_TOOLS_NOTE
  end
  if #gated > 0 then
    parts[#parts + 1] = WORKFLOW_OFF_NOTE:format(table.concat(gated, ", "))
  end
  if dctx.mcp then
    parts[#parts + 1] = MCP_NOTE
  end
  return table.concat(parts)
end

local function hooked(slot_names)
  local slots = maki.api.get_slots()
  for _, name in ipairs(slot_names) do
    if slots[name] and #slots[name].fillers > 0 then
      return true
    end
  end
  return false
end

-- `open()` rides on the read and write tools, so hooks, plugins that replace
-- them, permission prompts, the file lock and plan mode all still apply. The
-- read tool numbers and caps lines, so we ask it for one line just to get its
-- yes and record the read, then take the real content straight from disk.
-- A read hook may have pointed that check at another file, so while one is
-- installed we refuse rather than read the path it steered away from.
-- A replacement write tool without `append` would drop the flag and overwrite
-- the file, so appends are refused there.
-- Metadata and listings follow the list tool the same way, minus the call,
-- since it never prompts and a walk would make one call per directory. mkdir
-- has no tool of its own, so it only goes where the write tool would go
-- without asking. The sandbox looks before it creates, so mkdir also needs
-- `inspect`.
local function file_access(tools, schemas, ctx)
  local files = {}
  if tools.read then
    files.read = function(path)
      if hooked(READ_INPUT_SLOTS) then
        return nil, OPEN_READ_HOOKED_ERR
      end
      local _, err = tools.read({ path = path, offset = 1, limit = OPEN_READ_CHECK_LINES })
      if err then
        return nil, err
      end
      return maki.fs.read(maki.fs.abspath(path))
    end
  end
  if tools.write then
    local can_append = (schemas.write.properties or {}).append ~= nil
    files.write = function(path, content, append)
      if append and not can_append then
        return nil, OPEN_APPEND_UNSUPPORTED_ERR
      end
      return tools.write({ path = path, content = content, append = append })
    end
    files.mkdir = function(path, parents)
      local _, err = ctx:can_write(path)
      if err then
        return nil, err .. MKDIR_BASH_HINT
      end
      return maki.fs.mkdir(path, { parents = parents })
    end
  end
  if tools.list then
    files.inspect = function()
      if hooked(LIST_INPUT_SLOTS) then
        return nil, INSPECT_HOOKED_ERR
      end
      return true
    end
  end
  return files
end

local function handler(input, ctx)
  local started_at = os.time()
  local timeout = input.timeout or opts.timeout_secs

  local max_lines, max_bytes = output_limits.resolve(opts, ctx)
  local state = { text = "" }
  local display = output_limits.live_collector(ctx, state)
  ctx:set_state(state)
  local collected = output_limits.collector(max_lines, max_bytes)
  local function show(line)
    collected:append(line)
    display(line)
  end

  -- Memoized, because a cancel reaches us twice: once through the hook and
  -- again as the interpreter's error.
  local cut_reply
  local function cut(reason)
    local output = collected:result()
    cut_reply = cut_reply or partial.reply(output, partial.marker(output, reason, timeout, started_at))
    cut_reply.state = state
    return cut_reply
  end

  -- Only for a handler still parked when the host gives up on it: normally
  -- the interpreter sees the cancel and we return the partial reply below.
  maki.async.on_cancel(function(reason)
    ctx:finish(cut(reason))
  end)

  -- Registry, MCP and host tools in one list, already filtered by
  -- `disabled_tools`, each entry carrying the audience of the tool a call to
  -- that name would really reach.
  local callable, callable_err = maki.agent.callable_tools(ctx)
  if callable_err then
    return { llm_output = CALLABLE_TOOLS_ERR .. callable_err, is_error = true }
  end

  local tools, schemas = {}, {}
  for _, t in ipairs(interpreter_tools(callable, ctx:audience(), ctx:workflow())) do
    local bind, name = t.alias or t.name, t.name
    if bind:match(PY_IDENTIFIER) then
      schemas[bind] = t.schema or {}
      tools[bind] = function(tool_input)
        if t.workflow_only then
          return maki.agent.call_tool(ctx, name, tool_input)
        end
        -- The script clock stops while a tool call is awaited, so an explicit
        -- longer timeout on the call has to win over the script budget.
        local explicit = type(tool_input) == "table" and tonumber(tool_input.timeout) or nil
        local deadline = math.max(timeout, explicit or 0)
        return maki.agent.call_tool(ctx, name, tool_input, { timeout = deadline })
      end
    end
  end

  local result, err = maki.interpreter.run(input.code, {
    timeout = timeout,
    max_memory_mb = opts.max_memory_mb,
    preamble = PREAMBLE,
    on_output = show,
    tools = tools,
    files = file_access(tools, schemas, ctx),
  })

  if err then
    if err == CANCELLED_ERR then
      return cut("cancelled")
    end
    if err:find(TIME_LIMIT_SUBSTR, 1, true) then
      return cut("timeout")
    end
    -- The run is already paid for, so point a script that reached for
    -- `asyncio.gather` at the wrapper that would have kept its other results.
    local hint = input.code:find(ASYNCIO_GATHER, 1, true) and GATHER_HINT or ""
    return { llm_output = output_limits.cut_middle(err .. hint, max_lines, max_bytes), is_error = true, state = state }
  end

  if result.output then
    show("return: " .. result.output)
  end

  -- Every line already streamed through `show`, so the collector holds the
  -- reply: a finished run is cut like a partial one, keeping its tail, and
  -- nothing walks the whole output again. Full mode promises all of it, so
  -- a cut there is an error.
  local over_full_limit = not max_bytes and collected:truncated()
  local llm_output = collected:result()
  if llm_output == "" then
    llm_output = NO_OUTPUT
  elseif over_full_limit then
    llm_output = llm_output .. "\n" .. output_limits.FULL_LIMIT_NOTE
  end
  return { llm_output = llm_output, is_error = over_full_limit, state = state }
end

maki.api.register_tool({
  name = "code_execution",
  description = description,
  describe = describe,
  schema = schema,
  examples = examples,
  kind = "execute",
  audiences = { "main", "research_sub", "general_sub" },
  start_annotation = { field = "timeout", kind = "timeout" },
  handler = handler,
  summary = function(input)
    return (select(2, input.code:gsub("\n", "\n")) + 1) .. " lines"
  end,
})

maki.api.register_prompt_hint({
  slot = "efficient_tools",
  content = "code_execution",
})
