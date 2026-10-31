//! Tests the batch plugin's policy end-to-end: real plugin source, real
//! `maki.async.gather`, with tool dispatch replaced by a scriptable Lua stub.

mod common;

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::{Chat, call_ctx, exec, next_task_done, poll_until, request_move};

use maki_agent::ToolOutput;
use maki_agent::cancel::{CancelToken, CancelTrigger};
use maki_agent::tools::ProgressScope;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{ToolContext, ToolRegistry};
use maki_agent::{
    AgentEvent, AgentMode, AgentSeed, Envelope, PartStatus, SessionMailbox, ToolDoneEvent,
};
use maki_lua::test_support::runtime;
use maki_lua::{KILL_GRACE, MoveOutcome, PluginHost, PluginPermissions};

use serde_json::{Value, json};

const BATCH_PLUGIN_SRC: &str = include_str!("../../plugins/batch/init.lua");

// Mirrors of the plugin's format contracts.
const MAX_BATCH_SIZE: usize = 25;
const ERROR_PREFIX: &str = "[ERROR] ";
const EMPTY_ERROR: &str = "provide at least one tool call";
const NESTED_ERROR: &str = "cannot nest batch inside batch";
const DISCARDED_ERROR: &str = "maximum of 25 tools per batch";
const CANCELLED_ERROR: &str = "cancelled";
const SUMMARY_ALL_OK_FMT: &str = "All {} tools executed successfully.";
const SUMMARY_MIXED_FMT: &str = "Executed {}/{} successfully. {} failed.";

const BATCH_TOOL: &str = "batch";
const BATCH_ID: &str = "toolu_batch";
const PROBE_TOOL: &str = "probe";
const OK_TOOL: &str = "ok";
const PARKJOB_TOOL: &str = "parkjob";
const PARKJOB_SECOND_TOOL: &str = "parkjob_second";
const BOOM_TOOL: &str = "boom";
const BOOM_ERR: &str = "stub tool exploded";
const PARTIAL_TOOL: &str = "partial";
const PARTIAL_ERR: &str = "half the output [cancelled by user after 3s, output above is partial]";
/// Wildly generous vs the expected kill (one poll plus one
/// [`KILL_GRACE`]); only a watchdog that never fires gets here.
const RUNAWAY_BUDGET_GRACES: u32 = 20;
/// How long a `parkjob` child sits in an await that ignores the token, like a
/// child tool mid-request. It only has to outlast the cancel that follows the
/// first update, and stay inside the host's abandon window.
const PARK_SECS: f32 = 2.0;
const PAINT_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Deliberately generous: no assertion here is a race against the clock,
/// so a slow machine costs time only when a test is already failing.
const PAINT_TIMEOUT: Duration = Duration::from_secs(5);
/// Only a batch that never settles waits this long.
const BATCH_RESULT_TIMEOUT: Duration = Duration::from_secs(10);
const EDIT_TOOL: &str = "edit";
const EDIT_TARGET: &str = "f.rs";
const STATUS_SUCCESS: &str = "success";
const STATUS_ERROR: &str = "error";
const MARKER_A: &str = "AAA";
const MARKER_B: &str = "BBB";
const EDITED_A: &str = "aaa";
const EDITED_B: &str = "bbb";
/// Whichever of the two edits lands first erases the other's `old_string`.
const OVERLAP_SOURCE: &str = "XYZ";
const OVERLAP_HEAD: &str = "XY";
const OVERLAP_HEAD_NEW: &str = "Q";
const OVERLAP_TAIL: &str = "YZ";
const OVERLAP_TAIL_NEW: &str = "R";

const HOOKED_TOOL: &str = "hooked";
const RELEASE_CHILD_TOOL: &str = "release_child";
const IS_PARKED_TOOL: &str = "is_parked";
const IS_PARKED_TEXT: &str = "true";
const MOVED_HEADER: &str = "Still running in the background:";
const MOVED_BY_USER: &str = "Moved to the background by the user";
const MOVED_AS_ASKED: &str = "Started in the background as asked";
const FOREGROUND_NOTE_HEAD: &str = "Could not start in the background (";
const FOREGROUND_NOTE_TAIL: &str = "). Ran in the foreground.\n\n";
const WAITING_ON_EDIT: &str = "batch is waiting on edit";

/// `maki.agent.call_tool` is stubbed; `maki.async.gather` and the semaphore
/// stay real, so the park/release pair proves children genuinely overlap.
const STUB_PRELUDE: &str = r#"
recorder = { calls = {} }
local sem = maki.async.semaphore(1)
local held = sem:acquire()

