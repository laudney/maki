//! Shared configuration state and the agent's model and preference menus.

use std::sync::{Arc, Mutex, MutexGuard};

use super::remote_models::{ModelMenu, ModelPick};
use agent_client_protocol_schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigSelectOptions,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use maki_wire::client::Confirmed;
use maki_wire::configuration::{Change, Configuration, Key};
use maki_wire::settings::{FAST_CONFIG_ID, THINKING_CONFIG_ID, WORKFLOW_CONFIG_ID, YOLO_CONFIG_ID};
use serde_json::Value;

pub(crate) use maki_wire::configuration::REPLACED as SESSION_REPLACED;
#[cfg(test)]
pub(crate) use maki_wire::configuration::SUPERSEDED as SETTING_SUPERSEDED;
pub(crate) use maki_wire::configuration::UNOFFERED as SETTING_UNOFFERED;

/// The agent's answer to one change: `Ok` once it holds the value.
pub(crate) type SettingReply = maki_wire::configuration::Reply;

pub(crate) fn unoffered(setting: Setting) -> String {
    format!("{SETTING_UNOFFERED}: {}", setting.id())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Setting {
    Thinking,
    Fast,
    Workflow,
    Yolo,
}

impl Setting {
    pub(crate) const ALL: [Self; 4] = [Self::Thinking, Self::Fast, Self::Workflow, Self::Yolo];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Thinking => THINKING_CONFIG_ID,
            Self::Fast => FAST_CONFIG_ID,
            Self::Workflow => WORKFLOW_CONFIG_ID,
            Self::Yolo => YOLO_CONFIG_ID,
        }
    }

    /// These three ride on the prompt, so changing them needs the settings
    /// extension; yolo is an option of its own.
    pub(crate) fn rides_on_the_prompt(self) -> bool {
        self != Self::Yolo
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ThinkingMenu {
    pub(crate) current: String,
    /// Each value with what the agent's model makes of it.
    pub(crate) choices: Vec<(String, String)>,
}

/// What the agent offers and holds. An absent setting is not offered: the
/// agent leaves out fast for a model that cannot do it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SettingsMenu {
    pub(crate) thinking: Option<ThinkingMenu>,
    pub(crate) fast: Option<bool>,
    pub(crate) workflow: Option<bool>,
    pub(crate) yolo: Option<bool>,
}

impl SettingsMenu {
    pub(crate) fn from_options(options: &[SessionConfigOption]) -> Self {
        let find = |id: &str| options.iter().find(|option| option.id.to_string() == id);
        let toggle = |id: &str| match &find(id)?.kind {
            SessionConfigKind::Boolean(boolean) => Some(boolean.current_value),
            _ => None,
        };
        let thinking = find(THINKING_CONFIG_ID).and_then(|option| {
            let SessionConfigKind::Select(select) = &option.kind else {
                return None;
            };
            let SessionConfigSelectOptions::Ungrouped(choices) = &select.options else {
                return None;
            };
            Some(ThinkingMenu {
                current: select.current_value.to_string(),
                choices: choices
                    .iter()
                    .map(|choice| (choice.value.to_string(), choice.name.clone()))
                    .collect(),
            })
        });
        Self {
            thinking,
            fast: toggle(FAST_CONFIG_ID),
            workflow: toggle(WORKFLOW_CONFIG_ID),
            yolo: toggle(YOLO_CONFIG_ID),
        }
    }

    pub(crate) fn value(&self, setting: Setting) -> Option<Value> {
        match setting {
            Setting::Thinking => self
                .thinking
                .as_ref()
                .map(|menu| Value::String(menu.current.clone())),
            Setting::Fast => self.fast.map(Value::Bool),
            Setting::Workflow => self.workflow.map(Value::Bool),
            Setting::Yolo => self.yolo.map(Value::Bool),
        }
    }

    /// The name the agent gave `value`, which is what its model makes of it.
    pub(crate) fn thinking_label(&self, value: &str) -> Option<&str> {
        self.thinking
            .as_ref()?
            .choices
            .iter()
            .find_map(|(choice, name)| (choice == value).then_some(name.as_str()))
    }
}

/// The preferences shown by the UI; revisions live in the coordinator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Intent {
    pub(crate) thinking: String,
    pub(crate) fast: bool,
    pub(crate) workflow: bool,
    pub(crate) yolo: bool,
}

/// Shared configuration state and its UI projections. Model picks wait for
/// a prompt; preferences can be sent while it runs. Both use the same
/// revisions and confirmation records.
#[derive(Clone)]
pub struct RemoteSettings {
    pub(crate) models: Arc<ArcSwapOption<ModelMenu>>,
    pub(crate) menu: Arc<ArcSwap<SettingsMenu>>,
    pub(crate) intent: Arc<ArcSwap<Intent>>,
    pub(crate) failure: Arc<ArcSwapOption<String>>,
    pub(crate) wake: flume::Sender<()>,
    woken: flume::Receiver<()>,
    core: Arc<Mutex<Configuration>>,
}

