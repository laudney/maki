local NO_ANSWER = "(no answer)"

local QuestionHelpers = {}

-- Shared by the tool and by a client that opens the form for a remote ask.
function QuestionHelpers.normalize(questions)
  questions = questions or {}
  for _, q in ipairs(questions) do
    q.options = q.options or {}
    q.header = q.header or ""
    q.multiple = q.multiSelect or false
  end
  return questions
end

-- The questions sit in the tool input right above this result, and inputs
-- outlive outputs through compaction, so only the picked labels are sent.
function QuestionHelpers.format_answers(questions, answers)
  local lines = {}
  for i, q in ipairs(questions) do
    local ans = answers[i] or {}
    local label = (q.header and q.header ~= "") and q.header or ("Q" .. i)
    lines[#lines + 1] = label .. ": " .. (#ans > 0 and table.concat(ans, ", ") or NO_ANSWER)
  end
  return table.concat(lines, "\n")
end

return QuestionHelpers