maki.agent.call_tool = function(ctx, name, input, opts)
  recorder.calls[#recorder.calls + 1] = { tool = name, params = input }
  if name == "ok" then
    return "ok:" .. tostring(input.tag or "?")
  elseif name == "park" then
    -- Deadlocks unless a sibling runs concurrently and releases.
    local p = sem:acquire()
    p:release()
    return "parked_done"
  elseif name == "release" then
    held:release()
    return "released_done"
  elseif name == "boom" then
    return nil, "@BOOM_ERR@"
  elseif name:find("^parkjob") then
    -- Parks in an await that ignores the token, like a real child tool
    -- waiting on a request. Any `parkjob*` name parks, so a test can give
    -- one of them its own restore.
    maki.fn.jobwait(maki.fn.jobstart("sleep @PARK_SECS@"))
    return "parkjob_done"
  elseif name == "partial" then
    -- Parks past the sweep, then comes back with more than "cancelled".
    maki.fn.jobwait(maki.fn.jobstart("sleep @PARK_SECS@"))
    return nil, "@PARTIAL_ERR@"
  elseif name == "spin" then
    -- Never yields, so nothing but the watchdog can end it.
    while true do end
  end
  return nil, "unknown tool: " .. name
end

maki.api.register_tool({
  name = "probe",
  description = "recorder snapshot",
  schema = { type = "object", properties = {}, additionalProperties = false },
  audiences = { "main" },
  handler = function(input, ctx)
    return (maki.json.encode(recorder))
  end,
})

"#;

fn load_batch_host() -> (Arc<ToolRegistry>, PluginHost) {
    load_batch_host_with("")
}

/// `child_src` registers extra child tools next to the stub ones, so a
/// test can give a child its own header/restore behaviour.
fn load_batch_host_with(child_src: &str) -> (Arc<ToolRegistry>, PluginHost) {
    load_batch_host_as(child_src, PluginPermissions::trusted())
}

fn load_batch_host_as(
    child_src: &str,
    permissions: PluginPermissions,
) -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let prelude = STUB_PRELUDE
        .replace("@BOOM_ERR@", BOOM_ERR)
        .replace("@PARTIAL_ERR@", PARTIAL_ERR)
        .replace("@PARK_SECS@", &PARK_SECS.to_string());
    host.load_source_with_permissions(
        "batch_policy",
        &format!("{prelude}\n{child_src}\n{BATCH_PLUGIN_SRC}"),
        permissions,
    )
    .unwrap();
    (reg, host)
}

fn output_text(out: ToolOutput) -> String {
    match out {
        ToolOutput::Plain(s) | ToolOutput::Markdown(s) => s.text,
        other => panic!("unexpected output: {other:?}"),
    }
}

fn exec_with_ctx(
    reg: &ToolRegistry,
    name: &str,
    input: Value,
    ctx: &ToolContext,
) -> Result<ToolOutput, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    runtime().block_on(async { inv.execute(ctx).await }).output
}

fn exec_tool(reg: &ToolRegistry, name: &str, input: Value) -> Result<String, String> {
    exec_with_ctx(reg, name, input, &stub_ctx(&AgentMode::Build)).map(output_text)
}

fn batch_input(tool_calls: Value) -> Value {
    json!({ "tool_calls": tool_calls })
}

/// Same as [`run_batch`], but the run's token is tripped before a single
/// child starts, standing in for esc pressed while the batch is in flight.
fn run_cancelled_batch(reg: &ToolRegistry, tool_calls: Value) -> Result<String, String> {
    let mut ctx = stub_ctx(&AgentMode::Build);
    let (trigger, token) = CancelToken::new();
    trigger.cancel();
    ctx.cancel = token;
    exec_with_ctx(reg, BATCH_TOOL, batch_input(tool_calls), &ctx).map(output_text)
}

fn run_batch(reg: &ToolRegistry, tool_calls: Value) -> Result<String, String> {
    exec_tool(reg, BATCH_TOOL, batch_input(tool_calls))
}

fn recorded_calls(reg: &ToolRegistry) -> Vec<Value> {
    let out = exec_tool(reg, PROBE_TOOL, json!({})).expect("probe failed");
    let snap: Value = serde_json::from_str(&out).expect("probe returned invalid json");
    snap["calls"].as_array().cloned().unwrap_or_default()
}

