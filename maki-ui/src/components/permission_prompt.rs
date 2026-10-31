use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent};
use maki_config::ToolKey;
use maki_types::event::PermissionChoice;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::components::form::render_form;
use crate::components::{Overlay, hint_line, is_ctrl};
use crate::theme;

const QUEUED_NOTICE: &str = "more request(s) waiting";
const REMOTE_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "Move"),
    ("Enter", "Choose"),
    ("1-9", "Pick"),
    ("Esc", "Cancel"),
];
const MAX_REMOTE_CHOICES: usize = 9;
const REMOTE_CURSOR: &str = ">";

struct Request {
    id: String,
    tool: ToolKey,
    scopes: Vec<String>,
    subagent_id: Option<String>,
    choices: Vec<PermissionChoice>,
}

/// An answer carries the ask it settles: the agent only accepts one naming the
/// request it is parked on, and the ask on screen is not always the last one in.
#[derive(Debug, PartialEq, Eq)]
pub struct AnsweredRequest {
    pub id: String,
    pub answer: Option<String>,
}

/// Every request parks a tool call until it is answered, so asks that arrive
/// while one is on screen queue up behind it instead of replacing it.
pub struct PermissionPrompt {
    queue: VecDeque<Request>,
    /// The highlighted remote option, so every option is reachable however
    /// many the agent offers.
    cursor: usize,
}

impl Overlay for PermissionPrompt {
    fn is_prompting(&self) -> bool {
        self.is_open()
    }

    fn is_open(&self) -> bool {
        !self.queue.is_empty()
    }

    fn is_modal(&self) -> bool {
        false
    }

    /// Only reached by a cancel or a session reset, and the prompt takes every
    /// key while it holds an ask, so neither runs over one: an ask from a task
    /// that outlives them would wait on an answer that never comes.
    fn close(&mut self) {
        self.queue.clear();
        self.reset_entry();
    }
}

