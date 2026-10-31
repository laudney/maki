require("picker")

maki.ui.register_renderer("task", {
  body = function(data)
    if data.status == "running" and type(data.state) == "table" and data.state.waiting then
      local buf = maki.ui.buf()
      buf:line({ { "waiting for a free subagent slot", "dim" } })
      return buf
    end
    return nil
  end,
})

maki.ui.register_renderer("task_stop", {
  header = function(data)
    return type(data.state) == "table" and data.state.name or data.input.id
  end,
})
