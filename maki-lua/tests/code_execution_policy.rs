//! Tests the code_execution plugin's interpreter visibility: one predicate
//! gates both `describe` text and the handler's fn-map, so what the model
//! sees is exactly what the interpreter can call.

use std::collections::HashMap;
use std::sync::Arc;

use maki_agent::AgentMode;
use maki_agent::mcp::test_support::stub_session;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{
    DescriptionContext, ToolAudience, ToolContext, ToolFilter, ToolRegistry, local_tool,
};
use maki_config::{PluginFileConfig, PluginsConfig};
use maki_lua::PluginHost;

const CODE_EXECUTION_SRC: &str = include_str!("../../plugins/code_execution/init.lua");

const ECHO_PREFIX: &str = "echo:";
const FAIL_MSG: &str = "fixture blew up";
const ERROR_PREFIX: &str = "[ERROR] ";
const GATHER_HINT_SUBSTR: &str = "`gather(...)` keeps the other results";
const TASK_PREFIX: &str = "task:";
const WORKFLOW_NOTE_SUBSTR: &str = "Workflow mode: orchestrate subagents";
const WF_TASK_GATED_LINE: &str = "Not callable: wf_task";
const INTERP_ECHO_SIG: &str = "- interp_echo(msg: str, count: int = None, flag: bool = None, items: list = None, raw: any = None) -> str";
const WF_TASK_SIG: &str = "- wf_task(prompt: str, model_tier: str = None) -> str";
const SUB_TOOL_SIG: &str = "- sub_tool() -> str";
const MCP_NOTE_SUBSTR: &str = "MCP tools are callable too";
const MCP_TOOL_QUALIFIED: &str = "srv.fetch_issue";
const MCP_TOOL_WIRE: &str = "srv__fetch_issue";
/// Servers publish names Python cannot spell; the sandbox binds the alias.
const HYPHEN_QUALIFIED: &str = "srv.get-docs";
const HYPHEN_ALIAS: &str = "srv__get_docs";
/// The stub transport fails every request with this, so seeing it proves the
/// call reached MCP instead of dying at name lookup.
const MCP_REACHED_ERR: &str = "unknown MCP tool";
const NAME_ERROR: &str = "NameError";
const CODE_EXECUTION: &str = "code_execution";
const TOOLS_MCP_PROBE: &str = "tools_mcp_probe";
/// Stands in for an ACP client tool: dispatched from the context, not the
/// registry.
const CLIENT_TOOL: &str = "client_probe";
const CLIENT_TOOL_OUT: &str = "client ran";
const EDIT_TARGET: &str = "f.rs";
const MARKER_A: &str = "AAA";
const MARKER_B: &str = "BBB";
const EDITED_A: &str = "aaa";
const EDITED_B: &str = "bbb";
const OPEN_TARGET: &str = "notes.txt";
const OPEN_ORIGINAL: &str = "hello\n";
const PLAN_FILE: &str = "plan.md";
const FILE_WRITE_FAILED: &str = "file write failed";
const OPEN_REFUSED: &str = "refused";
const BLOCKING_READ_HOOK: &str =
    r#"maki.api.set_slot("tool.read.input", function() return nil, "blocked" end)"#;
/// Points reads at a file that exists, so the read tool says yes.
const REWRITING_READ_HOOK: &str = concat!(
    r#"maki.api.set_slot("tool.read.input", function(prev, input) input.path = ""#,
    env!("CARGO_MANIFEST_DIR"),
    r#"/Cargo.toml" return input end)"#
);
const WILDCARD_INPUT_HOOK: &str =
    r#"maki.api.set_slot("tool.*.input", function(prev, input) return input end)"#;
const NO_POLICY: &str = "";
const LIST_HOOK: &str =
    r#"maki.api.set_slot("tool.list.input", function(prev, input) return input end)"#;
const RUNTIME_ERROR: &str = "RuntimeError";
const MADE_DIR: &str = "made/deep";
const OPEN_APPEND_REFUSED: &str = "write tool has no append option";
const WRITE_WITHOUT_APPEND: &str = r#"maki.api.register_tool({
    name = "write",
    description = "replacement write without append",
    audiences = { "main", "interpreter" },
    schema = { type = "object", properties = { path = { type = "string" }, content = { type = "string" } } },
    handler = function(input) maki.fs.write(input.path, input.content) return "ok" end,
})"#;