fn section(tool: &str, body: &str) -> String {
    format!("## {tool}\n{body}\n\n")
}

fn summary_all_ok(total: usize) -> String {
    SUMMARY_ALL_OK_FMT.replacen("{}", &total.to_string(), 1)
}

fn summary_mixed(ok: usize, total: usize, failed: usize) -> String {
    SUMMARY_MIXED_FMT
        .replacen("{}", &ok.to_string(), 1)
        .replacen("{}", &total.to_string(), 1)
        .replacen("{}", &failed.to_string(), 1)
}

#[test]
fn all_success_exact_llm_output() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "ok", "parameters": { "tag": "a" } },
            { "tool": "ok", "parameters": { "tag": "b" } },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}",
        section("ok", "ok:a"),
        section("ok", "ok:b"),
        summary_all_ok(2)
    );
    assert_eq!(out, expected);
}

/// The grace is a budget, not a licence to hang: a child that never yields
/// still gets shot, so esc hands the session back instead of wedging it
/// until the outer backstop timeout. The kill lands inside the child, so
/// the batch survives and reports it cancelled like any other child.
#[test]
fn runaway_child_under_cancel_still_terminates() {
    let batch = start_batch(json!([{ "tool": "spin", "parameters": {} }]));
    assert_statuses(&batch.progress, &[("running", 1)]);

    batch.trigger.cancel();

    let outcome = batch
        .result
        .recv_timeout(KILL_GRACE * RUNAWAY_BUDGET_GRACES)
        .expect("a never-yielding child must be killed, not run forever");
    let expected = format!(
        "{}{}",
        section("spin", &format!("{ERROR_PREFIX}{CANCELLED_ERROR}")),
        summary_mixed(0, 1, 1)
    );
    assert_eq!(outcome, Ok(expected));
}

/// A batch running on its own thread, with what a cancel test needs to press
/// esc at a chosen moment: its progress, the trigger, and where its
/// result lands.
struct RunningBatch {
    trigger: maki_agent::cancel::CancelTrigger,
    progress: ProgressScope,
    result: flume::Receiver<Result<String, String>>,
}

fn start_batch(tool_calls: Value) -> RunningBatch {
    let (trigger, token) = CancelToken::new();
    let (result_tx, result) = flume::bounded(1);
    let progress = ProgressScope::default();
    let running = progress.clone();
    std::thread::spawn(move || {
        let (reg, _host) = load_batch_host();
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.cancel = token;
        ctx.progress = Some(running);
        let input = batch_input(tool_calls);
        result_tx
            .send(exec_with_ctx(&reg, BATCH_TOOL, input, &ctx).map(output_text))
            .ok();
    });
    RunningBatch {
        trigger,
        progress,
        result,
    }
}

fn assert_statuses(progress: &ProgressScope, expected: &[(&str, usize)]) {
    let deadline = Instant::now() + PAINT_TIMEOUT;
    let mut state = Value::Null;
    loop {
        for update in progress.take() {
            if update.path.is_empty()
                && let Some(value) = update.state
            {
                state = value;
            }
        }
        if let Some(children) = state["children"].as_array()
            && expected.iter().all(|(status, count)| {
                children
                    .iter()
                    .filter(|child| child["status"] == *status)
                    .count()
                    == *count
            })
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "expected {expected:?}, got {state}"
        );
        std::thread::sleep(PAINT_POLL_INTERVAL);
    }
}

/// Cancellation must settle pending children while keeping finished results.
/// A late child error can still replace the cancellation with partial output.
#[test]
fn cancel_mid_flight_settles_parked_children_and_spares_finished_ones() {
    let batch = start_batch(json!([
        { "tool": OK_TOOL, "parameters": { "tag": "a" } },
        { "tool": PARKJOB_TOOL, "parameters": {} },
        { "tool": PARKJOB_SECOND_TOOL, "parameters": {} },
        { "tool": PARTIAL_TOOL, "parameters": {} },
        { "tool": BOOM_TOOL, "parameters": {} },
    ]));
    assert_statuses(
        &batch.progress,
        &[("success", 1), ("error", 1), ("running", 3)],
    );
    batch.trigger.cancel();
    assert_statuses(
        &batch.progress,
        &[("success", 1), ("error", 4), ("running", 0)],
    );

    let cancelled = format!("{ERROR_PREFIX}{CANCELLED_ERROR}");
    let expected = format!(
        "{}{}{}{}{}{}",
        section(OK_TOOL, "ok:a"),
        section(PARKJOB_TOOL, &cancelled),
        section(PARKJOB_SECOND_TOOL, &cancelled),
        section(PARTIAL_TOOL, &format!("{ERROR_PREFIX}{PARTIAL_ERR}")),
        section(BOOM_TOOL, &format!("{ERROR_PREFIX}{BOOM_ERR}")),
        summary_mixed(1, 5, 4)
    );
    let settled = batch
        .result
        .recv_timeout(BATCH_RESULT_TIMEOUT)
        .expect("batch must settle instead of hanging on its cancelled children");
    assert_eq!(settled, Err(expected));
}

