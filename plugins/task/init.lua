-- The UI component loads the /tasks picker; this component owns its work.

require("stop")

local Runner = require("runner")
-- Steering is for parents that chose background delegation, the rest should
-- not pay for the tool's definition.
local Message = Runner.allow_background and require("message")

local MOVED_RECEIPT = "Subagent %s"
local FOREGROUND_NOTE = "Could not start in the background (%s). Ran in the foreground."

local description = [[Launch an autonomous subagent to perform tasks independently. Best combined with batch.

Subagent types (set via `subagent_type`):
- `research` (default): Read-only tools. For codebase exploration or gathering context.
- `general`: Full tool access. For delegating implementation work.

Notes:
1. Launch multiple tasks concurrently when possible.
2. The agent's result is not visible to the user. Summarize it in your response.
3. Each invocation starts fresh - inline any needed context into the prompt.
4. Tell it to return concise summaries with file:line refs, not full file contents.
]]

local examples = {
  {
    description = "Find auth middleware",
    prompt = "Search the codebase for authentication middleware. Return file paths and a summary of how auth is implemented.",
    model_tier = "weak",
  },
}

local function handler(input, ctx)
  local prepared, prepare_err = Runner.prepare(input, ctx)
  if prepare_err then
    return { llm_output = prepare_err, is_error = true }
  end

  local call_id, chat = ctx:tool_use_id(), ctx:session_id()
  -- Declared out here so the epilogue closes it on every path: left to the
  -- garbage collector it keeps the subagent's event relay alive on an idle VM.
  local sess
  local moved
  local function on_background()
    moved = Message and Message.track(call_id, chat, prepared.options.name)
    if moved and sess then
      Message.attach(moved, sess)
    end
    return string.format(MOVED_RECEIPT, prepared.options.name)
  end

  -- A background task moves before it waits for a slot, so a full house never
  -- holds the model's turn. Once moved it still queues for a slot like any
  -- other task. A blocking task only gets its hook once it holds a slot.
  -- Moving fails where background work is off (a zero --background-timeout),
  -- and then the task simply runs blocking.
  local foreground_note
  local run_in_background = Runner.allow_background and input.background == true
  if run_in_background then
    ctx:on_background(on_background)
    local moved, move_err = ctx:background()
    if not moved then
      foreground_note = string.format(FOREGROUND_NOTE, tostring(move_err))
    end
  end

  -- Shown only while every slot is taken, moved subagents included.
  local permit = Runner.acquire_slot(function()
    ctx:set_state({ waiting = true })
  end)
  ctx:set_state({ waiting = false })

  -- pcall so a raised error cannot leak the permit or the session.
  local ok, out = pcall(function()
    local sess_err
    sess, sess_err = maki.agent.session(ctx, prepared.options)
    if sess_err then
      return { llm_output = sess_err, is_error = true }
    end
    if moved then
      Message.attach(moved, sess)
    end
    if not run_in_background then
      ctx:on_background(on_background)
    end
    return Runner.run(sess, prepared)
  end)

  if moved then
    Message.untrack(call_id)
  end
  if sess then
    sess:close()
  end
  permit:release()
  if not ok then
    error(out, 0)
  end
  if foreground_note then
    out.llm_output = foreground_note .. "\n\n" .. out.llm_output
  end
  return out
end

maki.api.register_tool({
  name = "task",
  description = description,
  kind = "execute",
  audiences = { "main", "workflow" },
  examples = examples,
  schema = Runner.schema,
  handler = handler,
  summary = function(input)
    return input.description
  end,
})