fn fixture_plugin() -> String {
    format!(
        r#"
maki.api.register_tool({{
    name = "wf_task",
    description = "workflow-only fixture",
    audiences = {{ "main", "workflow" }},
    schema = {{
        type = "object",
        required = {{ "prompt" }},
        properties = {{
            prompt = {{ type = "string" }},
            model_tier = {{ type = "string" }},
        }},
    }},
    handler = function(input) return "{TASK_PREFIX}" .. input.prompt end,
}})
maki.api.register_tool({{
    name = "interp_echo",
    description = "interpreter fixture",
    audiences = {{ "main", "interpreter" }},
    schema = {{
        type = "object",
        required = {{ "msg" }},
        properties = {{
            msg = {{ type = "string" }},
            count = {{ type = "integer" }},
            flag = {{ type = "boolean" }},
            items = {{ type = "array", items = {{ type = "string" }} }},
            raw = {{ description = "no type, maps to any" }},
        }},
    }},
    handler = function(input) return "{ECHO_PREFIX}" .. input.msg end,
}})
maki.api.register_tool({{
    name = "interp_fail",
    description = "failing interpreter fixture",
    audiences = {{ "main", "interpreter" }},
    schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
    handler = function() return {{ llm_output = "{FAIL_MSG}", is_error = true }} end,
}})
maki.api.register_tool({{
    name = "{TOOLS_MCP_PROBE}",
    description = "returns the code_execution description agent.tools built",
    audiences = {{ "main" }},
    schema = {{
        type = "object",
        required = {{ "mcp" }},
        properties = {{ mcp = {{ type = "boolean" }} }},
        additionalProperties = false,
    }},
    handler = function(input, ctx)
        local defs, err = maki.agent.tools(ctx, {{ audience = "main", mcp = input.mcp }})
        if err then return {{ llm_output = err, is_error = true }} end
        for _, d in ipairs(defs) do
            if d.name == "{CODE_EXECUTION}" then return d.description end
        end
        return {{ llm_output = "{CODE_EXECUTION} missing", is_error = true }}
    end,
}})
maki.api.register_tool({{
    name = "sub_tool",
    description = "subagent fixture",
    audiences = {{ "general_sub", "interpreter" }},
    schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
    handler = function() return "" end,
}})
"#
    )
}

fn setup_with(reg: Arc<ToolRegistry>) -> (Arc<ToolRegistry>, PluginHost) {
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("code_execution", CODE_EXECUTION_SRC)
        .expect("real plugin should load");
    host.load_source("policy_fixtures", &fixture_plugin())
        .expect("fixture plugin should load");
    (reg, host)
}

fn setup() -> (Arc<ToolRegistry>, PluginHost) {
    setup_with(Arc::new(ToolRegistry::new()))
}

/// `maki.agent.tools` describes the process-wide registry, so the fixtures have
/// to land there. Safe: nextest runs each test in its own process.
fn setup_global() -> (Arc<ToolRegistry>, PluginHost) {
    setup_with(Arc::clone(ToolRegistry::global_arc()))
}

fn describe(
    reg: &ToolRegistry,
    filter: &ToolFilter,
    audience: ToolAudience,
    workflow: bool,
) -> String {
    reg.get(CODE_EXECUTION)
        .expect("code_execution registered")
        .tool
        .description(&DescriptionContext {
            filter,
            audience,
            workflow,
            mcp: false,
        })
        .into_owned()
}