/// Esc pressed before the batch is even dispatched: the handler still runs
/// and `gather` runs every fun it was handed, so only the batch itself can
/// stop a swept child from running anyway. The handler is doomed from its
/// first line, so on a starved machine the watchdog may kill it before the
/// cancel hook publishes anything. That is why we check what holds either
/// way: an error comes back and nothing was dispatched.
#[test]
fn cancelled_batch_dispatches_nothing() {
    let (reg, _host) = load_batch_host();
    run_cancelled_batch(
        &reg,
        json!([{ "tool": OK_TOOL, "parameters": { "tag": "a" } }]),
    )
    .expect_err("a cancelled batch is an error reply");
    assert!(
        recorded_calls(&reg).is_empty(),
        "a batch cancelled before it starts must dispatch nothing"
    );
}

#[test]
fn flat_nested_and_merged_params_normalize_identically() {
    let (reg, _host) = load_batch_host();
    run_batch(
        &reg,
        json!([
            { "tool": "ok", "tag": "flat", "n": 1 },
            { "tool": "ok", "parameters": { "tag": "nested", "n": 1 } },
            { "tool": "ok", "parameters": { "tag": "merged" }, "n": 1 },
        ]),
    )
    .expect("batch failed");

    let mut calls = recorded_calls(&reg);
    assert_eq!(calls.len(), 3);
    calls.sort_by_key(|c| c["params"]["tag"].as_str().unwrap_or("").to_owned());
    assert_eq!(calls[0]["params"], json!({ "tag": "flat", "n": 1 }));
    assert_eq!(calls[1]["params"], json!({ "tag": "merged", "n": 1 }));
    assert_eq!(calls[2]["params"], json!({ "tag": "nested", "n": 1 }));
}

/// The batch plugin strips GPT's `functions.` prefix in Lua so child
/// lookups and the nested-batch guard key on the canonical name.
#[test]
fn functions_prefix_stripped_from_child_tool_names() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "functions.ok", "parameters": { "tag": "a" } },
            { "tool": "functions.batch", "parameters": { "tool_calls": [] } },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}",
        section("ok", "ok:a"),
        section("batch", &format!("{ERROR_PREFIX}{NESTED_ERROR}")),
        summary_mixed(1, 2, 1)
    );
    assert_eq!(out, expected);
    let calls = recorded_calls(&reg);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["tool"], json!("ok"));
}

#[test_case::test_case(json!([]), EMPTY_ERROR ; "empty_list")]
#[test_case::test_case(json!([{ "tool": "ok", "parameters": { "tag": "x" }, "tag": "y" }]), "duplicate parameter 'tag'" ; "duplicate_key")]
fn invalid_input_errors_without_dispatch(tool_calls: Value, expected_err: &str) {
    let (reg, _host) = load_batch_host();
    let err = run_batch(&reg, tool_calls).unwrap_err();
    assert!(err.contains(expected_err), "got: {err}");
    assert!(
        recorded_calls(&reg).is_empty(),
        "nothing must be dispatched"
    );
}

#[test]
fn nested_batch_rejected_without_dispatch() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "batch", "parameters": { "tool_calls": [] } },
            { "tool": "ok", "parameters": { "tag": "a" } },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}",
        section("batch", &format!("{ERROR_PREFIX}{NESTED_ERROR}")),
        section("ok", "ok:a"),
        summary_mixed(1, 2, 1)
    );
    assert_eq!(out, expected);
    let calls = recorded_calls(&reg);
    assert_eq!(calls.len(), 1, "nested batch must not be dispatched");
    assert_eq!(calls[0]["tool"], json!("ok"));
}

