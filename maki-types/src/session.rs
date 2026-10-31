//! A persisted session, and what it says about itself besides its transcript.

use crate::event::{AgentEvent, Envelope};
use maki_storage::sessions::{Session, StoredMode, StoredSubagent, StoredThinking};
use std::sync::Arc;

use crate::message::{ContentBlock, Message, Role, TaskEnd, TaskSettled};
use crate::tokens::estimate_message_tokens;
use crate::tool::{ToolData, ToolOutput};
use crate::usage::TokenUsage;

/// The one spelling of a persisted maki session. It cannot live in
/// maki-storage: [`ToolOutput`] and [`ToolData`] are ours, and maki-storage
/// must not depend on us.
pub type StoredSession = Session<Message, TokenUsage, ToolOutput, ToolData>;

/// What a session says about itself besides its transcript: what export and
/// `maki.session.read` answer with.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionHeader {
    pub cwd: String,
    /// Empty until a turn picks one.
    pub model: String,
    pub title: String,
    pub mode: Option<StoredMode>,
    pub thinking: Option<StoredThinking>,
    pub fast: bool,
    pub usage: TokenUsage,
    pub context_size: u32,
    pub updated_at: u64,
    /// A turn is running. Always false for a session read from disk.
    pub working: bool,
}

impl SessionHeader {
    pub fn of(session: &StoredSession) -> Self {
        Self {
            cwd: session.cwd.clone(),
            model: session.model.clone(),
            title: session.title.clone(),
            mode: session.meta.mode,
            thinking: session.meta.thinking,
            fast: session.meta.fast,
            usage: session.token_usage,
            context_size: session.meta.context_size,
            updated_at: session.updated_at,
            working: false,
        }
    }
}

pub fn rewind_turns(messages: &[Message]) -> Vec<(usize, &str)> {
    let mut turns = Vec::new();
    let mut updates_from = None;
    for (index, message) in messages.iter().enumerate() {
        if message.is_context_update() {
            updates_from.get_or_insert(index);
            continue;
        }
        let start = updates_from.take().unwrap_or(index);
        if message.role == Role::User
            && !message.is_from_host()
            && let Some(text) = message.user_text()
        {
            turns.push((start, text));
        }
    }
    turns
}

/// Truncates and prunes a session, preserving the measured prompt baseline.
pub fn truncate_to_turn(session: &mut StoredSession, turn_index: usize, context_size: u32) -> u32 {
    let baseline = context_size.saturating_sub(estimate_message_tokens(session.messages()));
    session.truncate_messages(turn_index);
    session.prune_orphans(|m| m.tool_uses().map(|(id, _, _)| id.to_owned()).collect());
    let kept = estimate_message_tokens(session.messages());
    if kept == 0 { 0 } else { baseline + kept }
}

