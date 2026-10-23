use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::CancelToken;
use maki_agent::agent::{AgentCall, AgentSlot};
use maki_agent::tools::ToolRegistry;
use maki_agent::tools::hook::Verdict;
use maki_lua::{PluginHost, SessionRequest, UiAction};
use maki_storage::id::MakiId;
use serde_json::json;
use test_case::test_case;

const SKILL_SOURCE: &str = include_str!("../../plugins/skill/init.lua");
const MODEL: &str = "anthropic/claude-sonnet-4";
const DEADLINE: Duration = Duration::from_secs(30);
const RELEASE_SOURCE: &str = "---\nname: release\ndescription: Cut a release\ncompatibility: Requires git\n---\nRead CHANGELOG.md first.";
const SKILL_NOT_FOUND: &str = "skill not found: ";
const DURABLE_SKILL_KEY: &str = "skill:release";
const SKILL_USAGE: &str = "Usage: /skill name [request]";

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

fn isolated_skill_host() -> (tempfile::TempDir, Arc<ToolRegistry>, PluginHost) {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(".maki/skills/release");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), RELEASE_SOURCE).unwrap();
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    let source = format!(
        "maki.uv.cwd = function() return {} end\nmaki.uv.os_homedir = function() return nil end\nmaki.env.config_dir = function() return nil end\n{}",
        serde_json::to_string(dir.path()).unwrap(),
        SKILL_SOURCE,
    );
    host.load_source("skill", &source).unwrap();
    (dir, registry, host)
}

#[test_case("release include the changelog", true ; "injects_skill_before_the_model_runs")]
#[test_case("missing", false ; "unknown_skill_does_not_reach_the_model")]
#[test_case("", false ; "missing_name_does_not_start_a_prompt")]
#[test_case("release", false ; "invalidated_file_does_not_reach_the_model")]
fn direct_skill_command_uses_the_shared_loader(args: &str, loads: bool) {
    let (dir, registry, host) = isolated_skill_host();
    if args == "release" && !loads {
        fs::write(
            dir.path().join(".maki/skills/release/SKILL.md"),
            "---\n123\n---\nBody",
        )
        .unwrap();
    }
    assert!(
        host.command_reader()
            .load()
            .commands
            .iter()
            .any(|command| command.name.as_ref() == "/skill"),
        "the skill command must be registered"
    );
    let rx = host.ui_action_rx();
    host.event_handle()
        .run_command(Arc::from("skill"), Arc::from("/skill"), args.into(), 0);
    let session = MakiId::generate().to_string();
    let first = rx.recv_timeout(DEADLINE).unwrap();
    let first = if let UiAction::Session {
        req: SessionRequest::Current,
        reply_tx,
    } = first
    {
        reply_tx.send(Ok(json!(session))).unwrap();
        rx.recv_timeout(DEADLINE).unwrap()
    } else {
        first
    };
    match first {
        UiAction::Session {
            req: SessionRequest::Prompt { id, text },
            reply_tx,
        } => {
            assert_eq!(id.as_deref(), Some(session.as_str()));
            assert_eq!(text, format!("/skill {args}"));
            match user_message_verdict(&registry, &text, Some(&session), "plugin") {
                Verdict::Replaced(value) if loads => {
                    let rewritten = value["text"].as_str().unwrap();
                    assert!(rewritten.contains("compatibility: Requires git"));
                    assert!(rewritten.contains("Read CHANGELOG.md first."));
                    assert!(rewritten.contains(dir.path().to_str().unwrap()));
                    assert!(rewritten.ends_with("include the changelog"));
                    assert_eq!(value["durable"]["key"], DURABLE_SKILL_KEY);
                    assert!(
                        !value["durable"]["text"]
                            .as_str()
                            .unwrap()
                            .contains("include the changelog")
                    );
                }
                Verdict::Denied(message) if !loads => {
                    assert_eq!(message, format!("{SKILL_NOT_FOUND}{args}"))
                }
                _ => panic!("only valid skills may reach the model"),
            }
            reply_tx.send(Ok(json!("queued"))).unwrap();
        }
        UiAction::Flash(message) if args.is_empty() => assert_eq!(message, SKILL_USAGE),
        _ => panic!("unexpected skill command action"),
    }
}

fn user_message_verdict(
    registry: &ToolRegistry,
    text: &str,
    session: Option<&str>,
    source: &str,
) -> Verdict {
    let hook = registry.agent_hook().unwrap();
    let cancel = CancelToken::none();
    let call = AgentCall {
        session_id: session,
        task_id: None,
        model: MODEL,
        context_size: 0,
        context_window: 0,
        cancel: &cancel,
        deadline: Instant::now() + DEADLINE,
    };
    smol::block_on(hook.run(
        AgentSlot::UserMessage,
        json!({ "text": text, "source": source }),
        &call,
    ))
}

#[test_case("headless", "/skill release include the changelog", Some(true) ; "headless_activation_is_resolved_before_the_model")]
#[test_case("acp", "/skill release include the changelog", Some(true) ; "acp_activation_is_resolved_before_the_model")]
#[test_case("headless", "/skill missing", Some(false) ; "headless_unknown_skill_is_rejected")]
#[test_case("headless", "/skill", Some(false) ; "headless_missing_name_is_rejected")]
#[test_case("headless", "/skills release", None ; "other_prompts_are_unchanged")]
fn skill_activation_works_through_the_shared_agent_hook(
    source: &str,
    text: &str,
    loads: Option<bool>,
) {
    let (_dir, registry, _host) = isolated_skill_host();
    match (loads, user_message_verdict(&registry, text, None, source)) {
        (Some(true), Verdict::Replaced(value)) => {
            let text = value["text"].as_str().unwrap();
            let durable = value["durable"]["text"].as_str().unwrap();
            assert!(durable.contains("Read CHANGELOG.md first."));
            assert!(text.starts_with(durable));
            assert!(text.ends_with("include the changelog"));
            assert!(!durable.contains("include the changelog"));
            assert_eq!(value["durable"]["key"], DURABLE_SKILL_KEY);
            assert_eq!(value["source"], source);
        }
        (Some(false), Verdict::Denied(message)) => {
            let expected = if text == "/skill" {
                SKILL_USAGE.to_owned()
            } else {
                format!("{SKILL_NOT_FOUND}missing")
            };
            assert_eq!(message, expected);
        }
        (None, Verdict::Unchanged) => {}
        _ => panic!("skill activation must happen before the provider request"),
    }
}
