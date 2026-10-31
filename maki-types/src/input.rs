//! What a host hands the agent to start a run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use maki_config::SessionDefaults;
use serde::{Deserialize, Serialize};

use crate::message::{ImageSource, Message};
use crate::thinking::ThinkingConfig;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentMode {
    #[default]
    Build,
    Plan(PathBuf),
}

impl AgentMode {
    pub fn plan_path(&self) -> Option<&Path> {
        match self {
            Self::Plan(p) => Some(p),
            Self::Build => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptRef {
    pub qualified_name: String,
    pub arguments: HashMap<String, String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum InputSource {
    Tui,
    Acp,
    /// `maki -p` and sdk mode.
    Headless,
    /// A plugin prompting a session of its own, `task` subagents included.
    Plugin,
}

impl InputSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Acp => "acp",
            Self::Headless => "headless",
            Self::Plugin => "plugin",
        }
    }
}

/// A message queued ahead of the one that drives a run. It stays apart from
/// the preamble so `agent.user_message` gets to judge it like any other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EarlierInput {
    pub message: String,
    pub images: Vec<ImageSource>,
    pub preamble: Vec<Message>,
}

impl From<AgentInput> for EarlierInput {
    fn from(input: AgentInput) -> Self {
        Self {
            message: input.message,
            images: input.images,
            preamble: input.preamble,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInput {
    pub message: String,
    pub mode: AgentMode,
    pub images: Vec<ImageSource>,
    pub preamble: Vec<Message>,
    /// The rest of a burst of queued messages, oldest first.
    pub earlier: Vec<EarlierInput>,
    pub thinking: ThinkingConfig,
    pub fast: bool,
    /// No `Default` on this struct so adding a field forces every call site to update.
    pub workflow: bool,
    pub prompt: Option<Box<McpPromptRef>>,
    pub source: InputSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionInput {
    Prompt(AgentInput),
    Compact(Compaction),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compaction {
    pub instructions: Option<String>,
    pub workflow: bool,
}

impl AgentInput {
    /// MCP prompts run alone. Mode and workflow choose permissions and tools,
    /// so only inputs that agree on both can share a turn.
    pub fn batch_key(&self) -> Option<(AgentMode, bool)> {
        let Self {
            mode,
            workflow,
            prompt,
            message: _,
            images: _,
            preamble: _,
            earlier: _,
            thinking: _,
            fast: _,
            source: _,
        } = self;
        prompt.is_none().then(|| (mode.clone(), *workflow))
    }

    /// What a host with no toggle UI sends. `-p`, the SDK and ACP know nothing
    /// about the toggles beyond what config says, so they all build their input
    /// here and a knob added to [`SessionDefaults`] reaches every one of them.
    pub fn from_defaults(
        message: String,
        mode: AgentMode,
        images: Vec<ImageSource>,
        defaults: SessionDefaults,
        source: InputSource,
    ) -> Self {
        Self {
            message,
            mode,
            images,
            preamble: Vec::new(),
            earlier: Vec::new(),
            thinking: defaults.thinking.into(),
            fast: defaults.fast,
            workflow: defaults.workflow,
            prompt: None,
            source,
        }
    }
}

/// Folds a run of queued messages into one agent input. The last message
/// drives the run and the earlier ones ride in front of it as their own user
/// messages, so each keeps its images and the model answers the burst in one
/// request. They stay messages rather than preamble, so `agent.user_message`
/// gets to judge every one. The run shares one mode and one workflow by
/// construction, so only the preferences (thinking, fast) come from the last
/// message, the user's most recent intent.
pub fn merge_inputs(mut inputs: Vec<AgentInput>) -> Option<AgentInput> {
    let mut last = inputs.pop()?;
    let mut earlier: Vec<_> = inputs
        .into_iter()
        .flat_map(|input| {
            input
                .earlier
                .into_iter()
                .chain(std::iter::once(EarlierInput {
                    message: input.message,
                    images: input.images,
                    preamble: input.preamble,
                }))
        })
        .collect();
    earlier.append(&mut last.earlier);
    last.earlier = earlier;
    Some(last)
}
