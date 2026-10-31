//! The model picker for a remote agent: the agent's own names and values,
//! with none of the local picker's provider lookups, pricing or tiers.

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::agent::remote_models::{ModelChoice, ModelMenu, ModelPick};
use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::repaint::Cadence;

const TITLE: &str = " Models ";
const MAX_VISIBLE: u16 = 15;

pub enum RemoteModelPickerAction {
    Consumed,
    /// The model to ask the agent for, named by the option it came from.
    Select(ModelPick),
    Closed,
}

impl PickerItem for ModelChoice {
    fn label(&self) -> &str {
        &self.name
    }

    fn detail(&self) -> Option<&str> {
        (self.value != self.name).then_some(self.value.as_str())
    }
}

pub struct RemoteModelPicker {
    picker: ListPicker<ModelChoice>,
    /// The option the shown choices came from.
    config_id: String,
}

impl RemoteModelPicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new().with_max_visible(MAX_VISIBLE),
            config_id: String::new(),
        }
    }

    pub fn open(&mut self, menu: &ModelMenu) {
        self.config_id.clone_from(&menu.config_id);
        self.picker.open(menu.choices.clone(), TITLE);
        if let Some(current) = &menu.current {
            self.picker
                .select_item_by(|choice| &choice.value == current);
        }
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> RemoteModelPickerAction {
        match self.picker.handle_key(key) {
            PickerAction::Select(choice) => RemoteModelPickerAction::Select(ModelPick {
                config_id: self.config_id.clone(),
                value: choice.value,
            }),
            PickerAction::Close => RemoteModelPickerAction::Closed,
            PickerAction::Consumed | PickerAction::Toggle(..) => RemoteModelPickerAction::Consumed,
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

impl Overlay for RemoteModelPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }

    fn is_prompting(&self) -> bool {
        false
    }

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;
    use crate::components::key;

    const FIRST: &str = "vendor/first";
    const SECOND: &str = "vendor/second";
    const SECOND_NAME: &str = "Second";

    fn menu() -> ModelMenu {
        ModelMenu {
            config_id: "model".into(),
            current: Some(SECOND.into()),
            choices: vec![
                ModelChoice {
                    value: FIRST.into(),
                    name: FIRST.into(),
                },
                ModelChoice {
                    value: SECOND.into(),
                    name: SECOND_NAME.into(),
                },
            ],
        }
    }

    #[test]
    fn opens_on_the_current_model_and_picks_its_value() {
        let mut picker = RemoteModelPicker::new();
        picker.open(&menu());
        let RemoteModelPickerAction::Select(pick) = picker.handle_key(key(KeyCode::Enter)) else {
            panic!("Enter picks the selected model");
        };
        assert_eq!(pick.value, SECOND);
        assert_eq!(pick.config_id, menu().config_id);
    }

    #[test]
    fn detail_shows_the_value_only_when_it_differs() {
        let menu = menu();
        assert_eq!(menu.choices[0].detail(), None);
        assert_eq!(menu.choices[1].detail(), Some(SECOND));
    }

    #[test]
    fn esc_closes_without_a_pick() {
        let mut picker = RemoteModelPicker::new();
        picker.open(&menu());
        assert!(matches!(
            picker.handle_key(key(KeyCode::Esc)),
            RemoteModelPickerAction::Closed
        ));
        assert!(!picker.is_open());
    }
}