#[test]
fn overflow_entries_discarded_with_section() {
    let (reg, _host) = load_batch_host();
    let entries: Vec<Value> = (0..MAX_BATCH_SIZE + 1)
        .map(|i| json!({ "tool": "ok", "parameters": { "tag": i.to_string() } }))
        .collect();
    let out = run_batch(&reg, json!(entries)).expect("batch failed");
    assert!(
        out.contains(&format!("{ERROR_PREFIX}{DISCARDED_ERROR}")),
        "got: {out}"
    );
    assert!(
        out.ends_with(&summary_mixed(MAX_BATCH_SIZE, MAX_BATCH_SIZE + 1, 1)),
        "got: {out}"
    );
    assert_eq!(
        recorded_calls(&reg).len(),
        MAX_BATCH_SIZE,
        "only the first {MAX_BATCH_SIZE} entries may dispatch"
    );
}

/// One exact-string assertion pins the llm output contract: success
/// sections, error sections (child failure and unknown tool), input order,
/// and the mixed summary line.
#[test]
fn mixed_success_and_error_keeps_input_order() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "ok", "parameters": { "tag": "a" } },
            { "tool": "boom", "parameters": {} },
            { "tool": "nope", "parameters": {} },
            { "tool": "ok", "parameters": { "tag": "b" } },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}{}{}",
        section("ok", "ok:a"),
        section("boom", &format!("{ERROR_PREFIX}{BOOM_ERR}")),
        section("nope", &format!("{ERROR_PREFIX}unknown tool: nope")),
        section("ok", "ok:b"),
        summary_mixed(2, 4, 2)
    );
    assert_eq!(out, expected);
}

/// `park` blocks until `release` runs: completion order is release-first,
/// yet sections must come out in input order. Also proves real overlap
/// (serial execution would deadlock this test).
#[test]
fn children_overlap_and_output_keeps_input_order() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "park", "parameters": {} },
            { "tool": "release", "parameters": {} },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}",
        section("park", "parked_done"),
        section("release", "released_done"),
        summary_all_ok(2)
    );
    assert_eq!(out, expected);
}

fn exec_batch_live(_host: &PluginHost, reg: &Arc<ToolRegistry>, tool_calls: Value) -> Value {
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.registry = Arc::clone(reg);
    let input = batch_input(tool_calls);
    let inv = reg.get(BATCH_TOOL).unwrap().tool.parse(&input).unwrap();
    let result = runtime().block_on(inv.execute(&ctx));
    result.output.expect("batch failed");
    result.state.expect("no state on batch call")
}

/// Regression: two `edit` children on one file used to interleave their
/// read-modify-write and silently lose one edit. The dispatcher now holds a
/// per-file lock across each child's whole execution.
#[test]
fn parallel_edits_to_one_file_all_apply() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join(EDIT_TARGET);
    std::fs::write(&path, format!("{MARKER_A}\n{MARKER_B}\n")).unwrap();

    let (reg, host) = real_builtins();
    let state = exec_batch_live(
        &host,
        &reg,
        json!([
            edit_call(&path, MARKER_A, EDITED_A),
            edit_call(&path, MARKER_B, EDITED_B),
        ]),
    );

    assert_eq!(
        child_statuses(&state),
        vec![STATUS_SUCCESS, STATUS_SUCCESS],
        "both edits must apply: {state}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{EDITED_A}\n{EDITED_B}\n")
    );
}

/// Overlapping edits cannot both apply, so the contract is that the loser
/// says so. Which one loses is `async_lock`'s business, not ours.
#[test]
fn overlapping_parallel_edits_fail_loudly() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join(EDIT_TARGET);
    std::fs::write(&path, OVERLAP_SOURCE).unwrap();

    let (reg, host) = real_builtins();
    let state = exec_batch_live(
        &host,
        &reg,
        json!([
            edit_call(&path, OVERLAP_HEAD, OVERLAP_HEAD_NEW),
            edit_call(&path, OVERLAP_TAIL, OVERLAP_TAIL_NEW),
        ]),
    );

    let mut statuses = child_statuses(&state);
    statuses.sort_unstable();
    assert_eq!(
        statuses,
        vec![STATUS_ERROR, STATUS_SUCCESS],
        "exactly one edit may win, and the other must say so: {state}"
    );
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content == OVERLAP_SOURCE.replace(OVERLAP_HEAD, OVERLAP_HEAD_NEW)
            || content == OVERLAP_SOURCE.replace(OVERLAP_TAIL, OVERLAP_TAIL_NEW),
        "one whole edit, never a mix: {content}"
    );
}

