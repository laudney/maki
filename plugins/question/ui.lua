maki.ui.register_renderer("question", {
  body = function(data)
    if data.status == "running" then
      return nil
    end
    local buf = maki.ui.buf()
    for i, question in ipairs(data.input.questions or {}) do
      for _, line in ipairs(maki.split("Q" .. i .. ". " .. question.question, "\n")) do
        buf:line({ { line, "bold" } })
      end
    end
    local output = data.output or {}
    if output.Plain then
      for _, line in ipairs(maki.split(output.Plain.text, "\n")) do
        buf:line(line)
      end
    end
    return buf
  end,
})
