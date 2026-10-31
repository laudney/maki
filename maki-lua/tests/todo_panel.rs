use std::collections::HashMap;
use std::sync::Arc;

use maki_agent::AgentMode;
use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::stub_ctx;
use maki_config::PluginsConfig;
use maki_lua::PluginHost;
use serde_json::json;

const TODO_TOOL: &str = "todo_write";
const TODO_ITEM: &str = "wire the panel";

fn todo_host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let mut host = PluginHost::start(Arc::clone(&reg), true).unwrap();
    host.load_builtins(&PluginsConfig {
        enabled: true,
        names: vec![TODO_TOOL.to_owned()],
        packages: Vec::new(),
        opts: HashMap::new(),
    })
    .unwrap();
    (reg, host)
}

#[test]
fn todo_agent_composition_runs_the_tool_without_its_panel() {
    let (reg, _host) = todo_host();
    let input = json!({ "todos": [{ "content": TODO_ITEM, "status": "in_progress" }] });
    let tool = &reg.get(TODO_TOOL).unwrap().tool;
    let result = maki_lua::test_support::runtime().block_on(
        tool.parse(&input)
            .unwrap()
            .execute(&stub_ctx(&AgentMode::Build)),
    );
    assert!(result.output.is_ok());
}