fn real_builtins() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::with_all_builtins(Arc::clone(&reg)).unwrap();
    (reg, host)
}

fn edit_call(path: &std::path::Path, old_string: &str, new_string: &str) -> Value {
    json!({ "tool": EDIT_TOOL, "parameters": {
        "path": path.to_str().expect("utf-8 temp path"),
        "old_string": old_string,
        "new_string": new_string,
    } })
}

fn child_statuses(state: &Value) -> Vec<&str> {
    state["children"]
        .as_array()
        .expect("no children in batch state")
        .iter()
        .map(|child| child["status"].as_str().expect("child without status"))
        .collect()
}

/// Children for the move tests. `hooked` reports itself movable (a bash-like
/// child that registered `ctx:on_background`), `edit` never does. Both park
/// until `release_child` frees them, so a test decides who is still running
/// when the press lands.
const MOVABLE_CHILD_SRC: &str = r#"
do
  local stubbed = maki.agent.call_tool
  local parked, released = {}, {}
  local next_index = 0
  local function park(tag)
    if released[tag] then
      released[tag] = nil
      return
    end
    maki.async.await(1, function(cb)
      parked[tag] = cb
    end)
  end
  maki.agent.call_tool = function(ctx, name, input, opts)
    if name == "hooked" or name == "edit" then
      next_index = next_index + 1
      opts.on_progress({
        path = {},
        child = { index = next_index, summary = name == "hooked" and input.tag or name },
      })
      if name == "hooked" then
        opts.on_movable(true)
      end
      if input.subagent then
        opts.on_task("session-" .. input.tag)
      end
      park(input.tag)
      -- A batch hands a background ask over, so it never reaches the child.
      return name .. ":" .. input.tag .. (input.background and "+background" or "")
    end
    return stubbed(ctx, name, input, opts)
  end

  local schema = {
    type = "object",
    properties = {
      tag = { type = "string" },
      background = { type = "boolean", needs_background = true },
    },
  }
  local function tool(name, handler, header)
    maki.api.register_tool({
      name = name,
      description = name,
      schema = schema,
      audiences = { "main" },
      summary = header,
      handler = handler,
    })
  end
  tool("hooked", function()
    return "unused"
  end, function(input)
    return input.tag
  end)
  tool("is_parked", function(input)
    return tostring(parked[input.tag] ~= nil)
  end)
  tool("release_child", function(input)
    local cb = parked[input.tag]
    parked[input.tag] = nil
    if cb then
      cb()
    else
      released[input.tag] = true
    end
    return "released"
  end)
end
"#;

/// A batch on its own thread as dispatch makes it (a call scope under the run
/// token, in a live chat), so `request_background` can really move it.
struct MovableBatch {
    reg: Arc<ToolRegistry>,
    host: PluginHost,
    tasks: flume::Receiver<Envelope>,
    mailbox: SessionMailbox,
    progress: ProgressScope,
    _seed: Arc<AgentSeed>,
    run: Option<CancelTrigger>,
    events: flume::Receiver<Envelope>,
}

impl MovableBatch {
    fn start(tool_calls: Value) -> (Self, JoinHandle<ToolDoneEvent>) {
        Self::start_as(tool_calls, PluginPermissions::trusted())
    }

    fn start_as(
        tool_calls: Value,
        permissions: PluginPermissions,
    ) -> (Self, JoinHandle<ToolDoneEvent>) {
        let (reg, host) = load_batch_host_as(MOVABLE_CHILD_SRC, permissions);
        let Chat {
            id,
            mailbox,
            seed,
            events: tasks,
        } = Chat::live();
        let (run, run_token) = CancelToken::new();
        let (mut ctx, events) = call_ctx(&reg, id, &run_token, BATCH_ID);
        let progress = ProgressScope::default();
        ctx.progress = Some(progress.clone());
        let call = thread::spawn({
            let reg = Arc::clone(&reg);
            move || exec(&reg, BATCH_TOOL, batch_input(tool_calls), &ctx)
        });
        let batch = Self {
            reg,
            host,
            tasks,
            mailbox,
            progress,
            _seed: seed,
            run: Some(run),
            events,
        };
        (batch, call)
    }

    fn child_tool(&self, name: &str, tag: &str) -> String {
        exec_tool(&self.reg, name, json!({ "tag": tag })).expect("child control tool failed")
    }

    fn wait_parked(&self, tag: &str) {
        poll_until(&format!("child {tag} never parked"), || {
            (self.child_tool(IS_PARKED_TOOL, tag) == IS_PARKED_TEXT).then_some(())
        });
    }

