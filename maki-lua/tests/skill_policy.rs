use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::CancelToken;
use maki_agent::agent::{AgentCall, AgentSlot};
use maki_agent::tools::ToolRegistry;
use maki_agent::tools::hook::Verdict;
use maki_lua::PluginHost;
use serde_json::json;

const SKILL_SOURCE: &str = include_str!("../../plugins/skill/init.lua");
const MODEL: &str = "anthropic/claude-sonnet-4";
const DEADLINE: Duration = Duration::from_secs(30);

#[test]
fn skill_declares_core_retention_without_a_compaction_hook() {
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    let source = format!(
        "maki.uv.cwd = function() return nil end\nmaki.uv.os_homedir = function() return nil end\nmaki.env.config_dir = function() return nil end\n{}",
        SKILL_SOURCE,
    );
    host.load_source("skill", &source).unwrap();
    let tool = registry.get("skill").unwrap();
    assert_eq!(
        tool.tool.durable_key(&json!({"name": "maki-plugin-dev"})),
        Some("maki-plugin-dev")
    );
    if let Some(hook) = registry.agent_hook() {
        let cancel = CancelToken::none();
        let call = AgentCall {
            session_id: None,
            task_id: Some("child"),
            model: MODEL,
            context_size: 0,
            context_window: 0,
            cancel: &cancel,
            deadline: Instant::now() + DEADLINE,
        };
        assert!(matches!(
            smol::block_on(hook.run(AgentSlot::CompactBefore, json!({"reason": "auto"}), &call)),
            Verdict::Unchanged
        ));
    }
}
