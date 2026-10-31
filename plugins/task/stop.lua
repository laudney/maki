-- task_stop lives next to the /tasks picker: both act on tasks the user can
-- see in the task tab, and the host's maki.task.stop does the work.

local DESCRIPTION = "Stop a background task by id."
local ID_DESCRIPTION = "Task id from its receipt"
-- What the model reads when the final reply is not in yet.
local STOP_REQUESTED_REPLY = "Stop requested. Its output arrives later and will not wake you, so do not wait for it."

maki.api.register_tool({
  name = "task_stop",
  description = DESCRIPTION,
  kind = "execute",
  audiences = { "main" },
  needs_background = true,
  schema = {
    type = "object",
    required = { "id" },
    properties = {
      id = { type = "string", description = ID_DESCRIPTION },
    },
  },
  summary = function(input)
    return input.id
  end,
  handler = function(input, ctx)
    local stopped, err = maki.task.stop(ctx, input.id)
    if not stopped then
      return { llm_output = err, is_error = true }
    end
    return {
      llm_output = stopped.output or STOP_REQUESTED_REPLY,
      state = { name = stopped.name },
    }
  end,
})