    fn release(&self, tag: &str) {
        self.child_tool(RELEASE_CHILD_TOOL, tag);
    }

    fn press(&self) -> MoveOutcome {
        request_move(&self.host.agent_handle(), BATCH_ID)
    }

    /// The user's Esc, before any move.
    fn cancel(&mut self) {
        self.run.take().expect("cancelled once").cancel();
    }

    /// Waits for the batch to say it is made of {want}, as `(id, name, status)`.
    fn wait_parts(&self, want: &[(&str, &str, PartStatus)]) {
        poll_until(&format!("parts never became {want:?}"), || {
            self.events
                .try_iter()
                .any(|envelope| match &envelope.event {
                    AgentEvent::ToolParts { id, parts } if id == BATCH_ID => parts
                        .iter()
                        .map(|part| (part.id.as_str(), part.name.as_str(), part.status))
                        .eq(want.iter().copied()),
                    _ => false,
                })
                .then_some(())
        });
    }

    /// The final reply of a moved batch, as its task tab gets it.
    fn task_done(&self) -> ToolDoneEvent {
        next_task_done(&self.tasks)
    }
}

/// The receipt carries what already finished, in the batch's own section
/// format, then lists the movable children still running. The late child's
/// result is the whole final reply.
#[test]
fn batch_moves_with_finished_results_inline_and_replies_with_the_late_children() {
    let (batch, call) = MovableBatch::start(json!([
        { "tool": OK_TOOL, "parameters": { "tag": "a" } },
        { "tool": HOOKED_TOOL, "parameters": { "tag": "early" } },
        { "tool": HOOKED_TOOL, "parameters": { "tag": "late" } },
    ]));
    batch.wait_parked("early");
    batch.wait_parked("late");
    batch.release("early");
    assert_statuses(&batch.progress, &[(STATUS_SUCCESS, 2)]);

    assert_eq!(batch.press(), MoveOutcome::Moved);

    let result = call.join().expect("batch thread");
    assert!(result.backgrounded);
    let receipt = result.output.as_text();
    let inline = format!(
        "{}{}{MOVED_HEADER}\n- {HOOKED_TOOL}: late",
        section(OK_TOOL, "ok:a"),
        section(HOOKED_TOOL, "hooked:early"),
    );
    let suffix = receipt
        .strip_prefix(&inline)
        .unwrap_or_else(|| panic!("receipt must start with the inline results:\n{receipt}"));
    assert!(suffix.starts_with("\n\n"), "{suffix}");
    assert!(suffix.contains(MOVED_BY_USER), "{suffix}");

    batch.release("late");
    let done = batch.task_done();
    assert!(!done.is_error);
    assert_eq!(
        done.output.as_text(),
        format!(
            "{}{}",
            section(HOOKED_TOOL, "hooked:late"),
            summary_all_ok(1)
        )
    );
    assert_eq!(batch.mailbox.drain().len(), 1);
}

/// A decline names the child that cannot move and leaves the batch untouched:
/// once that child finishes, the same press moves, and its result rides in
/// the receipt.
#[test]
fn batch_declines_while_a_child_without_a_hook_runs_and_moves_once_it_finished() {
    let (batch, call) = MovableBatch::start(json!([
        { "tool": HOOKED_TOOL, "parameters": { "tag": "a" } },
        { "tool": EDIT_TOOL, "parameters": { "tag": "b" } },
    ]));
    batch.wait_parked("a");
    batch.wait_parked("b");
    assert_eq!(
        batch.press(),
        MoveOutcome::NotMovable(WAITING_ON_EDIT.to_owned())
    );

    batch.release("b");
    assert_statuses(&batch.progress, &[(STATUS_SUCCESS, 1)]);
    assert_eq!(batch.press(), MoveOutcome::Moved);

    let result = call.join().expect("batch thread");
    assert!(result.backgrounded);
    let receipt = result.output.as_text();
    let inline = format!(
        "{}{MOVED_HEADER}\n- {HOOKED_TOOL}: a",
        section(EDIT_TOOL, "edit:b")
    );
    assert!(receipt.starts_with(&inline), "{receipt}");

    batch.release("a");
    assert_eq!(
        batch.task_done().output.as_text(),
        format!("{}{}", section(HOOKED_TOOL, "hooked:a"), summary_all_ok(1))
    );
}