fn run_tool(
    reg: &ToolRegistry,
    ctx: &ToolContext,
    name: &str,
    input: serde_json::Value,
) -> Result<String, String> {
    let entry = reg.get(name).unwrap_or_else(|| panic!("{name} registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    maki_lua::test_support::runtime()
        .block_on(async { inv.execute(ctx).await })
        .output
        .map(|out| match out {
            maki_agent::ToolOutput::Plain(s) => s.text,
            other => panic!("unexpected output: {other:?}"),
        })
}

fn exec_code(reg: &ToolRegistry, ctx: &ToolContext, code: &str) -> Result<String, String> {
    run_tool(
        reg,
        ctx,
        CODE_EXECUTION,
        serde_json::json!({ "code": code, "timeout": 10 }),
    )
}

/// One seam for every scenario: `shape` sets whatever the case is about
/// (audience, workflow, MCP, host tools) on an otherwise stock context.
fn shaped_ctx(reg: &Arc<ToolRegistry>, shape: impl FnOnce(&mut ToolContext)) -> ToolContext {
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.registry = Arc::clone(reg);
    shape(&mut ctx);
    ctx
}

fn run_code_with(code: &str, shape: impl FnOnce(&mut ToolContext)) -> Result<String, String> {
    let (reg, _host) = setup();
    exec_code(&reg, &shaped_ctx(&reg, shape), code)
}

fn run_code(code: &str) -> Result<String, String> {
    run_code_with(code, |_| {})
}

#[test]
fn describe_main_hides_workflow_and_sub_tools() {
    let (reg, _host) = setup();
    let desc = describe(&reg, &ToolFilter::All, ToolAudience::MAIN, false);
    assert!(
        desc.lines().any(|l| l == INTERP_ECHO_SIG),
        "expected exact line {INTERP_ECHO_SIG:?} in: {desc}"
    );
    assert!(!desc.contains(WF_TASK_SIG), "got: {desc}");
    assert!(desc.lines().any(|l| l == WF_TASK_GATED_LINE), "got: {desc}");
    assert!(!desc.contains("sub_tool"), "got: {desc}");
    assert!(!desc.contains(WORKFLOW_NOTE_SUBSTR), "got: {desc}");
}

#[test]
fn describe_workflow_adds_workflow_tools_and_note() {
    let (reg, _host) = setup();
    let desc = describe(&reg, &ToolFilter::All, ToolAudience::MAIN, true);
    assert!(desc.contains(WF_TASK_SIG), "got: {desc}");
    assert!(desc.contains(WORKFLOW_NOTE_SUBSTR), "got: {desc}");
    assert!(!desc.contains(WF_TASK_GATED_LINE), "got: {desc}");
    assert!(!desc.contains("sub_tool"), "got: {desc}");
}

#[test]
fn describe_general_sub_scopes_to_sub_audience() {
    let (reg, _host) = setup();
    let desc = describe(&reg, &ToolFilter::All, ToolAudience::GENERAL_SUB, false);
    assert!(desc.contains(SUB_TOOL_SIG), "got: {desc}");
    assert!(!desc.contains("interp_echo"), "got: {desc}");
    assert!(!desc.contains("wf_task"), "got: {desc}");
}

#[test_case::test_case("interp_echo" ; "callable")]
#[test_case::test_case("wf_task" ; "workflow_gated")]
fn except_filter_removes_tool_from_description(name: &str) {
    let (reg, _host) = setup();
    let filter = ToolFilter::AllExcept(vec![name.to_owned()]);
    let desc = describe(&reg, &filter, ToolAudience::MAIN, false);
    assert!(!desc.contains(name), "got: {desc}");
}

#[test]
fn interpreter_calls_advertised_tool_end_to_end() {
    let out = run_code("result = await interp_echo(msg='hi')\nprint(result)")
        .expect("advertised tool must be callable");
    assert!(out.contains(&format!("{ECHO_PREFIX}hi")), "got: {out}");
}

/// The list form covers a model that forgets the `*`. It must run rather than
/// cost a TypeError round-trip.
#[test_case::test_case("gather(interp_echo(msg='hi'), interp_fail())" ; "varargs")]
#[test_case::test_case("gather([interp_echo(msg='hi'), interp_fail()])" ; "single_list")]
fn gather_keeps_sibling_results_when_one_call_fails(call: &str) {
    let out = run_code(&format!("ok, bad = await {call}\nprint(ok)\nprint(bad)"))
        .expect("a failed call must not fail the script");
    assert!(out.contains(&format!("{ECHO_PREFIX}hi")), "got: {out}");
    assert!(
        out.contains(&format!("{ERROR_PREFIX}interp_fail: {FAIL_MSG}")),
        "failed call must name the tool and its error: {out}"
    );
}

/// Regression, and the sibling of `parallel_edits_to_one_file_all_apply` in
/// `batch_policy`: `asyncio.gather` is the other way two `edit` calls end up
/// interleaved, and a batch-only fix would miss it.
#[test]
fn parallel_edits_to_one_file_all_apply() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join(EDIT_TARGET);
    std::fs::write(&path, format!("{MARKER_A}\n{MARKER_B}\n")).unwrap();
    let quoted = path.to_str().expect("utf-8 temp path");

    let reg = Arc::new(ToolRegistry::new());
    let _host = PluginHost::with_all_builtins(Arc::clone(&reg)).unwrap();
    let out = exec_code(
        &reg,
        &shaped_ctx(&reg, |_| {}),
        &format!(
            "await asyncio.gather(\n\
             \x20 edit(path='{quoted}', old_string='{MARKER_A}', new_string='{EDITED_A}'),\n\
             \x20 edit(path='{quoted}', old_string='{MARKER_B}', new_string='{EDITED_B}'),\n\
             )"
        ),
    )
    .expect("both edits must succeed");

    assert!(!out.contains(ERROR_PREFIX), "got: {out}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{EDITED_A}\n{EDITED_B}\n")
    );
}