impl PermissionPrompt {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            cursor: 0,
        }
    }

    /// An ask from a remote agent, answered with one of its own options.
    pub fn push_choices(
        &mut self,
        id: String,
        tool: ToolKey,
        scopes: Vec<String>,
        choices: Vec<PermissionChoice>,
        subagent_id: Option<String>,
    ) {
        self.queue.push_back(Request {
            id,
            tool,
            scopes,
            subagent_id,
            choices,
        });
    }

    fn reset_entry(&mut self) {
        self.cursor = 0;
    }

    /// A cancelled subagent has nothing left to answer with, so its asks leave
    /// with it instead of parking the panel on a dead tool call.
    pub fn drop_subagent(&mut self, subagent_id: &str) {
        let owns = |request: &Request| request.subagent_id.as_deref() == Some(subagent_id);
        if self.queue.front().is_some_and(owns) {
            self.reset_entry();
        }
        self.queue.retain(|request| !owns(request));
    }

    pub(crate) fn is_asking(&self, id: &str) -> bool {
        self.queue.iter().any(|request| request.id == id)
    }

    pub(crate) fn tool(&self) -> Option<&ToolKey> {
        self.queue.front().map(|request| &request.tool)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<AnsweredRequest> {
        let answer = self.key_answer(key)?;
        let request = self.queue.pop_front()?;
        self.reset_entry();
        Some(AnsweredRequest {
            id: request.id,
            answer,
        })
    }

    fn key_answer(&mut self, key: KeyEvent) -> Option<Option<String>> {
        let choices = &self.queue.front()?.choices;
        if (is_ctrl(&key) && key.code == KeyCode::Char('c')) || key.code == KeyCode::Esc {
            return Some(None);
        }
        let last = choices.len().checked_sub(1)?;
        let index = match key.code {
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                return None;
            }
            KeyCode::Down => {
                self.cursor = (self.cursor + 1).min(last);
                return None;
            }
            KeyCode::Enter => self.cursor,
            KeyCode::Char(digit) => {
                let index = (digit.to_digit(10)? as usize).checked_sub(1)?;
                (index < MAX_REMOTE_CHOICES).then_some(index)?
            }
            _ => return None,
        };
        choices.get(index).map(|choice| Some(choice.id.clone()))
    }

    pub fn handle_paste(&mut self, _text: &str) -> bool {
        self.is_open()
    }

    fn build_lines(&self) -> Vec<Line<'static>> {
        self.build_lines_with_cursor().0
    }

    /// Also the index of the line under a remote cursor, which the view keeps
    /// on screen.
    fn build_lines_with_cursor(&self) -> (Vec<Line<'static>>, Option<usize>) {
        let Some(Request {
            tool,
            scopes,
            subagent_id,
            choices,
            ..
        }) = self.queue.front()
        else {
            return (Vec::new(), None);
        };
        let t = theme::current();
        let label_style = t.tool_dim;
        let value_style = Style::new().fg(t.foreground);

        let mut tool_spans = vec![Span::raw("  "), Span::styled("tool  ", label_style)];
        if subagent_id.is_some() {
            tool_spans.push(Span::styled("[subtask] ", t.item_desc));
        }
        tool_spans.push(Span::styled(tool.to_string(), value_style));

        let mut lines = vec![Line::raw(""), Line::from(tool_spans)];

        let waiting = self.queue.len() - 1;
        if waiting > 0 {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("queue ", label_style),
                Span::styled(format!("{waiting} {QUEUED_NOTICE}"), t.item_desc),
            ]));
        }
        for (i, s) in scopes.iter().enumerate() {
            let label = if i == 0 { "scope " } else { "    + " };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(label, label_style),
                Span::styled(s.clone(), value_style),
            ]));
        }

        if !choices.is_empty() {
            lines.push(Line::raw(""));
            let cursor_line = lines.len() + self.cursor;
            for (index, choice) in choices.iter().enumerate() {
                let marker = if index == self.cursor {
                    REMOTE_CURSOR
                } else {
                    " "
                };
                let key = if index < MAX_REMOTE_CHOICES {
                    (index + 1).to_string()
                } else {
                    " ".into()
                };
                let style = if choice.allow {
                    t.keybind_key
                } else {
                    t.tool_dim
                };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {marker}{key}"), style),
                    Span::styled(format!(" {}", choice.label), value_style),
                ]));
            }
            lines.push(Line::raw(""));
            lines.push(hint_line(REMOTE_HINTS));
            lines.push(Line::raw(""));
            return (lines, Some(cursor_line));
        }

        lines.push(hint_line(REMOTE_HINTS));
        (lines, None)
    }

    pub fn view(&self, frame: &mut Frame, area: Rect) {
        if !self.is_open() {
            return;
        }
        let (lines, cursor_line) = self.build_lines_with_cursor();
        let scroll = cursor_line.map_or(0, |line| scroll_to_show(&lines, line, area));
        let t = theme::current();
        render_form(&t, " Permission Required ", frame, area, lines, (scroll, 0));
    }

    pub fn height(&self, width: u16) -> u16 {
        let inner_width = width.saturating_sub(2);
        let lines = self.build_lines();
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        para.line_count(inner_width) as u16 + 2
    }
}