/// Records the data a task view needs after a session is reopened.
pub fn record_task_event(
    session: &mut StoredSession,
    envelope: &Envelope,
    plugin: Option<Arc<str>>,
) -> bool {
    let mut changed = false;
    if let Some(info) = &envelope.subagent
        && (matches!(envelope.event, AgentEvent::TurnComplete(_))
            || !session
                .subagents()
                .iter()
                .any(|task| task.tool_use_id == info.parent_tool_use_id))
    {
        changed = true;
        let mut tasks = session.subagents().to_vec();
        let task = match tasks
            .iter_mut()
            .find(|task| task.tool_use_id == info.parent_tool_use_id)
        {
            Some(task) => task,
            None => {
                tasks.push(StoredSubagent {
                    tool_use_id: info.parent_tool_use_id.clone(),
                    parent_call: info.parent_call.clone(),
                    name: info.name.clone(),
                    model: info.model.clone(),
                    thinking: info.opts.map(|opts| opts.thinking.into()),
                    fast: info.opts.is_some_and(|opts| opts.fast),
                    plugin: plugin.clone(),
                    cost: None,
                });
                tasks.last_mut().unwrap()
            }
        };
        task.parent_call.clone_from(&info.parent_call);
        if let AgentEvent::TurnComplete(turn) = &envelope.event {
            if let Some(cost) = turn.cost {
                *task.cost.get_or_insert(0.0) += cost;
            }
            task.model = Some(turn.model.clone());
        }
        session.set_subagents(tasks);
    }
    match &envelope.event {
        AgentEvent::SubagentHistory {
            tool_use_id,
            messages,
        } => {
            changed = true;
            session.set_subagent_messages(tool_use_id.clone(), messages.clone());
        }
        AgentEvent::ToolDone(done) => {
            changed = true;
            let moved = done.backgrounded
                || (plugin.is_some()
                    && envelope
                        .subagent
                        .as_ref()
                        .is_none_or(|info| info.parent_tool_use_id == done.id));
            let output_id = if moved {
                let mut tasks = session.subagents().to_vec();
                if let Some(task) = tasks.iter_mut().find(|task| task.tool_use_id == done.id) {
                    if plugin.is_some() {
                        task.plugin.clone_from(&plugin);
                    }
                } else {
                    tasks.push(StoredSubagent {
                        tool_use_id: done.id.clone(),
                        parent_call: Some(done.id.clone()),
                        name: done
                            .data
                            .as_ref()
                            .map(|data| data.summary.as_str())
                            .filter(|summary| !summary.is_empty())
                            .unwrap_or(&done.tool)
                            .to_owned(),
                        model: None,
                        thinking: None,
                        fast: false,
                        plugin,
                        cost: None,
                    });
                }
                session.set_subagents(tasks);
                let previous_end = session
                    .subagent_messages()
                    .get(&done.id)
                    .and_then(|messages| {
                        messages
                            .iter()
                            .rev()
                            .find_map(|message| message.settles.as_ref())
                    })
                    .map(|settled| settled.end);
                let end = previous_end.or_else(|| {
                    (!done.backgrounded).then_some(if done.is_error {
                        TaskEnd::Failed
                    } else {
                        TaskEnd::Finished
                    })
                });
                if done.backgrounded {
                    session.insert_tool_output(
                        done.id.clone(),
                        Arc::clone(&done.output),
                        done.data.clone(),
                    );
                }
                let output_id = format!("{}:background", done.id);
                if done.backgrounded
                    && previous_end.is_some()
                    && session.tool_outputs().contains_key(&output_id)
                {
                    return true;
                }
                session.set_subagent_messages(
                    done.id.clone(),
                    vec![
                        Message {
                            role: Role::Assistant,
                            content: vec![ContentBlock::tool_use(
                                output_id.clone(),
                                done.tool.to_string(),
                                done.data
                                    .as_ref()
                                    .map_or(serde_json::Value::Null, |data| data.input.clone()),
                            )],
                            ..Default::default()
                        },
                        Message {
                            role: Role::User,
                            content: vec![ContentBlock::tool_result(
                                output_id.clone(),
                                done.output.as_text(),
                                done.is_error,
                            )],
                            settles: end.map(|end| TaskSettled {
                                task: done.id.clone(),
                                end,
                            }),
                            ..Default::default()
                        },
                    ],
                );
                output_id
            } else {
                done.id.clone()
            };
            session.insert_tool_output(output_id, Arc::clone(&done.output), done.data.clone());
        }
        AgentEvent::TaskEnded { id, end } => {
            let mut messages = session
                .subagent_messages()
                .get(id)
                .map_or_else(Vec::new, |messages| messages.as_ref().clone());
            let mut mark = Message::observation(String::new());
            mark.settles = Some(TaskSettled {
                task: id.clone(),
                end: *end,
            });
            messages.push(mark);
            session.set_subagent_messages(id.clone(), messages);
            changed = true;
        }
        _ => {}
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ToolDoneEvent;
    use crate::tool::DEFAULT_DATA_VERSION;
    use maki_storage::sessions::SessionClaim;
    use serde_json::json;

    const CALL: &str = "bash-call";
    const OUTPUT: &str = "background output";
    const RECEIPT: &str = "moved to background";

    #[test_case::test_case(TaskEnd::Cancelled, true, false ; "cancelled_final")]
    #[test_case::test_case(TaskEnd::Lost, false, true ; "lost_before_receipt")]
    #[test_case::test_case(TaskEnd::Finished, true, true ; "final_before_receipt")]
    fn a_moved_tool_reloads_its_task_without_replacing_the_receipt(
        end: TaskEnd,
        final_reply: bool,
        final_before_receipt: bool,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut session = StoredSession::new("model", "/p");
        let claim = SessionClaim::acquire_in(session.id, dir.path()).unwrap();
        session.push_message(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                CALL,
                "bash",
                json!({"command": "ls"}),
            )],
            ..Default::default()
        });
        let mut receipt = Message::user(RECEIPT.into());
        receipt.receipts.push(CALL.into());
        session.push_message(receipt);
        session.insert_tool_output(
            CALL.into(),
            Arc::new(ToolOutput::Plain(RECEIPT.into())),
            None,
        );
        let data = Arc::new(ToolData {
            tool: "bash".into(),
            version: DEFAULT_DATA_VERSION,
            input: json!({"command": "ls"}),
            summary: "list files".into(),
            annotations: Vec::new(),
            state: json!({"text": OUTPUT}),
            children: Vec::new(),
            rewritten: false,
        });
        let event = Envelope {
            event: AgentEvent::ToolDone(Box::new(ToolDoneEvent {
                id: CALL.into(),
                tool: "bash".into(),
                output: Arc::new(ToolOutput::Plain(OUTPUT.into())),
                is_error: false,
                annotation: None,
                written_path: None,
                call: None,
                data: Some(Arc::clone(&data)),
                backgrounded: false,
            })),
            subagent: None,
            run_id: 0,
        };
        let AgentEvent::ToolDone(done) = &event.event else {
            unreachable!()
        };
        let mut receipt = done.as_ref().clone();
        receipt.output = Arc::new(ToolOutput::Plain(RECEIPT.into()));
        receipt.backgrounded = true;
        let receipt = Envelope {
            event: AgentEvent::ToolDone(Box::new(receipt)),
            subagent: None,
            run_id: 0,
        };
        let ended = Envelope {
            event: AgentEvent::TaskEnded {
                id: CALL.into(),
                end,
            },
            subagent: None,
            run_id: 0,
        };
        if !final_before_receipt {
            assert!(record_task_event(&mut session, &receipt, None));
        }
        assert!(record_task_event(&mut session, &ended, Some("bash".into())));
        if final_reply {
            assert!(record_task_event(&mut session, &event, Some("bash".into())));
        }
        if final_before_receipt {
            assert!(record_task_event(&mut session, &receipt, None));
        }
        session.save_to(&claim, dir.path()).unwrap();
        let mut loaded = StoredSession::load_from(session.id, dir.path()).unwrap();
        assert_eq!(loaded.tool_outputs()[CALL].as_text(), RECEIPT);
        assert_eq!(loaded.subagents()[0].tool_use_id, CALL);
        assert_eq!(loaded.subagents()[0].name, "list files");
        let transcript = &loaded.subagent_messages()[CALL];
        let (id, tool, input) = transcript[0].tool_uses().next().unwrap();
        assert_eq!(tool, "bash");
        assert_eq!(input, &data.input);
        assert_eq!(
            loaded.tool_outputs()[id].as_text(),
            if final_reply { OUTPUT } else { RECEIPT }
        );
        assert_eq!(
            transcript
                .iter()
                .rev()
                .find_map(|message| message.settles.as_ref())
                .unwrap()
                .end,
            end
        );
        assert_eq!(loaded.tool_data()[id].state, data.state);
        loaded.truncate_messages(0);
        loaded.prune_orphans(|message| {
            message
                .tool_uses()
                .map(|(id, _, _)| id.to_owned())
                .collect()
        });
        assert!(loaded.subagents().is_empty());
        assert!(loaded.subagent_messages().is_empty());
        assert!(loaded.tool_outputs().is_empty());
    }
}
