//! A remote tab's thinking, fast, workflow and yolo. The App owns the
//! user's intent, as locally, and seeds it from the agent so a prompt never
//! carries the client's defaults over the agent's own; the agent clamps and
//! reports what is effective.

use serde_json::Value;
use std::borrow::Cow;

use maki_storage::sessions::StoredThinking;
use maki_types::thinking::ThinkingConfig;

use super::{App, FAST_OFF_MSG, FAST_ON_MSG, WORKFLOW_OFF_MSG, WORKFLOW_ON_MSG};
use crate::agent::capabilities::UNAVAILABLE_REMOTE;
use crate::agent::remote_settings::{Intent, Setting};
use crate::repaint::Dirty;

const YOLO_ON_MSG: &str = "YOLO mode enabled";
const YOLO_OFF_MSG: &str = "YOLO mode disabled";
/// The agent's name for a thinking value its model runs as off.
const OFF_LABEL: &str = "off";

impl App {
    /// Before any submission, so even the first prompt carries the agent's
    /// values. Nothing is clamped against the placeholder display model.
    pub(crate) fn seed_remote_settings(&mut self) {
        let Some(settings) = self.remote_settings.clone() else {
            return;
        };
        let intent = settings.intent.load_full();
        self.adopt(&intent);
        let _ = self
            .remote_settings_drawn
            .poll(Some(settings.menu.load_full()));
        let _ = self.remote_intent.poll(intent);
    }

    /// `/fast`, `/workflow` and `/yolo` on a remote tab. The first two ride
    /// on the prompt, so they need the settings extension too.
    pub(super) fn toggle_remote_setting(&mut self, setting: Setting) {
        let Some(settings) = self.remote_settings.clone() else {
            self.flash(UNAVAILABLE_REMOTE.into());
            return;
        };
        let offered = settings.menu.load().value(setting).is_some();
        if !offered || (setting.rides_on_the_prompt() && !self.backend.takes_settings()) {
            self.flash(UNAVAILABLE_REMOTE.into());
            return;
        }
        let (on, message) = match setting {
            Setting::Fast => {
                self.state.fast = !self.state.fast;
                let on = self.state.fast;
                (on, if on { FAST_ON_MSG } else { FAST_OFF_MSG })
            }
            Setting::Workflow => {
                self.state.workflow = !self.state.workflow;
                let on = self.state.workflow;
                (
                    on,
                    if on {
                        WORKFLOW_ON_MSG
                    } else {
                        WORKFLOW_OFF_MSG
                    },
                )
            }
            Setting::Yolo => {
                let on = self.yolo.toggle_yolo();
                (on, if on { YOLO_ON_MSG } else { YOLO_OFF_MSG })
            }
            // `/thinking` comes with the UI Lua runtime.
            Setting::Thinking => return,
        };
        settings.change(setting, Value::Bool(on));
        self.flash(message.into());
    }

    /// The menu repaints an idle tab. The loop puts a refused change back
    /// in the intent, so the App adopts the intent whenever it moves, and
    /// says why from the failure slot, whatever run is current.
    pub(super) fn poll_remote_settings(&mut self) -> Dirty {
        let Some(settings) = self.remote_settings.clone() else {
            return Dirty::NO;
        };
        let menu = self
            .remote_settings_drawn
            .poll(Some(settings.menu.load_full()));
        let intent = self.remote_intent.poll(settings.intent.load_full());
        if intent != Dirty::NO
            && let Some(intent) = self.remote_intent.get().cloned()
        {
            self.adopt(&intent);
        }
        let failure = self.remote_failure.poll(settings.failure.load_full());
        if failure != Dirty::NO
            && let Some(reason) = self.remote_failure.get().cloned()
        {
            self.flash(reason);
        }
        menu | intent | failure
    }

    fn adopt(&mut self, intent: &Intent) {
        self.state.thinking = StoredThinking::parse_setting(&intent.thinking)
            .map(ThinkingConfig::from)
            .unwrap_or_default();
        self.state.fast = intent.fast;
        self.state.workflow = intent.workflow;
        if self.yolo.is_yolo() != intent.yolo {
            self.yolo.set_session_yolo(Some(intent.yolo));
        }
    }

    /// The status bar's four indicators for a remote tab: what the agent's
    /// model makes of the intent, each only while the agent offers it.
    pub(super) fn remote_indicators(&self) -> RemoteIndicators {
        let Some(menu) = self.remote_settings_drawn.get() else {
            return RemoteIndicators::default();
        };
        let thinking = menu.thinking.as_ref().and_then(|_| {
            let intent = self.state.thinking.to_string();
            match menu.thinking_label(&intent) {
                Some(label) => (label != OFF_LABEL).then(|| Cow::Owned(label.to_owned())),
                None => self.state.thinking.status_label(),
            }
        });
        RemoteIndicators {
            thinking,
            fast: menu.fast.is_some() && self.state.fast,
            workflow: menu.workflow.is_some() && self.state.workflow,
            yolo: menu.yolo.is_some() && self.yolo.is_yolo(),
        }
    }
}

#[derive(Default)]
pub(super) struct RemoteIndicators {
    pub(super) thinking: Option<Cow<'static, str>>,
    pub(super) fast: bool,
    pub(super) workflow: bool,
    pub(super) yolo: bool,
}