/// The smallest scroll that keeps `line` on screen, counting wrapped rows,
/// since a long label wraps inside the bordered form.
fn scroll_to_show(lines: &[Line<'static>], line: usize, area: Rect) -> u16 {
    let width = area.width.saturating_sub(2);
    let visible = area.height.saturating_sub(2);
    let rows_through: usize = lines
        .iter()
        .take(line + 1)
        .map(|l| {
            Paragraph::new(l.clone())
                .wrap(Wrap { trim: false })
                .line_count(width)
        })
        .sum();
    u16::try_from(rows_through)
        .unwrap_or(u16::MAX)
        .saturating_sub(visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn rendered(prompt: &PermissionPrompt) -> String {
        prompt
            .build_lines()
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn no_offered_choices_can_only_be_cancelled() {
        let mut prompt = remote_prompt(0);
        assert!(prompt.handle_key(key(KeyCode::Enter)).is_none());
        assert!(prompt.handle_key(key(KeyCode::Char('1'))).is_none());
        let answer = prompt.handle_key(key(KeyCode::Esc)).unwrap();
        assert_eq!(answer.id, REMOTE_ID);
        assert_eq!(answer.answer, None);
    }

    const REMOTE_ID: &str = "remote-ask-0";
    const REMOTE_SCOPE: &str = "bash: rm -rf build";
    const MANY_CHOICES: usize = 10;

    fn choice(index: usize) -> PermissionChoice {
        PermissionChoice {
            id: format!("option-{index}"),
            label: format!("Label {index}"),
            allow: index.is_multiple_of(2),
        }
    }

    fn remote_prompt(count: usize) -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        prompt.push_choices(
            REMOTE_ID.into(),
            ToolKey::native("bash"),
            vec![REMOTE_SCOPE.into()],
            (0..count).map(choice).collect(),
            None,
        );
        prompt
    }

    fn remote_answer(prompt: &mut PermissionPrompt, code: KeyCode) -> Option<Option<String>> {
        prompt.handle_key(key(code)).map(|answered| answered.answer)
    }

    #[test]
    fn remote_ask_shows_the_agents_labels_and_no_local_rows() {
        let prompt = remote_prompt(2);
        let text = rendered(&prompt);
        assert!(
            text.contains("Label 0") && text.contains("Label 1"),
            "{text}"
        );
        assert!(!text.contains("Always (project)"), "{text}");
        assert!(!text.contains("Deny-always"), "{text}");
    }

    #[test]
    fn digit_picks_that_option() {
        let mut prompt = remote_prompt(2);
        assert_eq!(
            remote_answer(&mut prompt, KeyCode::Char('2')),
            Some(Some(choice(1).id))
        );
        assert!(!prompt.is_open());
    }

    #[test]
    fn every_option_is_reachable_with_the_cursor() {
        let mut prompt = remote_prompt(MANY_CHOICES);
        for _ in 1..MANY_CHOICES {
            assert_eq!(remote_answer(&mut prompt, KeyCode::Down), None);
        }
        assert_eq!(
            remote_answer(&mut prompt, KeyCode::Down),
            None,
            "stops at the end"
        );
        assert_eq!(
            remote_answer(&mut prompt, KeyCode::Enter),
            Some(Some(choice(MANY_CHOICES - 1).id))
        );
    }

    /// A short panel scrolls so the highlighted option is the one on screen.
    #[test]
    fn cursor_option_stays_visible_in_a_short_panel() {
        const WIDTH: u16 = 40;
        const HEIGHT: u16 = 8;
        let mut prompt = remote_prompt(MANY_CHOICES);
        for _ in 1..MANY_CHOICES {
            remote_answer(&mut prompt, KeyCode::Down);
        }
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| prompt.view(frame, frame.area()))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let last = choice(MANY_CHOICES - 1).label;
        assert!(screen.contains(&last), "{screen}");
        assert!(!screen.contains(&choice(0).label), "{screen}");
    }

    #[test]
    fn esc_cancels_a_remote_ask() {
        let mut prompt = remote_prompt(2);
        assert_eq!(remote_answer(&mut prompt, KeyCode::Esc), Some(None));
    }

    #[test_case(KeyCode::Char('5') ; "digit_beyond_the_list")]
    #[test_case(KeyCode::Char('y') ; "local_allow_key")]
    #[test_case(KeyCode::Char('0') ; "zero")]
    fn other_keys_leave_the_remote_ask_open(code: KeyCode) {
        let mut prompt = remote_prompt(2);
        assert_eq!(remote_answer(&mut prompt, code), None);
        assert!(prompt.is_open());
    }
}