/// Binds `path` to a file holding [`OPEN_ORIGINAL`] and returns the script's
/// result with what the file holds afterwards. `policy` is Lua loaded next to
/// the builtins, minus the `disabled` ones.
fn run_on_file(
    code: &str,
    policy: &str,
    shape: fn(&mut ToolContext),
    disabled: &[&str],
) -> (Result<String, String>, String) {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join(OPEN_TARGET);
    std::fs::write(&path, OPEN_ORIGINAL).unwrap();
    let quoted = path.to_str().expect("utf-8 temp path");

    let reg = Arc::new(ToolRegistry::new());
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_builtins(&PluginsConfig::from_plugins(
        disabled
            .iter()
            .map(|name| {
                let off = PluginFileConfig {
                    enabled: Some(false),
                    ..PluginFileConfig::default()
                };
                ((*name).to_owned(), off)
            })
            .collect(),
    ))
    .unwrap();
    host.load_source("policy", policy).unwrap();
    let out = exec_code(
        &reg,
        &shaped_ctx(&reg, shape),
        &format!("path = '{quoted}'\n{code}"),
    );
    (out, std::fs::read_to_string(&path).unwrap())
}

#[test_case::test_case("text = open(path).read()\nwith open(path, 'w') as f:\n    f.write(text.upper())", "HELLO\n" ; "read_then_write")]
#[test_case::test_case("with open(path, 'a') as f:\n    f.write('world\\n')\n    f.write('!')", "hello\nworld\n!" ; "append")]
fn open_reads_and_writes_through_the_file_tools(code: &str, expected: &str) {
    let (out, content) = run_on_file(code, NO_POLICY, |_| {}, &[]);
    out.expect("open must be served");
    assert_eq!(content, expected);
}

/// Validation drops the `append` flag a replacement tool does not declare, so
/// going ahead would overwrite the file with only the new text.
#[test]
fn open_append_is_refused_when_the_write_tool_cannot_append() {
    let code = "with open(path, 'a') as f:\n    f.write('world')";
    let (out, content) = run_on_file(code, WRITE_WITHOUT_APPEND, |_| {}, &["write"]);
    let err = out.expect_err("the append must be refused");
    assert!(err.contains(OPEN_APPEND_REFUSED), "got: {err}");
    assert_eq!(content, OPEN_ORIGINAL);
}

/// `open()` reads the file itself, so it cannot follow a hook that points the
/// read somewhere else. Refusing is the only answer that never hands the
/// script a file the hook steered it away from.
#[test_case::test_case(BLOCKING_READ_HOOK ; "blocking_hook")]
#[test_case::test_case(REWRITING_READ_HOOK ; "rewriting_hook")]
#[test_case::test_case(WILDCARD_INPUT_HOOK ; "wildcard_hook")]
fn open_reads_are_refused_while_the_read_tool_is_hooked(policy: &str) {
    let code = format!(
        "try:\n    out = open(path).read()\nexcept OSError:\n    out = '{OPEN_REFUSED}'\nout"
    );
    let (out, _) = run_on_file(&code, policy, |_| {}, &[]);
    let out = out.expect("the refusal must reach the script as OSError");
    assert!(out.contains(OPEN_REFUSED), "got: {out}");
}

