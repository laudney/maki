local State = require("todo_state")

local DESCRIPTION = [[Create or update a structured todo list to track tasks.

**Use after EACH completed step!**

- Send the complete list each time (replace-all semantics).
- Use ONLY for multi-step work (3+ steps).
- Skip for trivial tasks.]]

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Use todo_write for multi-step tasks (3+ steps); update **after EACH step** (done + next in_progress), never batched at the end.",
})

maki.api.register_tool({
  name = "todo_write",
  description = DESCRIPTION,
  schema = {
    type = "object",
    required = { "todos" },
    properties = {
      todos = {
        type = "array",
        description = "The updated todo list",
        items = {
          type = "object",
          required = { "content", "status" },
          properties = {
            content = { type = "string", description = "Task description" },
            status = {
              type = "string",
              enum = { "pending", "in_progress", "completed", "cancelled" },
            },
            priority = {
              type = "string",
              enum = { "high", "medium", "low" },
            },
          },
        },
      },
    },
  },
  audiences = { "main", "research_sub", "general_sub" },

  summary = function(input)
    return string.format("%d todos", #(input.todos or {}))
  end,

  -- Without a session there is no tab to put a panel in.
  handler = function(input, ctx)
    local items = input.todos or {}
    local sid = ctx:session_id()
    if sid and State.on_write then
      State.on_write(sid, ctx:task_id(), items)
    end
    if #items == 0 then
      return "Todos cleared"
    end
    return ""
  end,
})
