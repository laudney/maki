use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::agent::QueuedMessage;
use crate::agent::capabilities::{Feature, UNAVAILABLE_REMOTE};
use crate::components::Status;
use crate::theme;
use maki_lua_core::agent_autocmd::HostEvent;
use maki_types::input::{AgentInput, AgentMode, InputSource};
use maki_types::thinking::RequestOptions;
use ratatui::style::{Color, Modifier, Style};

use super::App;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Build,
    Plan,
}

pub(crate) enum PlanTrigger {
    WriteDone,
    InteractivePrompt,
}

impl Mode {
    pub(crate) fn color(&self) -> Color {
        match self {
            Self::Build => theme::current().mode_build,
            Self::Plan => theme::current().mode_plan,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum PlanState {
    #[default]
    None,
    Drafting(PathBuf),
    Ready(PathBuf),
}

impl PlanState {
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::None => Option::None,
            Self::Drafting(p) | Self::Ready(p) => Some(p),
        }
    }

    pub(crate) fn mark_ready(&mut self) {
        if let Self::Drafting(p) = self {
            *self = Self::Ready(std::mem::take(p));
        }
    }

    pub(crate) fn mark_drafting(&mut self) {
        if let Self::Ready(p) = self {
            *self = Self::Drafting(std::mem::take(p));
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }
}

impl App {
    pub(crate) fn transition_plan(&mut self, trigger: PlanTrigger) {
        if self.state.mode != Mode::Plan {
            return;
        }
        match trigger {
            PlanTrigger::WriteDone => {
                if self.state.plan.is_ready() {
                    return;
                }
                self.state.plan.mark_ready();
                let path = self.state.plan.path().map(|p| p.display().to_string());
                self.offer_plan_form(path.as_deref());
                // The path is the whole point here, so with no path we stay
                // quiet instead of handing a plugin an empty one to open.
                if let Some(path) = path {
                    self.fire_session_event(HostEvent::PlanReady { path: &path });
                }
            }
            PlanTrigger::InteractivePrompt => {
                if self.state.plan.is_ready() {
                    self.state.plan.mark_drafting();
                    self.plan_form.on_plan_drafting();
                }
            }
        }
    }

    /// A remote agent keeps its own plan file, so only the mode is set.
    pub(super) fn enter_plan(&mut self) {
        self.state.mode = Mode::Plan;
    }

    /// The mode switch behind `maki.session.set_mode`, i.e. what Tab does
    /// without the toggling. Build mode is what makes the next prompt an
    /// implementation of the plan instead of another draft.
    pub(crate) fn set_mode(&mut self, mode: Mode) {
        match mode {
            Mode::Plan => self.enter_plan(),
            Mode::Build => self.state.mode = Mode::Build,
        }
    }

    pub(super) fn toggle_mode(&mut self) -> Vec<super::Action> {
        if !self.backend.supports(Feature::ModeChange) {
            self.flash(UNAVAILABLE_REMOTE.into());
            return vec![];
        }
        match self.state.mode {
            Mode::Build => self.enter_plan(),
            Mode::Plan => self.state.mode = Mode::Build,
        };
        vec![]
    }

    /// A remote prompt sends only the mode id, so its plan path stays empty.
    pub(crate) fn agent_mode(&self) -> AgentMode {
        match self.state.mode {
            Mode::Plan => AgentMode::Plan(PathBuf::new()),
            Mode::Build => AgentMode::Build,
        }
    }

    /// Background subagents are capped by this same value, so they can never
    /// disagree with the chat's next prompt.
    pub(crate) fn request_options(&self) -> RequestOptions {
        RequestOptions {
            thinking: self.state.thinking,
            fast: self.state.fast,
        }
    }

    pub(crate) fn build_agent_input(&self, msg: &QueuedMessage) -> AgentInput {
        let RequestOptions { thinking, fast } = self.request_options();
        AgentInput {
            message: msg.text.clone(),
            mode: self.agent_mode(),
            images: msg.images.clone(),
            preamble: Vec::new(),
            earlier: Vec::new(),
            thinking,
            fast,
            workflow: self.state.workflow,
            prompt: None,
            source: InputSource::Tui,
        }
    }

    pub(super) fn mode_label(&self) -> (Cow<'static, str>, Style) {
        let label: Cow<'static, str> = if self.is_bash_input() {
            "[BASH]".into()
        } else {
            match self.state.mode {
                Mode::Build => "[BUILD]".into(),
                Mode::Plan => "[PLAN]".into(),
            }
        };
        let style = Style::new()
            .fg(self.effective_mode_color())
            .add_modifier(Modifier::BOLD);
        (label, style)
    }

    pub(crate) fn is_bash_input(&self) -> bool {
        self.input_box.buffer.value().starts_with('!')
    }

    pub(super) fn effective_mode_color(&self) -> Color {
        if self.is_bash_input() {
            theme::current().mode_bash
        } else {
            self.state.mode.color()
        }
    }

    pub(super) fn separator_style(&self) -> Style {
        if self.status == Status::Streaming {
            theme::current().input_border
        } else {
            Style::new().fg(self.effective_mode_color())
        }
    }
}
