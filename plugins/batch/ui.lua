local ToolView = require("maki.tool_view")

local INDICATOR = {
  pending = { "○ ", "dim" },
  running = { "· ", "spinner" },
  success = { "● ", "tool_success" },
  done = { "● ", "tool_success" },
  error = { "● ", "tool_error" },
  cancelled = { "● ", "tool_error" },
}

local function child_ctx(ctx)
  return {
    cwd = ctx.cwd,
    home = ctx.home,
    tool_output_lines = ctx.tool_output_lines,
    width = math.max(ctx.width - 2, 1),
    reason = ctx.reason,
  }
end

maki.ui.register_renderer("batch", {
  body = function(data, ctx)
    local state = data.state
    if not state or not state.children then
      return nil
    end
    local indexed = {}
    for _, child in ipairs(data.children) do
      indexed[child.index] = child
    end
    local buf = maki.ui.buf()
    local ranges = {}
    local rctx = child_ctx(ctx)
    local line_count = 0
    for i, saved in ipairs(state.children) do
      local input = (data.input.tool_calls or {})[i] or {}
      local child = indexed[saved.index]
        or {
          tool = saved.tool,
          version = 2,
          summary = saved.tool,
          input = input.parameters or input,
          children = {},
          status = saved.status,
          output = saved.output and { Plain = { text = saved.output } },
        }
      if saved.status == "error" and child.status == "running" then
        child.status = "error"
      end
      if not child.output and saved.output then
        child.output = { Plain = { text = saved.output } }
      end
      local draw = maki.ui.renderer(child.tool)
      local header = draw.header(child, rctx)
      local indicator = INDICATOR[child.status] or INDICATOR.error
      local line = { indicator, { child.tool .. "> ", "tool_prefix" } }
      if type(header) == "userdata" then
        for _, span in ipairs(header:get_lines()[1] or {}) do
          line[#line + 1] = span
        end
      else
        line[#line + 1] = { header or child.summary or child.tool, "tool" }
      end
      for _, annotation in ipairs(child.annotations or {}) do
        line[#line + 1] = { " (" .. annotation .. ")", "tool_annotation" }
      end
      if child.usage then
        line[#line + 1] = { "  " .. child.usage, "dim" }
      end
      local first = line_count + 1
      buf:line(line)
      line_count = line_count + 1
      local body = draw.body(child, rctx) or ToolView.draw_output(child, rctx)
      for _, row in ipairs(body and body:get_lines() or {}) do
        local indented = { { "  " } }
        for _, span in ipairs(row) do
          indented[#indented + 1] = span
        end
        buf:line(indented)
        line_count = line_count + 1
      end
      ranges[#ranges + 1] = { first = first, last = line_count, child = child }
    end
    data.ranges = ranges
    return buf
  end,
  click = function(data, ctx, row)
    local rctx = child_ctx(ctx)
    for _, range in ipairs(data.ranges or {}) do
      if row == 0 or (row >= range.first and row <= range.last) then
        local child = range.child
        if maki.ui.has_renderer(child.tool) and not child.rewritten then
          maki.ui.renderer(child.tool).click(child, rctx, row == 0 and 0 or row - range.first)
        else
          child.expanded = not child.expanded
        end
        if row ~= 0 then
          break
        end
      end
    end
  end,
})