impl RemoteSettings {
    pub fn from_options(options: &[SessionConfigOption]) -> Self {
        let menu = SettingsMenu::from_options(options);
        let mut core = Configuration::new(
            Confirmed {
                config_options: options.to_vec(),
                mode: None,
            },
            0,
        );
        for setting in Setting::ALL {
            let value = menu.value(setting).unwrap_or_else(|| match setting {
                Setting::Thinking => Value::String(String::new()),
                _ => Value::Bool(false),
            });
            core.seed(setting.key(), value);
        }
        let (wake, woken) = flume::bounded(1);
        let state = Self {
            models: Arc::default(),
            menu: Arc::default(),
            intent: Arc::default(),
            failure: Arc::default(),
            wake,
            woken,
            core: Arc::new(Mutex::new(core)),
        };
        state.refresh(&state.lock());
        state
    }

    pub(crate) fn woken(&self) -> flume::Receiver<()> {
        self.woken.clone()
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Configuration> {
        self.core.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn refresh(&self, core: &Configuration) {
        let models = ModelMenu::from_options(&core.confirmed.config_options);
        if self.models.load_full().as_deref() != models.as_ref() {
            self.models.store(models.map(Arc::new));
        }
        let menu = SettingsMenu::from_options(&core.confirmed.config_options);
        if **self.menu.load() != menu {
            self.menu.store(Arc::new(menu));
        }
        let value = |setting: Setting| core.wanted(&setting.key()).cloned().unwrap_or_default();
        let intent = Intent {
            thinking: value(Setting::Thinking)
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            fast: value(Setting::Fast).as_bool().unwrap_or_default(),
            workflow: value(Setting::Workflow).as_bool().unwrap_or_default(),
            yolo: value(Setting::Yolo).as_bool().unwrap_or_default(),
        };
        if **self.intent.load() != intent {
            self.intent.store(Arc::new(intent));
        }
    }

    pub(crate) fn publish(&self, options: &[SessionConfigOption], seq: u64) {
        let mut core = self.lock();
        core.publish_options(options.to_vec(), seq);
        self.refresh(&core);
        core.settle_held(|key| self.offered(key));
    }

    pub(crate) fn offered(&self, key: &Key) -> bool {
        Setting::ALL
            .into_iter()
            .any(|setting| setting.key() == *key && self.menu.load().value(setting).is_some())
    }

    pub(crate) fn change(&self, setting: Setting, value: Value) -> SettingReply {
        let mut core = self.lock();
        if self.menu.load().value(setting).is_none() {
            let (reply, answer) = flume::bounded(1);
            let _ = reply.send(Err(unoffered(setting)));
            return answer;
        }
        let answer = core.request(setting.key(), value);
        self.refresh(&core);
        let _ = self.wake.try_send(());
        answer
    }

    pub(crate) fn wanted(&self) -> Option<Arc<ModelPick>> {
        let change = self.lock().deferred_change()?;
        let Key::Option(config_id) = change.key else {
            return None;
        };
        Some(Arc::new(ModelPick {
            config_id,
            value: change.value.as_str()?.to_owned(),
        }))
    }

    pub(crate) fn pick(&self, pick: ModelPick) {
        self.lock()
            .desire(Key::Option(pick.config_id), pick.value.into(), true);
    }

    pub(crate) fn take_pick(&self) -> Option<Change> {
        let mut core = self.lock();
        let key = core.deferred_change()?.key;
        core.take_deferred(&key)
    }
}

impl Setting {
    pub(crate) fn key(self) -> Key {
        Key::Option(self.id().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::SessionConfigSelectOption;

    use super::*;

    fn options() -> Vec<SessionConfigOption> {
        vec![
            SessionConfigOption::select(
                THINKING_CONFIG_ID,
                "Thinking",
                "adaptive".to_owned(),
                vec![
                    SessionConfigSelectOption::new("off".to_owned(), "off".to_owned()),
                    SessionConfigSelectOption::new("adaptive".to_owned(), "high".to_owned()),
                ],
            ),
            SessionConfigOption::boolean(WORKFLOW_CONFIG_ID, "Workflow", true),
            SessionConfigOption::boolean(YOLO_CONFIG_ID, "Yolo", false),
        ]
    }

    #[test]
    fn the_menu_reads_each_option_and_leaves_absent_ones_out() {
        let menu = SettingsMenu::from_options(&options());
        assert_eq!(
            menu.value(Setting::Thinking),
            Some(Value::String("adaptive".into()))
        );
        assert_eq!(menu.fast, None, "the agent did not offer fast");
        assert_eq!(menu.workflow, Some(true));
        assert_eq!(menu.yolo, Some(false));
        assert_eq!(menu.thinking_label("adaptive"), Some("high"));
        assert_eq!(menu.thinking_label("budget"), None);
    }
}
