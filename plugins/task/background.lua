-- A background task lives in `entries` from the moment its session starts
-- opening. Once open it gets a session and a worker task, and once the worker
-- gets a concurrency slot it also holds a permit. It leaves exactly once:
-- through `finish` with one notice, or without a notice when its chat ends.
--
-- A user stop reaches a running task as an error from its own prompt, so it
-- keeps its partial output. A queued task has no prompt to fail, so the
-- TaskStopped event finishes it instead. The session owns its notice, so one
-- that outlives its chat generation is dropped instead of delivered.
--
-- A blocking task with a chat runs on the same worker, which also opens its
-- session, with a call waiting on its result. Moving the call to the
-- background only changes who gets that result: from then on it is an
-- ordinary background task with a notice.

local Runner = require("runner")

local NOT_LIVE_PREFIX = "session not live: "
local CANCELLED = "background task cancelled"
local STOPPED = { llm_output = "sub-agent error: stopped by user", is_error = true }
local LINE = "Background agent %s: %s"
local MOVED = "background"
local ESC = "cancelled"
local MOVED_TASK_RECEIPT =
  "The user moved this task to the background. Its result arrives as a notification; do not wait for it. Continue with other work, or end your turn."

local entries = {}
local Background = {}

local function release(entry)
  if entry.permit then
    entry.permit:release()
    entry.permit = nil
  end
end

-- The host drops a plugin session from its list when the user stops it, and
-- cancels the child of a blocking task.
local function stopped(entry)
  if entry.child then
    return entry.session:cancelled()
  end
  local id = entry.session:id()
  for _, session in ipairs(maki.agent.list({ session = entry.chat }) or {}) do
    if session.id == id then
      return false
    end
  end
  return true
end

local function notify(entry, result, wake)
  local status = result.is_error and "failed" or "finished"
  -- Only a user stop keeps the chat asleep, so a quiet error is a cancel.
  local verb = result.is_error and not wake and "cancelled" or status
  local _, err = maki.session.notify(
    "Background task " .. entry.name .. " " .. status .. ":\n\n" .. result.llm_output,
    { session = entry.chat, wake = wake, owner = entry.session, display = LINE:format(verb, entry.name) }
  )
  if err then
    maki.log.warn("background task notification failed: " .. err)
  end
end

-- A blocking task that has not moved still has a call waiting for its result.
local function finish(entry, result, wake)
  if not entries[entry] then
    return
  end
  entries[entry] = nil
  if entry.reply then
    entry.reply(result)
  else
    notify(entry, result, wake)
  end
end

local function work(entry, prepared)
  -- Hooks must not yield, so closing the session is left to whoever cancels us.
  maki.async.on_cancel(function()
    release(entry)
  end)
  entry.permit = Runner.semaphore:acquire()
  local ran, result = pcall(Runner.run, entry.session, prepared)
  if not ran then
    result = { llm_output = tostring(result), is_error = true }
  end
  -- A task the user stopped still reports back, it just does not wake the chat.
  local wake = not stopped(entry)
  -- Keep the permit until the session is closed, so TaskStopped never takes
  -- this task for a queued one. A child outlives its call, so the call cannot
  -- carry its verdict.
  entry.session:close(entry.child and (result.is_error and "error" or "done") or nil)
  release(entry)
  finish(entry, result, wake)
end

-- Returns nil when there is no live chat to deliver to, and the caller runs
-- the task blocking instead.
function Background.start(prepared, ctx)
  local chat = ctx:session_id()
  if not chat then
    return nil
  end
  local entry = { chat = chat, name = prepared.options.name }
  entries[entry] = true
  -- Once the session is open the task no longer belongs to this tool call.
  maki.async.on_cancel(function()
    if not entry.session then
      entries[entry] = nil
    end
  end)
  -- The blocking fallback opens with a ctx, which ignores these two.
  prepared.options.session, prepared.options.scope = chat, "session"
  local session, err = maki.agent.session(prepared.options)
  if err then
    entries[entry] = nil
    if err == NOT_LIVE_PREFIX .. chat then
      return nil
    end
    return { llm_output = err, is_error = true }
  end
  -- Opening yields, and the tool call or the chat may have ended meanwhile.
  if not entries[entry] then
    session:close()
    return { llm_output = CANCELLED, is_error = true }
  end
  entry.session = session
  entry.task = maki.async.spawn(function()
    work(entry, prepared)
  end)
  return {
    llm_output = "Background task started: " .. entry.name .. ". The result will be delivered when it finishes.",
  }
end

-- Returns the result for the waiting call, or nothing once its cancel hook has
-- answered for it.
function Background.blocking(prepared, ctx)
  local entry = { chat = ctx:session_id(), name = prepared.options.name, child = true }
  local parked
  local function resume()
    if parked then
      local wake = parked
      parked = nil
      wake()
    end
  end
  function entry.reply(result)
    entry.result = result
    resume()
  end
  entries[entry] = true
  maki.async.on_cancel(function(reason)
    -- Without an entry the result is already in hand, and the call returns it.
    if entries[entry] then
      if reason == MOVED then
        entry.reply = nil
        ctx:finish(MOVED_TASK_RECEIPT)
      elseif reason == ESC and entry.permit then
        -- The host still waits for the call, so the worker hands it the
        -- prompt's error with whatever the child streamed. A worker still
        -- queued for a slot has run nothing, so its call ends below.
        entry.session:cancel()
        return
      else
        entries[entry] = nil
        -- A running worker closes the session once its prompt fails. A queued
        -- one has no prompt to fail, so it ends here and the cancel closes the
        -- session for it.
        if entry.session then
          if not entry.permit then
            entry.task:cancel()
          end
          entry.session:cancel()
        end
        ctx:finish({ llm_output = reason, is_error = true })
      end
    end
    resume()
  end)
  -- A call already cancelled runs its hook at once.
  if not entries[entry] then
    return
  end
  entry.task = maki.async.spawn(function()
    local options = table.clone(prepared.options)
    options.background, options.presentation, options.prompt = true, "attached", prepared.message
    local session, err = maki.agent.session(ctx, options)
    if err then
      finish(entry, { llm_output = err, is_error = true }, true)
      return
    end
    -- Opening yields, and the call or the chat may have ended meanwhile.
    if not entries[entry] then
      session:close()
      return
    end
    entry.session = session
    work(entry, prepared)
  end)
  if entry.reply and not entry.result then
    maki.async.await(1, function(wake)
      parked = wake
    end)
  end
  return entry.result
end

maki.api.create_autocmd("TaskStopped", {
  callback = function(ev)
    for entry in pairs(entries) do
      if entry.session and entry.session:id() == ev.data.id then
        -- Any plugin can fire TaskStopped, so the host's list has the final word.
        if not entry.permit and stopped(entry) then
          finish(entry, STOPPED, false)
          entry.task:cancel()
          entry.session:close()
        end
        return
      end
    end
  end,
})

maki.api.create_autocmd("SessionEnd", {
  callback = function(ev)
    -- Cancel every worker before the first close yields, or a slot freed by
    -- one could start a sibling that is about to be stopped.
    local closing = {}
    for entry in pairs(entries) do
      if entry.chat == ev.data.session_id then
        entries[entry] = nil
        if entry.session then
          entry.task:cancel()
          closing[#closing + 1] = entry.session
        end
      end
    end
    for _, session in ipairs(closing) do
      session:close()
    end
  end,
})

return Background