#[test_case::test_case(|ctx| ctx.audience = ToolAudience::RESEARCH_SUB ; "write_tool_not_callable")]
#[test_case::test_case(|ctx| ctx.mode = AgentMode::Plan(PLAN_FILE.into()) ; "plan_mode_blocks_the_write_tool")]
fn open_for_writing_is_refused_where_the_write_tool_is(shape: fn(&mut ToolContext)) {
    let (out, content) = run_on_file("open(path, 'w').write('x')", NO_POLICY, shape, &[]);
    let err = out.expect_err("the write must be refused");
    assert!(err.contains(FILE_WRITE_FAILED), "got: {err}");
    assert_eq!(content, OPEN_ORIGINAL);
}

/// Loud on purpose, since `os.path.exists` would turn an `OSError` into False.
/// mkdir looks before it creates, so it needs the list tool too.
#[test_case::test_case("os.path.exists(path)", LIST_HOOK, &[] ; "list_tool_hooked")]
#[test_case::test_case("os.path.exists(path)", NO_POLICY, &["list"] ; "list_tool_disabled")]
#[test_case::test_case("os.makedirs(path + '.d', exist_ok=True)", NO_POLICY, &["list"] ; "mkdir_with_list_tool_disabled")]
fn os_metadata_is_refused_where_the_list_tool_is(code: &str, policy: &str, disabled: &[&str]) {
    let (out, _) = run_on_file(code, policy, |_| {}, disabled);
    let err = out.expect_err("the query must be refused");
    assert!(err.contains(RUNTIME_ERROR), "got: {err}");
}

/// `build_mode` also proves metadata is served, as a refused `isdir` would raise.
#[test_case::test_case(|_| {}, true ; "build_mode")]
#[test_case::test_case(|ctx| ctx.mode = AgentMode::Plan(PLAN_FILE.into()), false ; "plan_mode_blocks_the_write_tool")]
#[test_case::test_case(|ctx| ctx.audience = ToolAudience::RESEARCH_SUB, false ; "write_tool_not_callable")]
fn os_makedirs_goes_where_the_write_tool_would(shape: fn(&mut ToolContext), made: bool) {
    let code = format!(
        "target = os.path.join(os.path.dirname(path), '{MADE_DIR}')\ntry:\n    os.makedirs(target)\nexcept OSError:\n    pass\nos.path.isdir(target)"
    );
    let (out, _) = run_on_file(&code, NO_POLICY, shape, &[]);
    let out = out.expect("a refused mkdir raises OSError, which the script catches");
    assert!(out.contains(&format!("return: {made}")), "got: {out}");
}

/// Only a failed tool call belongs in the results. The script's own mistake
/// must stop the run instead of hiding as an `[ERROR]` entry.
#[test]
fn gather_lets_script_errors_through() {
    let err = run_code("await gather(interp_echo(msg='hi'), 'not a call')")
        .expect_err("awaiting a non-call must raise");
    assert!(!err.contains(ERROR_PREFIX), "got: {err}");
}

/// `asyncio.gather` still cancels its siblings, so its error is the one place
/// worth pointing at the wrapper. A bare await is fail-fast on purpose and
/// must not nag.
#[test_case::test_case("await interp_fail()", false ; "plain_await_stays_fail_fast")]
#[test_case::test_case("await asyncio.gather(interp_echo(msg='hi'), interp_fail())", true ; "asyncio_gather_points_at_the_wrapper")]
fn failed_call_error_names_the_tool_and_hints_only_for_asyncio_gather(code: &str, hint: bool) {
    let err = run_code(code).expect_err("a failed call must raise");
    assert!(
        err.contains("interp_fail") && err.contains(FAIL_MSG),
        "got: {err}"
    );
    assert_eq!(err.contains(GATHER_HINT_SUBSTR), hint, "got: {err}");
}