/// A child that asked for the background lets the batch answer as soon as
/// nothing else runs: the batch moves itself, carrying the rest of the
/// results, and the ask never reaches the child, which cannot move inside it.
#[test]
fn a_background_child_moves_the_batch_once_the_rest_finished() {
    let (batch, call) = MovableBatch::start(json!([
        { "tool": HOOKED_TOOL, "parameters": { "tag": "slow", "background": true } },
        { "tool": EDIT_TOOL, "parameters": { "tag": "quick" } },
    ]));
    batch.wait_parked("slow");
    batch.wait_parked("quick");

    batch.release("quick");

    let result = call.join().expect("batch thread");
    assert!(result.backgrounded);
    let receipt = result.output.as_text();
    let inline = format!(
        "{}{MOVED_HEADER}\n- {HOOKED_TOOL}: slow",
        section(EDIT_TOOL, "edit:quick")
    );
    assert!(receipt.starts_with(&inline), "{receipt}");
    assert!(receipt.contains(MOVED_AS_ASKED), "{receipt}");

    batch.release("slow");
    assert_eq!(
        batch.task_done().output.as_text(),
        format!(
            "{}{}",
            section(HOOKED_TOOL, "hooked:slow"),
            summary_all_ok(1)
        )
    );
}

/// A part keeps the id of its place in the batch, so a subagent child, which
/// is a task of its own and no part, leaves the other ids alone.
#[test]
fn batch_parts_keep_their_place_and_leave_subagents_out() {
    let (batch, call) = MovableBatch::start(json!([
        { "tool": HOOKED_TOOL, "parameters": { "tag": "agent", "subagent": true } },
        { "tool": EDIT_TOOL, "parameters": { "tag": "quick" } },
        { "tool": HOOKED_TOOL, "parameters": { "tag": "slow" } },
    ]));
    for tag in ["agent", "quick", "slow"] {
        batch.wait_parked(tag);
    }
    batch.release("quick");
    batch.wait_parts(&[
        ("2", EDIT_TOOL, PartStatus::Done),
        ("3", "slow", PartStatus::Working),
    ]);

    batch.release("agent");
    batch.release("slow");
    batch.wait_parts(&[
        ("2", EDIT_TOOL, PartStatus::Done),
        ("3", "slow", PartStatus::Done),
    ]);
    assert!(!call.join().expect("batch thread").is_error);
}

/// A stopped batch lists what it cut short as cancelled, and what had
/// finished as it ended.
#[test]
fn a_cancelled_batch_lists_its_cut_children_as_cancelled() {
    let (mut batch, call) = MovableBatch::start(json!([
        { "tool": EDIT_TOOL, "parameters": { "tag": "quick" } },
        { "tool": HOOKED_TOOL, "parameters": { "tag": "slow" } },
    ]));
    batch.wait_parked("quick");
    batch.wait_parked("slow");
    batch.release("quick");
    batch.wait_parts(&[
        ("1", EDIT_TOOL, PartStatus::Done),
        ("2", "slow", PartStatus::Working),
    ]);

    batch.cancel();

    batch.wait_parts(&[
        ("1", EDIT_TOOL, PartStatus::Done),
        ("2", "slow", PartStatus::Cancelled),
    ]);
    let _ = call.join();
}

/// Without the right to move itself the batch runs everything in the
/// foreground, and says so once, the way the child alone would have.
#[test]
fn a_batch_that_cannot_move_for_its_child_says_why() {
    let (batch, call) = MovableBatch::start_as(
        json!([
            { "tool": HOOKED_TOOL, "parameters": { "tag": "slow", "background": true } },
            { "tool": EDIT_TOOL, "parameters": { "tag": "quick" } },
        ]),
        PluginPermissions::denied(),
    );
    batch.wait_parked("slow");
    batch.wait_parked("quick");
    batch.release("quick");
    batch.release("slow");

    let result = call.join().expect("batch thread");
    assert!(!result.backgrounded);
    let output = result.output.as_text();
    let (note, results) = output
        .split_once(FOREGROUND_NOTE_TAIL)
        .unwrap_or_else(|| panic!("no foreground note:\n{output}"));
    assert!(note.starts_with(FOREGROUND_NOTE_HEAD), "{output}");
    assert_eq!(
        results,
        format!(
            "{}{}{}",
            section(HOOKED_TOOL, "hooked:slow"),
            section(EDIT_TOOL, "edit:quick"),
            summary_all_ok(2)
        )
    );
}