/// The model counts lines in the code it wrote, so the preamble it never sees
/// must not shift them. Guards the plugin passing the preamble as its own
/// option instead of pasting it onto the code.
#[test]
fn traceback_lines_are_numbered_from_the_users_first_line() {
    let err = run_code("x = 1\nprint(boom_undefined)").expect_err("undefined name must error");
    assert!(err.contains("line 2, in <module>"), "got: {err}");
}

#[test]
fn workflow_tool_not_callable_when_workflow_false() {
    let err = run_code("await wf_task(prompt='x')")
        .expect_err("workflow tool must not be in the fn-map when workflow=false");
    assert!(err.contains("wf_task"), "got: {err}");
}

/// Regression guard: the old `ctx:agent_context()` take() used to reset
/// audience/workflow reads, leaving workflow tools uncallable.
#[test]
fn workflow_tool_callable_when_workflow_true() {
    let out = run_code_with("result = await wf_task(prompt='x')\nprint(result)", |ctx| {
        ctx.workflow = true
    })
    .expect("workflow tool must be callable when workflow=true");
    assert!(out.contains(&format!("{TASK_PREFIX}x")), "got: {out}");
}

// --- context tools (MCP, client tools) ---

fn with_mcp(qualified: &'static str) -> impl FnOnce(&mut ToolContext) {
    move |ctx| ctx.mcp = Some(stub_session(&[(qualified, "")]))
}

/// A caller that drops MCP for a subagent must not hand it a description
/// promising MCP tools that subagent cannot call.
#[test_case::test_case(true ; "session_keeps_mcp")]
#[test_case::test_case(false ; "session_drops_mcp")]
fn agent_tools_describes_mcp_only_when_the_session_keeps_it(enabled: bool) {
    let (reg, _host) = setup_global();
    let ctx = shaped_ctx(&reg, with_mcp(MCP_TOOL_QUALIFIED));
    let desc = run_tool(
        &reg,
        &ctx,
        TOOLS_MCP_PROBE,
        serde_json::json!({ "mcp": enabled }),
    )
    .expect("probe must describe code_execution");
    assert_eq!(desc.contains(MCP_NOTE_SUBSTR), enabled, "got: {desc}");
}

/// The stub leaves the tool deferred, so binding it means reading the MCP index
/// rather than the request's tool array.
#[test_case::test_case(MCP_TOOL_QUALIFIED, MCP_TOOL_WIRE ; "deferred_tool_binds_its_wire_name")]
#[test_case::test_case(HYPHEN_QUALIFIED, HYPHEN_ALIAS    ; "hyphenated_tool_binds_an_alias")]
fn mcp_tool_is_callable_from_the_sandbox(qualified: &'static str, bound: &str) {
    let err = run_code_with(&format!("await {bound}()"), with_mcp(qualified))
        .expect_err("the stub transport fails every call");
    assert!(err.contains(MCP_REACHED_ERR), "got: {err}");
}

fn with_client_tool(audience: ToolAudience) -> impl FnOnce(&mut ToolContext) {
    let tool = local_tool(audience, |_, _| {
        Box::pin(async { Ok(CLIENT_TOOL_OUT.to_owned()) })
    });
    move |ctx| ctx.local_tools = Arc::new(HashMap::from([(CLIENT_TOOL.to_owned(), tool)]))
}

#[test]
fn client_tool_is_callable_when_it_admits_the_interpreter() {
    let out = run_code_with(
        &format!("print(await {CLIENT_TOOL}())"),
        with_client_tool(ToolAudience::MAIN | ToolAudience::INTERPRETER),
    )
    .expect("a client tool the interpreter may call must be callable");
    assert!(out.contains(CLIENT_TOOL_OUT), "got: {out}");
}

/// `MODEL` is what a host tool gets when nobody opted it in, and a subagent's
/// `structured_output` rides on that default.
#[test]
fn model_only_client_tool_stays_out_of_the_sandbox() {
    let err = run_code_with(
        &format!("await {CLIENT_TOOL}()"),
        with_client_tool(ToolAudience::MODEL),
    )
    .expect_err("a model-only host tool must not be bound at all");
    assert!(err.contains(NAME_ERROR), "got: {err}");
}

// --- script rendering ---
