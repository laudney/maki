mod render;
mod route;
mod scroll;
mod segment;
mod selection;
#[cfg(test)]
mod tests;

pub(crate) use self::route::{CallData, RenderPlace};
pub use self::scroll::ScrollPos;

use self::render::RenderCursor;
use self::route::{RenderJob, RoutedCall, draws_from_data};
use self::scroll::Layout;
use self::segment::{Segment, SegmentCache};

use super::tool_display::{
    MOVABLE_HINT, RenderCtx, RoleStyle, ToolLines, append_annotation, append_right_info,
    assistant_style, build_instructions_lines, build_tool_lines, done_style, error_style,
    format_timestamp_now, hint_columns, instructions_search_text, notice_style, outcome_glyph,
    sanitize_lines, search_text_for, thinking_style, truncate_to_header, user_style,
};
use super::{
    BackgroundBadge, BackgroundOutcome, DisplayMessage, DisplayRole, IMAGE_PLACEHOLDER, ToolRole,
    ToolStatus, code_view::SectionFlags,
};
use crate::animation::spinner_str;
use crate::components::keybindings::key;
use crate::markdown::{hr_line, plain_lines, text_to_lines, truncate_output};
use crate::render_worker::RenderWorker;
use crate::selection::{DocPos, RowPos, Selection};
use crate::splash::{ColorTransition, Splash};
use crate::{terminal_image, theme, update};
use maki_config::{ClockFormat, ThinkingLines, ToolOutputLines, UiConfig};
use ratatui_image::picker::Picker;

use std::collections::{HashMap, HashSet};
use std::mem;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::scrollbar::{self, render_vertical_scrollbar};
use super::streaming_content::StreamingContent;
use super::thinking;
use crate::EventHandle;
use maki_types::event::{ToolDoneEvent, ToolStartEvent};
use maki_types::message::ImageSource;
use maki_types::snapshot::BufferSnapshot;
use maki_types::tool::{InstructionBlock, NO_FILES_FOUND, ToolOutput};
struct SessionPlace {
    cwd: std::path::PathBuf,
    home: Option<std::path::PathBuf>,
}
use maki_lua_ui::{LuaCommandInfo, RenderReason, RenderRoute, WinView};
use maki_storage::id::{MakiId, SessionRef};

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::repaint::{Cadence, Dirty};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

const REFLOW_MARGIN_VIEWPORTS: u32 = 1;
/// How far outside the drawn range an image keeps its encoded protocol.
const IMAGE_KEEP_MARGIN_SEGMENTS: usize = 8;
/// A click on transcript thinking expands or collapses it.
const CLICKABLE: bool = true;
const NOTICE_COLLAPSED_MARK: &str = " ▸";
const NOTICE_EXPANDED_MARK: &str = " ▾";
const NOTICE_DETAIL_INDENT: &str = "  ";

/// What a click in the transcript asks of the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickOutcome {
    None,
    Handled,
    /// A moved row: open its task tab.
    OpenTask(String),
    /// A running row's hint: move only this call.
    MoveCall(String),
}

#[derive(Clone, Copy)]
pub struct PromptProgress {
    pub processed: u32,
    pub total: u32,
    pub cache: u32,
}

pub struct MessagesPanel {
    messages: Vec<DisplayMessage>,
    streaming_thinking: StreamingContent,
    streaming_text: StreamingContent,
    started_at: Instant,
    scroll: ScrollPos,
    auto_scroll: bool,
    viewport_height: u16,
    viewport_width: u16,
    cache: SegmentCache,
    hl_worker: RenderWorker,
    image_picker: Option<Picker>,
    inline_images: bool,
    image_generation: u64,
    theme_generation: u64,
    highlight_segment: Option<usize>,
    idle_splash: Splash,
    accent: ColorTransition,
    expanded_tools: HashMap<String, SectionFlags>,
    /// Per-tool log of post-completion click rows, replayed on restore.
    lua_clicks: HashMap<String, Vec<usize>>,
    tool_output_lines: ToolOutputLines,
    lua_event_handle: EventHandle,
    thinking_lines: ThinkingLines,
    /// The reader's click on the live thinking, handed to its message by
    /// [`Self::flush_thinking`].
    live_thinking_expanded: bool,
    clock_format: ClockFormat,
    prompt_progress: Option<PromptProgress>,
    /// The chat this panel shows, stamped on every restore it requests so a
    /// plugin files the call where the live one went.
    session_id: Option<SessionRef>,
    task_id: Option<Arc<str>>,
    /// Where the session runs, which a restore draws its paths against.
    place: Option<SessionPlace>,
    /// Where the streaming segments start. `view` drops and rebuilds them
    /// every frame, so they always sit at the end of the cache.
    live_start: usize,
    /// Each call's route, by tool use id, see [`route`]. A call without one
    /// has no data and takes the legacy path.
    routes: HashMap<String, RoutedCall>,
    /// The renderer requests owed or in flight, by tool use id.
    render_jobs: HashMap<String, RenderJob>,
    /// The routing table generation the routes were last checked against.
    routes_seen: u64,
}

impl MessagesPanel {
    pub fn new(ui_config: UiConfig, lua_event_handle: EventHandle) -> Self {
        let routes_seen = lua_event_handle.ui.renderer_routes().generation();
        let thinking = thinking_style();
        let assistant = assistant_style();
        let ms = ui_config.typewriter_ms_per_char;
        Self {
            messages: Vec::new(),
            streaming_thinking: StreamingContent::new(
                thinking.prefix,
                thinking.text_style,
                thinking.prefix_style,
                ms,
            ),
            streaming_text: StreamingContent::new(
                assistant.prefix,
                assistant.text_style,
                assistant.prefix_style,
                ms,
            ),
            started_at: Instant::now(),
            scroll: ScrollPos::default(),
            auto_scroll: true,
            viewport_height: 24,
            viewport_width: crossterm::terminal::size().map_or(80, |(w, _)| w.saturating_sub(1)),
            cache: SegmentCache::new(),
            hl_worker: RenderWorker::new(),
            image_picker: terminal_image::picker(ui_config.inline_images),
            inline_images: ui_config.inline_images,
            image_generation: terminal_image::generation(),
            theme_generation: theme::generation(),
            highlight_segment: None,
            idle_splash: Splash::new(ui_config.splash_animation),
            accent: ColorTransition::new(theme::current().mode_build),
            expanded_tools: HashMap::new(),
            lua_clicks: HashMap::new(),
            tool_output_lines: ui_config.tool_output_lines,
            lua_event_handle,
            thinking_lines: ui_config.thinking_lines,
            live_thinking_expanded: false,
            clock_format: ui_config.clock_format,
            prompt_progress: None,
            session_id: None,
            task_id: None,
            place: None,
            live_start: 0,
            routes: HashMap::new(),
            render_jobs: HashMap::new(),
            routes_seen,
        }
    }

    pub(crate) fn set_chat(&mut self, session_id: MakiId, task_id: Option<Arc<str>>) {
        self.session_id = Some(SessionRef::from(session_id));
        self.task_id = task_id;
    }

    pub(crate) fn session_id(&self) -> Option<MakiId> {
        self.session_id.as_ref().map(SessionRef::id)
    }

    /// Keeps {place} for the restores this panel asks for.
    pub(crate) fn keep_place(&mut self, place: RenderPlace<'_>) {
        let kept = self
            .place
            .as_ref()
            .is_some_and(|kept| kept.cwd == place.cwd && kept.home.as_deref() == place.home);
        if !kept {
            self.place = Some(SessionPlace {
                cwd: place.cwd.to_path_buf(),
                home: place.home.map(Path::to_path_buf),
            });
        }
    }

    pub(crate) fn task_id(&self) -> Option<&Arc<str>> {
        self.task_id.as_ref()
    }

    /// Hands back the index of the message, which [`Self::replace`] needs to
    /// correct it later.
    pub fn push(&mut self, msg: DisplayMessage) -> usize {
        self.messages.push(msg);
        self.messages.len() - 1
    }

    /// Drops the whole segment cache, so keep it for one-off corrections and
    /// never for streaming. Marking the message stale is not enough: only the
    /// segments the viewport reaches get reflowed, so a fix above it would
    /// keep painting the old bubble.
    pub fn replace(&mut self, index: usize, msg: DisplayMessage) {
        let Some(slot) = self.messages.get_mut(index) else {
            return;
        };
        *slot = msg;
        self.cache.clear();
    }

    pub fn load_messages(&mut self, msgs: Vec<DisplayMessage>) {
        self.messages = msgs;
        self.cache.clear();
        self.expanded_tools.clear();
        self.lua_clicks.clear();
        self.routes.clear();
        self.render_jobs.clear();
        self.highlight_segment = None;
        self.live_thinking_expanded = false;
    }

    pub fn thinking_delta(&mut self, text: &str) {
        self.streaming_thinking.push(text);
    }

    pub fn text_delta(&mut self, text: &str) {
        self.flush_thinking();
        self.streaming_text.push(text);
    }

    /// Rows drawn from history were not run now, and history keeps no time,
    /// so they show none, and their renderers draw them as loaded, as a
    /// loaded session's rows do.
    pub(crate) fn forget_times(&mut self) {
        for msg in &mut self.messages {
            msg.timestamp = None;
        }
        self.draw_owed_as_load();
        self.cache.clear();
    }

    pub fn tool_pending(&mut self, id: String, name: &str) {
        self.flush();
        let role = DisplayRole::Tool(Box::new(ToolRole {
            id,
            status: ToolStatus::InProgress,
            name: Arc::from(name),
        }));
        let mut msg = DisplayMessage::new(role, String::new());
        msg.timestamp = Some(format_timestamp_now(self.clock_format));
        self.messages.push(msg);
    }

    pub fn tool_start(&mut self, event: ToolStartEvent) {
        self.route_start(&event);
        if let Some(msg) = self.find_tool_msg_mut(&event.id) {
            if let DisplayRole::Tool(t) = &mut msg.role {
                t.name = Arc::clone(&event.tool);
            }
            msg.text = event.summary;
            msg.tool_input = event.input.map(Arc::new);
            msg.tool_raw_input = event.raw_input.map(Arc::new);
            msg.tool_output = event.output.map(Arc::new);
            msg.annotation = event.annotation;
            self.rebuild_tool_segment(&event.id);
            return;
        }
        self.flush();
        let mut msg = DisplayMessage::new(
            DisplayRole::Tool(Box::new(ToolRole {
                id: event.id,
                status: ToolStatus::InProgress,
                name: Arc::clone(&event.tool),
            })),
            event.summary,
        );
        msg.tool_input = event.input.map(Arc::new);
        msg.tool_raw_input = event.raw_input.map(Arc::new);
        msg.tool_output = event.output.map(Arc::new);
        msg.annotation = event.annotation;
        msg.timestamp = Some(format_timestamp_now(self.clock_format));
        self.messages.push(msg);
    }

    pub fn tool_output(&mut self, tool_id: &str, content: &str) {
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let tool_name = msg.role.tool_name().unwrap_or("");
        truncate_to_header(&mut msg.text);
        let truncated = truncate_output(content, self.tool_output_lines.get(tool_name));
        msg.truncated_lines = truncated.skipped;
        msg.text.push('\n');
        msg.text.push_str(&truncated.kept);
        msg.live_output = Some(content.to_owned());
        self.rebuild_tool_segment(tool_id);
    }

    pub fn tool_done(&mut self, event: ToolDoneEvent) {
        self.route_done(&event.id, event.data.as_ref());
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == event.id))
        else {
            return;
        };
        if let DisplayRole::Tool(t) = &mut msg.role {
            t.status = if event.is_error {
                ToolStatus::Error
            } else {
                ToolStatus::Success
            };
        }
        msg.movable = false;
        if event.backgrounded && msg.background.is_none() {
            msg.background = Some(BackgroundBadge::moved(&event));
        }
        truncate_to_header(&mut msg.text);
        let done_annotation = event
            .annotation
            .as_deref()
            .map(str::to_owned)
            .or_else(|| event.output.annotation());
        if let Some(suffix) = &done_annotation {
            append_annotation(&mut msg.annotation, suffix);
        }

        match event.output.as_ref() {
            ToolOutput::Plain(text) | ToolOutput::Markdown(text) | ToolOutput::ReadDir(text)
                if msg.render_snapshot.is_none() =>
            {
                let tr = truncate_output(&text.text, self.tool_output_lines.get(&event.tool));
                msg.truncated_lines = tr.skipped;
                if !tr.kept.is_empty() {
                    msg.text = format!("{}\n{}", msg.text, tr.kept);
                }
            }
            ToolOutput::GrepResult { entries } if entries.is_empty() => {
                msg.text = format!("{}\n{NO_FILES_FOUND}", msg.text);
            }
            _ => {}
        }
        msg.tool_output = Some(event.output);
        msg.live_output = None;
        self.rebuild_tool_segment(&event.id);
    }

    /// Ends a row that no `ToolDone` will reach.
    pub fn settle_tool(&mut self, tool_id: &str, status: ToolStatus) {
        self.update_tool(tool_id, |msg| {
            if let DisplayRole::Tool(tool) = &mut msg.role {
                tool.status = status;
            }
        });
    }

    pub fn set_tool_movable(&mut self, tool_id: &str, movable: bool) {
        self.update_tool(tool_id, |msg| msg.movable = movable);
    }

    pub fn set_tool_background(&mut self, tool_id: &str, badge: BackgroundBadge) {
        self.update_tool(tool_id, |msg| msg.background = Some(badge));
    }

    pub fn set_background_outcome(&mut self, tool_id: &str, outcome: BackgroundOutcome) {
        self.update_tool(tool_id, |msg| {
            if let Some(badge) = &mut msg.background {
                badge.outcome = Some(outcome);
            }
        });
    }

    pub fn has_background_badge(&self, tool_id: &str) -> bool {
        self.find_tool_msg(tool_id)
            .is_some_and(|m| m.background.is_some())
    }

    pub fn background_badges(&self) -> Vec<BackgroundBadge> {
        self.messages
            .iter()
            .filter_map(|m| m.background.clone())
            .collect()
    }

    /// A copy of the tool's row, for a task tab that shows the same call.
    pub fn tool_message(&self, tool_id: &str) -> Option<DisplayMessage> {
        self.find_tool_msg(tool_id).cloned()
    }

    pub fn set_snapshot(&mut self, tool_id: &str, snapshot: BufferSnapshot) {
        self.update_tool(tool_id, |msg| msg.render_snapshot = Some(snapshot));
    }

    pub fn update_tool_summary(&mut self, tool_id: &str, summary: &str) {
        self.update_tool(tool_id, |msg| msg.text = summary.to_owned());
    }

    pub fn annotate_tool(&mut self, tool_id: &str, note: &str) {
        self.update_tool(tool_id, |msg| append_annotation(&mut msg.annotation, note));
    }

    /// A subagent stamps its own cumulative usage on the task header, and that
    /// header is usually the last tool of the turn, so an existing stamp wins.
    pub fn set_turn_usage_on_last_tool(&mut self, usage: String) {
        let last_tool = self.messages.iter().rev().find_map(|msg| match &msg.role {
            DisplayRole::Tool(tool) => Some((tool.id.clone(), msg.turn_usage.is_none())),
            _ => None,
        });
        if let Some((id, unstamped)) = last_tool
            && unstamped
        {
            self.set_tool_turn_usage(&id, usage);
        }
    }

    pub fn set_tool_turn_usage(&mut self, tool_id: &str, usage: String) {
        self.update_tool(tool_id, |msg| msg.turn_usage = Some(usage));
    }

    fn upsert_instruction_segment(&mut self, parent_id: &str, blocks: &[InstructionBlock]) {
        if blocks.is_empty() {
            return;
        }
        let inst_id = segment::instruction_id(parent_id);
        let Some(seg_idx) = self.cache.find_by_tool_id(&inst_id) else {
            return;
        };
        let exp = self
            .expanded_tools
            .get(&inst_id)
            .copied()
            .unwrap_or_default();
        let tl = build_instructions_lines(blocks, self.viewport_width, exp.output);

        // The spacer gets its line only now. Empty means no rows, so a tool
        // that never sends instructions leaves no gap behind.
        if let Some(spacer) = self.cache.get_mut(seg_idx - 1)
            && spacer.lines().is_empty()
        {
            spacer.set_lines(vec![Line::default()]);
        }
        let seg = self.cache.get_mut(seg_idx).unwrap();
        seg.update_with_reuse(tl, &self.hl_worker);
    }

    fn update_tool(&mut self, tool_id: &str, update_msg: impl FnOnce(&mut DisplayMessage)) {
        let Some(msg) = self.find_tool_msg_mut(tool_id) else {
            return;
        };
        update_msg(msg);
        self.rebuild_tool_segment(tool_id);
    }

    pub fn stream_reset(&mut self) {
        self.streaming_thinking.clear();
        self.streaming_text.clear();
        self.live_thinking_expanded = false;
        self.cancel_in_progress();
    }

    pub fn fail_in_progress_with_message(&mut self, message: String) {
        self.fail_in_progress_except(message, &HashSet::new());
    }

    pub fn fail_in_progress_except(&mut self, message: String, excluded: &HashSet<String>) {
        let ids: Vec<(String, Arc<str>)> = self
            .messages
            .iter()
            .filter_map(|m| {
                if let DisplayRole::Tool(t) = &m.role
                    && t.status == ToolStatus::InProgress
                    && !excluded.contains(&t.id)
                {
                    Some((t.id.clone(), Arc::clone(&t.name)))
                } else {
                    None
                }
            })
            .collect();
        for (id, tool) in ids {
            self.tool_done(ToolDoneEvent {
                call: None,
                id,
                tool,
                output: Arc::new(ToolOutput::Plain(message.clone().into())),
                is_error: true,
                annotation: None,
                written_path: None,
                data: None,

                backgrounded: false,
            });
        }
    }

    pub fn cancel_in_progress(&mut self) {
        let affected_ids: Vec<String> = self
            .messages
            .iter_mut()
            .filter_map(|msg| {
                if let DisplayRole::Tool(t) = &mut msg.role
                    && t.status == ToolStatus::InProgress
                {
                    t.status = ToolStatus::Error;
                    Some(t.id.clone())
                } else {
                    None
                }
            })
            .collect();

        for id in &affected_ids {
            self.owe_draw(id, RenderReason::Live);
            self.rebuild_tool_segment(id);
        }
    }

    pub fn in_progress_count(&self) -> usize {
        self.messages
            .iter()
            .filter(
                |m| matches!(&m.role, DisplayRole::Tool(t) if t.status == ToolStatus::InProgress),
            )
            .count()
    }

    #[cfg(test)]
    pub fn toggle_expansion(&mut self, tool_id: &str) -> bool {
        let Some(seg) = self
            .cache
            .segments()
            .iter()
            .find(|s| s.tool_id.as_deref() == Some(tool_id))
        else {
            return false;
        };
        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        if !seg.truncation.any() && !exp.any() {
            return false;
        }
        let tool_id = tool_id.to_owned();
        let entry = self.expanded_tools.entry(tool_id.clone()).or_default();
        entry.script = !entry.script;
        entry.output = !entry.output;
        self.rebuild_expanded_tool(&tool_id);
        true
    }

    #[cfg(test)]
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    #[cfg(test)]
    pub fn message_at(&self, index: usize) -> Option<&DisplayMessage> {
        self.messages.get(index)
    }

    pub fn last_message_text(&self) -> &str {
        self.messages.last().map(|m| m.text.as_str()).unwrap_or("")
    }

    #[cfg(test)]
    pub fn last_message_is_plan(&self) -> bool {
        self.messages.last().is_some_and(|m| m.plan_path.is_some())
    }

    #[cfg(test)]
    pub fn last_message_role(&self) -> Option<&DisplayRole> {
        self.messages.last().map(|m| &m.role)
    }

    #[cfg(test)]
    pub fn snapshot_gen_of(&self, tool_id: &str) -> Option<u64> {
        self.messages
            .iter()
            .rfind(|msg| matches!(&msg.role, DisplayRole::Tool(role) if role.id == tool_id))
            .map(|msg| msg.snapshot_theme_gen)
    }

    #[cfg(test)]
    pub fn streaming_text_is_empty(&self) -> bool {
        self.streaming_text.is_empty()
    }

    #[cfg(test)]
    pub fn streaming_thinking_is_empty(&self) -> bool {
        self.streaming_thinking.is_empty()
    }

    #[cfg(test)]
    pub fn tool_turn_usage(&self, tool_id: &str) -> Option<&str> {
        self.messages.iter().rev().find_map(|msg| match &msg.role {
            DisplayRole::Tool(tool) if tool.id == tool_id => msg.turn_usage.as_deref(),
            _ => None,
        })
    }

    pub fn set_prompt_progress(&mut self, progress: Option<PromptProgress>) {
        self.prompt_progress = progress;
    }

    pub fn clear_prompt_progress(&mut self) {
        self.prompt_progress = None;
    }

    pub fn flush(&mut self) {
        self.flush_thinking();
        self.prompt_progress = None;
        if !self.streaming_text.is_empty() {
            self.messages.push(DisplayMessage::new(
                DisplayRole::Assistant,
                self.streaming_text.take_all(),
            ));
        }
    }

    fn layout(&self) -> Layout<'_> {
        Layout::new(&self.cache, self.viewport_width)
    }

    /// Positive scrolls up. Clamping is immediate rather than deferred to the
    /// next `view`, so scrolling back up starts from the bottom row and not
    /// from wherever an overscroll left the position.
    pub fn scroll(&mut self, delta: i32) {
        let rows = delta.unsigned_abs();
        let layout = self.layout();
        let moved = if delta >= 0 {
            layout.retreat(self.scroll, rows)
        } else {
            layout.advance(self.scroll, rows)
        };
        let clamped = moved.min(layout.bottom(self.viewport_height));
        self.scroll_to(clamped);
    }

    /// Always unpins, and the next `view` re-pins if this lands on the
    /// bottom line.
    fn scroll_to(&mut self, pos: ScrollPos) {
        self.scroll = pos;
        self.auto_scroll = false;
    }

    pub fn auto_scroll(&self) -> bool {
        self.auto_scroll
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_to(ScrollPos::default());
    }

    pub fn enable_auto_scroll(&mut self) {
        self.auto_scroll = true;
    }

    pub fn scroll_to_segment(&mut self, segment_index: usize) {
        self.scroll_to(ScrollPos {
            seg: segment_index,
            row: 0,
        });
    }

    /// Backs `maki.fn.winrestview`, the one caller that still speaks in
    /// document rows.
    pub fn scroll_to_row(&mut self, doc_row: u32) {
        self.scroll_to(self.layout().at_row(doc_row));
    }

    pub fn restore_scroll(&mut self, scroll: ScrollPos, auto_scroll: bool) {
        self.scroll = scroll;
        self.auto_scroll = auto_scroll;
    }

    pub fn set_highlight_segment(&mut self, idx: Option<usize>) {
        self.highlight_segment = idx;
    }

    pub fn half_page(&self) -> i32 {
        self.viewport_height as i32 / 2
    }

    pub fn page(&self) -> i32 {
        self.viewport_height.max(1) as i32
    }

    pub fn set_accent(&mut self, color: ratatui::style::Color) {
        self.accent.set(color);
    }

    /// {col} is the screen column, for the right-aligned move hint.
    pub fn handle_click(&mut self, row: u16, col: u16, area: Rect) -> ClickOutcome {
        self.tool_target_at(row, col, area)
            .unwrap_or_else(|| self.toggle_click(row, area))
    }

    /// The click a row takes when it targets no task, for a moved row whose
    /// task is gone.
    pub fn toggle_click(&mut self, row: u16, area: Rect) -> ClickOutcome {
        if self.toggle_at(row, area) {
            ClickOutcome::Handled
        } else {
            ClickOutcome::None
        }
    }

    /// A moved row opens its task from anywhere on it. On a running row only
    /// the hint moves that one call, so the rest of the row keeps its clicks.
    fn tool_target_at(&self, row: u16, col: u16, area: Rect) -> Option<ClickOutcome> {
        if area.height == 0 {
            return None;
        }
        let pos = self
            .layout()
            .advance(self.scroll, u32::from(row.saturating_sub(area.y)));
        if pos.seg >= self.live_start {
            return None;
        }
        let seg = self.cache.get(pos.seg)?;
        let tool_id = seg.tool_id.as_deref()?;
        let msg = self.find_tool_msg(tool_id)?;
        if let Some(badge) = &msg.background {
            return Some(ClickOutcome::OpenTask(badge.id.clone()));
        }
        let col = col.checked_sub(area.x)?;
        (pos.row == 0 && hint_columns(seg.lines().first()?)?.contains(&col))
            .then(|| ClickOutcome::MoveCall(tool_id.to_owned()))
    }

    fn toggle_at(&mut self, row: u16, area: Rect) -> bool {
        if area.height == 0 {
            return false;
        }
        let pos = self
            .layout()
            .advance(self.scroll, u32::from(row.saturating_sub(area.y)));
        let width = self.viewport_width;
        if pos.seg >= self.live_start {
            return self.try_toggle_live_thinking(pos);
        }
        let Some(seg) = self.cache.get(pos.seg) else {
            return false;
        };
        if !seg.images.is_empty() && pos.row >= seg.text_height(width) {
            return false;
        }
        let Some(tool_id) = seg.tool_id.as_deref() else {
            return self.try_toggle_cached_thinking(pos.seg, width)
                || self.try_toggle_cached_notice(pos.seg, width);
        };
        // A moved row draws only its header, so there is nothing to unfold.
        if self
            .find_tool_msg(tool_id)
            .is_some_and(|msg| msg.background.is_some())
        {
            return false;
        }

        if self.has_snapshot(tool_id) {
            let buf_row = seg
                .source_line_at(pos.row, width)
                .map_or(0, |l| seg.buf_row(l));
            if self.route_of(tool_id) == RenderRoute::Renderer {
                let tool_id = tool_id.to_owned();
                self.click_rendered(&tool_id, buf_row);
                return true;
            }
            return true;
        }

        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        if !seg.truncation.any() && !exp.any() {
            return false;
        }
        let tool_id = tool_id.to_owned();
        let truncation = seg.truncation;

        let entry = self.expanded_tools.entry(tool_id.clone()).or_default();
        if truncation.output || entry.output {
            entry.output = !entry.output;
        } else if truncation.script || entry.script {
            entry.script = !entry.script;
        }
        self.rebuild_expanded_tool(&tool_id);
        true
    }

    fn rebuild_expanded_tool(&mut self, tool_id: &str) {
        if segment::is_instruction_segment(tool_id) {
            if let Some(parent_id) = segment::instruction_parent(tool_id)
                && let Some(blocks) = self.get_instructions_for_tool(parent_id)
            {
                self.upsert_instruction_segment(parent_id, &blocks);
            }
        } else {
            self.rebuild_tool_segment(tool_id);
        }
    }

    fn get_instructions_for_tool(&self, tool_id: &str) -> Option<Vec<InstructionBlock>> {
        let msg = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))?;
        msg.tool_output.as_deref()?.owned_instructions()
    }

    /// Drains the highlight worker, renderer replies and image decodes. These used to run inside [`Self::view`], which is why a
    /// running tool had to claim it was animating: it was the only way to keep
    /// them fed.
    pub fn tick(&mut self) -> Dirty {
        let mut dirty = self.drain_highlights() | self.poll_renders() | self.refresh_images();
        if self.show_idle_splash() {
            dirty |= self.idle_splash.poll_update(update::latest_version());
        }
        dirty
    }

    pub fn cadence(&self) -> Cadence {
        let smooth = self.streaming_text.is_animating()
            || self.accent.is_animating()
            || (self.streaming_thinking.is_animating()
                && thinking::reveals_text(self.thinking_lines, self.live_thinking_expanded));
        Cadence::any([
            // A running tool draws a spinner. Its output arriving is data, and
            // `tick` reports that separately.
            Cadence::when(self.in_progress_count() > 0, Cadence::SPINNER),
            // A moved row's elapsed time only changes once a second, and it
            // may tick for hours.
            self.elapsed_due().map_or(Cadence::IDLE, Cadence::clock),
            Cadence::when(smooth, Cadence::SMOOTH),
            Cadence::when(self.show_idle_splash(), self.idle_splash.cadence()),
        ])
    }

    fn show_idle_splash(&self) -> bool {
        self.messages.is_empty()
            && self.streaming_thinking.is_empty()
            && self.streaming_text.is_empty()
    }

    /// A resume can come back to a terminal whose font size changed while we
    /// were suspended, and the picker caches those cell metrics, so rebuild it
    /// and drop every protocol encoded against the old ones.
    fn refresh_images(&mut self) -> Dirty {
        let generation = terminal_image::generation();
        if generation == self.image_generation {
            return Dirty::any(
                self.cache
                    .segments_mut()
                    .iter_mut()
                    .map(Segment::poll_images),
            );
        }
        self.image_generation = generation;
        self.image_picker = terminal_image::picker(self.inline_images);
        for seg in self.cache.segments_mut() {
            seg.release_images();
        }
        Dirty::YES
    }

    /// Rebuilding an encoded protocol costs a decode, a resize and, on kitty, a
    /// retransmit of megabytes. Releasing exactly at the viewport edge would
    /// make a one row scroll thrash, so segments just outside keep theirs.
    fn release_images_outside(&mut self, last_drawn: usize) {
        let keep = self.scroll.seg.saturating_sub(IMAGE_KEEP_MARGIN_SEGMENTS)
            ..=last_drawn.saturating_add(IMAGE_KEEP_MARGIN_SEGMENTS);
        for (i, seg) in self.cache.segments_mut().iter_mut().enumerate() {
            if !keep.contains(&i) {
                seg.release_images();
            }
        }
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        has_selection: bool,
        images_visible: bool,
        lua_commands: &[LuaCommandInfo],
    ) {
        self.viewport_height = area.height;
        let width = area.width.saturating_sub(1);
        let theme_gen = theme::generation();
        let theme_changed = self.theme_generation != theme_gen;
        let needs_reflow = self.viewport_width != width || theme_changed;
        if needs_reflow {
            self.viewport_width = width;
            self.theme_generation = theme_gen;
        }
        if theme_changed {
            self.rebake_stale_snapshots(theme_gen);
        }

        // Last frame's streaming segments go first. Otherwise a message that
        // `flush` just committed would be built after them and show up twice.
        self.cache.truncate(self.live_start);
        if self.show_idle_splash() {
            let accent = self.accent.resolve();
            self.idle_splash
                .render(area, frame.buffer_mut(), accent, lua_commands);
            return;
        }

        if needs_reflow {
            self.cache.mark_all_stale();
            let thinking = thinking_style();
            let assistant = assistant_style();
            self.streaming_thinking.set_style(
                thinking.prefix,
                thinking.text_style,
                thinking.prefix_style,
            );
            self.streaming_text.set_style(
                assistant.prefix,
                assistant.text_style,
                assistant.prefix_style,
            );
        }
        self.rebuild_line_cache();
        if self.in_progress_count() > 0 || self.elapsed_due().is_some() {
            self.update_spinners();
        }

        self.rebuild_live_segments(width);

        // The reflow window is picked from `scroll` and the bottom pin, and
        // the reflow changes the heights both are derived from: resolve
        // before to aim the window, and after to place the result.
        self.resolve_scroll(has_selection);
        self.reflow_viewport(width);
        self.resolve_scroll(has_selection);

        let viewport = Rect::new(area.x, area.y, width, area.height);
        let mut cursor = RenderCursor::new(self.scroll.row, viewport);

        let mut last_drawn = self.scroll.seg;
        for (i, seg) in self
            .cache
            .segments_mut()
            .iter_mut()
            .enumerate()
            .skip(self.scroll.seg)
        {
            if cursor.past_bottom() {
                break;
            }
            let highlight = self.highlight_segment == Some(i);
            let style = seg.tool_id.as_ref().map(|_| theme::current().tool_bg);
            cursor.render_segment(seg, style, highlight, frame);
            for image in &mut seg.images {
                cursor.render_image(image, self.image_picker.as_ref(), images_visible, frame);
            }
            last_drawn = i;
        }
        self.release_images_outside(last_drawn);

        if let Some(pp) = self.prompt_progress
            && pp.total > 0
        {
            let ratio = pp.processed as f64 / pp.total as f64;
            let bar_width = (width as f64 * 0.1).round() as u16;
            let label = " Processing ";
            let label_width = label.len() as u16;
            let total_width = label_width + bar_width;
            let bar_x = area.x + width.saturating_sub(total_width);
            let bar_y = area.y + area.height.saturating_sub(1);
            let bar_area = Rect::new(bar_x, bar_y, total_width, 1);
            crate::components::progress_bar::render(
                frame,
                bar_area,
                &crate::components::progress_bar::ProgressBarConfig {
                    ratio,
                    style: theme::current().progress_bar,
                    cache_ratio: pp.cache as f64 / pp.total as f64,
                    cache_style: Style::new().fg(Color::Green),
                    label: Some(label),
                    label_style: Some(theme::current().tool_dim),
                    bar_width,
                },
            );
        }

        // Both walks are O(transcript) and a resize makes each one re-wrap
        // every segment it touches, so they stay behind the toggle that
        // decides whether anything is drawn from them.
        if scrollbar::is_enabled() {
            let layout = self.layout();
            let total_rows = layout.total_rows();
            if total_rows > u32::from(area.height) {
                render_vertical_scrollbar(frame, area, total_rows, layout.doc_row(self.scroll));
            }
        }
    }

    /// Same order and spacers as `rebuild_line_cache` builds once they flush,
    /// so a [`ScrollPos`] still points at the same text after the flush.
    fn rebuild_live_segments(&mut self, width: u16) {
        self.live_start = self.cache.len();
        if !self.streaming_thinking.is_empty() {
            let collapsed = thinking::collapsed_view(
                thinking::tail_rows(self.thinking_lines, self.live_thinking_expanded),
                self.streaming_thinking.line_count(),
                CLICKABLE,
                |rows| {
                    self.streaming_thinking
                        .render_tail(rows, width)
                        .map(<[_]>::to_vec)
                },
            );
            let drawn_collapsed = collapsed.is_some();
            let lines =
                collapsed.unwrap_or_else(|| self.streaming_thinking.render_lines(width).to_vec());
            self.push_live_segment(lines, drawn_collapsed);
        }
        if !self.streaming_text.is_empty() {
            let lines = self.streaming_text.render_lines(width).to_vec();
            self.push_live_segment(lines, false);
        }
    }

    fn push_live_segment(&mut self, lines: Vec<Line<'static>>, collapsed: bool) {
        self.cache.push_spacer_if_needed();
        let mut seg = Segment::with_lines(lines, None);
        seg.truncation.output = collapsed;
        self.cache.push(seg);
    }

    pub fn scroll_pos(&self) -> ScrollPos {
        self.scroll
    }

    /// Where a click at `rel_row` rows into the viewport lands, as a place a
    /// resize cannot move.
    pub fn doc_pos_at(&self, rel_row: u16, col: u16) -> DocPos {
        let pos = self.layout().advance(self.scroll, u32::from(rel_row));
        DocPos {
            seg: pos.seg,
            row: pos.row,
            col,
        }
    }

    /// Walks at most a viewport worth of segments, so a live selection costs
    /// O(viewport) per frame rather than O(transcript).
    pub fn project_row(&self, pos: DocPos) -> RowPos {
        let at = ScrollPos {
            seg: pos.seg,
            row: pos.row,
        };
        if at < self.scroll {
            return RowPos::Above;
        }
        let rows = self.layout().rows_from(self.scroll, at);
        if rows >= u32::from(self.viewport_height) {
            RowPos::Below
        } else {
            RowPos::At(rows as u16)
        }
    }

    /// Backs `maki.fn.winsaveview`. The clamp matters: a pinned or restored
    /// scroll position can sit past the end until the next `view` resolves it
    /// against the current line count.
    pub fn win_view(&self) -> WinView {
        let layout = self.layout();
        let bottom = layout.bottom(self.viewport_height);
        WinView {
            scroll_top: layout.doc_row(layout.clamp(self.scroll.min(bottom))),
            line_count: layout.total_rows(),
            height: self.viewport_height,
            auto_scroll: self.auto_scroll,
        }
    }

    /// Built on demand rather than retained: a plain-text copy of the whole
    /// transcript, sitting beside the rendered one, was the second largest
    /// thing the panel held. Entry `i` must describe segment `i`, because
    /// `SearchAction::Select` feeds the index straight back to
    /// [`Self::scroll_to_segment`].
    pub fn segment_search_texts(&self) -> Vec<String> {
        let by_tool: HashMap<&str, &DisplayMessage> = self
            .messages
            .iter()
            .filter_map(|m| match &m.role {
                DisplayRole::Tool(t) => Some((t.id.as_str(), m)),
                _ => None,
            })
            .collect();
        let tool_text = |id: &str| match segment::instruction_parent(id) {
            Some(parent) => by_tool
                .get(parent)
                .and_then(|m| m.tool_output.as_deref())
                .and_then(ToolOutput::instructions)
                .map(instructions_search_text),
            None => by_tool.get(id).map(|m| search_text_for(m)),
        };
        self.cache
            .segments()
            .iter()
            .map(|seg| {
                match seg.tool_id.as_deref() {
                    Some(id) => tool_text(id),
                    None => seg
                        .msg_index
                        .and_then(|i| self.messages.get(i))
                        .map(message_search_text),
                }
                .unwrap_or_default()
            })
            .collect()
    }

    pub fn extract_selection_text(&self, sel: &Selection, msg_area: Rect) -> String {
        selection::extract_selection_text(&self.cache, self.viewport_width, sel, msg_area)
    }

    fn has_snapshot(&self, tool_id: &str) -> bool {
        self.messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
            .is_some_and(|m| m.render_snapshot.is_some())
    }

    /// Draw renderer snapshots again when the theme changes.
    fn rebake_stale_snapshots(&mut self, current_gen: u64) {
        let rendered: Vec<_> = self
            .messages
            .iter()
            .filter_map(|msg| {
                let DisplayRole::Tool(role) = &msg.role else {
                    return None;
                };
                (self.route_of(&role.id) == RenderRoute::Renderer
                    && msg.snapshot_is_stale(current_gen))
                .then(|| role.id.clone())
            })
            .collect();
        for id in rendered {
            self.owe_draw(&id, RenderReason::Rerender);
        }
    }

    fn find_tool_msg(&self, tool_id: &str) -> Option<&DisplayMessage> {
        self.messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
    }

    fn find_tool_msg_mut(&mut self, tool_id: &str) -> Option<&mut DisplayMessage> {
        self.messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
    }

    fn rctx(&self) -> RenderCtx<'_> {
        RenderCtx {
            started_at: self.started_at,
            width: self.viewport_width,
            tool_output_lines: &self.tool_output_lines,
        }
    }

    /// {from_data} rows show what the call's data holds, which a remote agent
    /// sends raw, so their text is sanitized here, where it is laid out.
    fn build_tool_segment_lines(
        msg: &DisplayMessage,
        status: ToolStatus,
        rctx: &RenderCtx,
        exp: SectionFlags,
        from_data: bool,
    ) -> ToolLines {
        let mut tl = build_tool_lines(msg, status, rctx, exp);
        if from_data {
            sanitize_lines(&mut tl.lines);
        }
        if let Some(header) = tl.lines.first_mut() {
            let hint = (msg.movable && status == ToolStatus::InProgress).then_some(MOVABLE_HINT);
            let usage = msg
                .turn_usage
                .as_deref()
                .filter(|_| msg.timestamp.is_some());
            append_right_info(header, hint, usage, msg.timestamp.as_deref(), rctx.width);
        }
        tl
    }

    fn flush_thinking(&mut self) {
        if self.streaming_thinking.is_empty() {
            return;
        }
        let mut msg =
            DisplayMessage::new(DisplayRole::Thinking, self.streaming_thinking.take_all());
        msg.thinking_expanded = mem::take(&mut self.live_thinking_expanded);
        self.messages.push(msg);
    }

    fn try_toggle_live_thinking(&mut self, pos: ScrollPos) -> bool {
        // Thinking is always the first live segment, so it sits right after
        // the spacer that separates it from whatever came before.
        let thinking_seg = self.live_start + usize::from(self.live_start > 0);
        if pos.seg != thinking_seg || self.streaming_thinking.is_empty() {
            return false;
        }
        let drawn_collapsed = self.cache.get(pos.seg).is_some_and(|s| s.truncation.output);
        if !thinking::click_toggles(self.live_thinking_expanded, drawn_collapsed) {
            return false;
        }
        self.live_thinking_expanded = !self.live_thinking_expanded;
        true
    }

    fn try_toggle_cached_thinking(&mut self, seg_idx: usize, width: u16) -> bool {
        let Some(seg) = self.cache.get(seg_idx) else {
            return false;
        };
        let drawn_collapsed = seg.truncation.output;
        let Some(msg) = seg
            .msg_index
            .and_then(|idx| self.messages.get_mut(idx))
            .filter(|m| matches!(m.role, DisplayRole::Thinking))
        else {
            return false;
        };
        if !thinking::click_toggles(msg.thinking_expanded, drawn_collapsed) {
            return false;
        }
        msg.thinking_expanded = !msg.thinking_expanded;
        self.reflow_text_segment(seg_idx, width);
        true
    }

    fn try_toggle_cached_notice(&mut self, seg_idx: usize, width: u16) -> bool {
        let msg_index = self.cache.get(seg_idx).and_then(|seg| seg.msg_index);
        let Some(msg) = msg_index
            .and_then(|idx| self.messages.get_mut(idx))
            .filter(|m| matches!(m.role, DisplayRole::Notice) && m.detail.is_some())
        else {
            return false;
        };
        msg.detail_expanded = !msg.detail_expanded;
        self.reflow_text_segment(seg_idx, width);
        true
    }

    fn elapsed_due(&self) -> Option<Duration> {
        self.cache
            .segments()
            .iter()
            .filter_map(Segment::elapsed_due)
            .min()
    }

    fn update_spinners(&mut self) {
        let spinner_span = Span::styled(
            spinner_str(self.started_at.elapsed().as_millis()),
            theme::current().spinner,
        );
        for seg in self.cache.segments_mut() {
            seg.update_spinners(&spinner_span);
            seg.update_elapsed();
        }
    }

    fn drain_highlights(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        while let Some(mut result) = self.hl_worker.try_recv() {
            if let Some(seg) = self
                .cache
                .segments_mut()
                .iter_mut()
                .find(|s| s.matches_pending_highlight(result.id))
            {
                if seg
                    .tool_id
                    .as_deref()
                    .is_some_and(|id| draws_from_data(&self.routes, id))
                {
                    sanitize_lines(&mut result.lines);
                }
                seg.apply_highlight_result(result.lines);
                dirty = Dirty::YES;
            }
        }
        dirty
    }

    fn rebuild_tool_segment(&mut self, tool_id: &str) {
        let Some(msg) = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let DisplayRole::Tool(t) = &msg.role else {
            unreachable!()
        };
        let status = t.status;
        let Some(seg_idx) = self.cache.find_by_tool_id(tool_id) else {
            return;
        };

        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        let rctx = self.rctx();
        let from_data = self.routed_away(tool_id);
        let tl = Self::build_tool_segment_lines(msg, status, &rctx, exp, from_data);

        let instructions = msg
            .tool_output
            .as_deref()
            .and_then(|o| o.owned_instructions());

        let seg = self.cache.get_mut(seg_idx).unwrap();
        seg.update_with_reuse(tl, &self.hl_worker);
        seg.set_images(message_images(msg));

        if let Some(blocks) = instructions {
            self.upsert_instruction_segment(tool_id, &blocks);
        }
    }

    fn rebuild_line_cache(&mut self) {
        if !self.cache.needs_rebuild(self.messages.len()) {
            return;
        }
        for i in self.cache.msg_count()..self.messages.len() {
            let msg = &self.messages[i];

            if let DisplayRole::Tool(t) = &msg.role {
                let exp = self.expanded_tools.get(&t.id).copied().unwrap_or_default();
                let status = t.status;
                let from_data = self.routed_away(&t.id);
                let tl = Self::build_tool_segment_lines(msg, status, &self.rctx(), exp, from_data);
                let id = t.id.clone();
                self.cache.push_spacer_if_needed();
                let mut seg = Segment::with_tool(id.clone());
                seg.apply_highlight(tl, &self.hl_worker);
                seg.set_images(message_images(msg));
                self.cache.push(seg);
                self.cache.reserve_instructions(&id);

                let blocks = msg
                    .tool_output
                    .as_deref()
                    .and_then(|o| o.owned_instructions());
                if let Some(blocks) = blocks {
                    self.upsert_instruction_segment(&id, &blocks);
                }
            } else {
                let (lines, truncated) =
                    message_view(msg, self.thinking_lines, self.viewport_width);
                self.cache.push_spacer_if_needed();
                let mut seg = Segment::with_lines(lines, Some(i));
                seg.truncation.output = truncated;
                seg.set_images(message_images(msg));
                self.cache.push(seg);
            }
        }
        self.cache.mark_built(self.messages.len());
    }

    /// Clamps the scroll position against the document end and applies the
    /// bottom pin.
    fn resolve_scroll(&mut self, has_selection: bool) {
        let bottom = self.layout().bottom(self.viewport_height);
        self.scroll = self.layout().clamp(self.scroll.min(bottom));
        if !has_selection {
            if self.scroll >= bottom {
                self.auto_scroll = true;
            }
            if self.auto_scroll {
                self.scroll = bottom;
            }
        }
    }

    /// Re-lays out the stale segments the viewport plus its margin reaches.
    /// The scroll position names a segment, so reflowing cannot slide the
    /// content the reader is looking at; the window only has to cover what is
    /// on screen. Skipping the rest only costs fidelity, see
    /// [`Segment::stale`].
    ///
    /// Heights are counted after each segment is reflowed, so the window is
    /// right the first time.
    fn reflow_viewport(&mut self, width: u16) {
        let viewport = u32::from(self.viewport_height);
        let margin = viewport.saturating_mul(REFLOW_MARGIN_VIEWPORTS);
        // The first visible row sits `scroll.row` rows into the starting
        // segment, so the downward window has to clear those before it starts
        // covering the viewport.
        let below = viewport
            .saturating_add(margin)
            .saturating_add(u32::from(self.scroll.row));
        let start = self.scroll.seg.min(self.cache.len().saturating_sub(1));

        self.reflow_run(start..self.cache.len(), below, width);
        self.reflow_run((0..start).rev(), margin, width);
    }

    fn reflow_run(&mut self, indices: impl Iterator<Item = usize>, row_budget: u32, width: u16) {
        let mut rows = 0;
        for i in indices {
            if rows >= row_budget {
                return;
            }
            rows += self.reflowed_height(i, width);
        }
    }

    /// Reflows `seg_idx` if it is stale, then reports the height it draws at.
    fn reflowed_height(&mut self, seg_idx: usize, width: u16) -> u32 {
        // A tool segment and its instruction segment both map back to the
        // same parent, and one `rebuild_tool_segment` clears both flags.
        // Re-check so the parent is not rebuilt twice.
        if self.cache.get(seg_idx).is_some_and(|s| s.stale) {
            self.reflow_segment(seg_idx, width);
        }
        self.cache
            .get(seg_idx)
            .map_or(0, |s| s.height(width) as u32)
    }

    fn reflow_segment(&mut self, seg_idx: usize, width: u16) {
        // Clear up front so a reflow that bails early (message gone, empty
        // instructions) costs one frame of old-width lines instead of
        // retrying forever.
        let Some(seg) = self.cache.get_mut(seg_idx) else {
            return;
        };
        seg.stale = false;
        let Some(tid) = seg.tool_id.clone() else {
            self.reflow_text_segment(seg_idx, width);
            return;
        };
        let parent = segment::instruction_parent(&tid)
            .map(str::to_string)
            .unwrap_or(tid);
        self.rebuild_tool_segment(&parent);
    }

    fn reflow_text_segment(&mut self, seg_idx: usize, width: u16) {
        let Some(msg_idx) = self.cache.get(seg_idx).and_then(|s| s.msg_index) else {
            return;
        };
        let Some(msg) = self.messages.get(msg_idx) else {
            return;
        };
        let (lines, truncated) = message_view(msg, self.thinking_lines, width);
        let Some(seg) = self.cache.get_mut(seg_idx) else {
            return;
        };
        seg.set_lines(lines);
        seg.truncation.output = truncated;
    }
}

/// An image the message carries is all there is to see, so a terminal without
/// graphics gets an `[image]` line in its place. A tool's image already has a
/// header naming the file above it, so it needs no stand-in.
fn message_images(
    msg: &DisplayMessage,
) -> impl Iterator<Item = (ImageSource, Option<&'static str>)> + '_ {
    msg.images
        .iter()
        .map(|source| (source.clone(), Some(IMAGE_PLACEHOLDER)))
        .chain(match msg.tool_output.as_deref() {
            Some(ToolOutput::Image { source, .. }) => Some((source.clone(), None)),
            _ => None,
        })
}

fn message_style(role: &DisplayRole) -> RoleStyle {
    match role {
        DisplayRole::User => user_style(),
        DisplayRole::Assistant => assistant_style(),
        DisplayRole::Thinking => thinking_style(),
        DisplayRole::Error => error_style(),
        DisplayRole::Done => done_style(),
        DisplayRole::Notice => notice_style(),
        DisplayRole::Tool(_) => unreachable!(),
    }
}

/// A plan message draws its own rule and path instead of the role prefix.
fn message_prefix(msg: &DisplayMessage, style: &RoleStyle) -> &'static str {
    if msg.plan_path.is_some() {
        ""
    } else {
        style.prefix
    }
}

/// Carries the role prefix so a query can hit either the prose or the "you>"
/// and "thinking>" markers the reader sees. Collapsed thinking needs no case of
/// its own: its indicator is drawn from the same prefix.
fn message_search_text(msg: &DisplayMessage) -> String {
    let style = message_style(&msg.role);
    format!("{}{}", message_prefix(msg, &style), msg.text)
}

/// A non-Tool message's lines at `width`, and whether they are thinking drawn
/// collapsed, which is what makes a click on them expand it.
fn message_view(
    msg: &DisplayMessage,
    mode: ThinkingLines,
    width: u16,
) -> (Vec<Line<'static>>, bool) {
    if matches!(msg.role, DisplayRole::Thinking)
        && let Some(lines) = thinking::collapsed_view(
            thinking::tail_rows(mode, msg.thinking_expanded),
            thinking::line_count(&msg.text),
            CLICKABLE,
            |rows| thinking::render_tail(&msg.text, rows, width, thinking_style().text_style, None),
        )
    {
        return (lines, true);
    }
    let mut lines = build_message_lines(msg, width);
    if let Some(end) = msg.task_end
        && let Some(first) = lines.first_mut()
    {
        first.spans.insert(0, outcome_glyph(end));
    }
    if matches!(msg.role, DisplayRole::Notice)
        && let Some(detail) = &msg.detail
    {
        push_notice_detail(&mut lines, detail, msg.detail_expanded);
    }
    (lines, false)
}

fn push_notice_detail(lines: &mut Vec<Line<'static>>, detail: &str, expanded: bool) {
    let style = notice_style().text_style;
    let mark = if expanded {
        NOTICE_EXPANDED_MARK
    } else {
        NOTICE_COLLAPSED_MARK
    };
    if let Some(last) = lines.last_mut() {
        last.spans.push(Span::styled(mark, style));
    }
    if expanded {
        lines.extend(
            detail.lines().map(|line| {
                Line::from(Span::styled(format!("{NOTICE_DETAIL_INDENT}{line}"), style))
            }),
        );
    }
}

/// Builds ratatui lines for a non-Tool message shown in full at the given
/// width. Shared by `rebuild_line_cache` (new messages) and
/// `reflow_text_segment` (stale-on-resize messages) so both paths produce
/// identical segments.
fn build_message_lines(msg: &DisplayMessage, width: u16) -> Vec<Line<'static>> {
    let style = message_style(&msg.role);
    let prefix = message_prefix(msg, &style);
    let text = if !msg.images.is_empty() && msg.text == IMAGE_PLACEHOLDER {
        ""
    } else {
        &msg.text
    };
    let mut lines = if style.use_markdown {
        text_to_lines(
            text,
            prefix,
            style.text_style,
            style.prefix_style,
            width,
            style.max_line_bytes,
        )
    } else {
        plain_lines(text, prefix, style.text_style, style.prefix_style)
    };
    if let Some(pp) = &msg.plan_path {
        if !msg.text.is_empty() {
            let rule = hr_line(width, theme::current().plan_rule);
            lines.insert(0, rule.clone());
            lines.push(rule);
        } else {
            lines.clear();
        }
        if !msg.text.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            pp.to_owned(),
            theme::current().plan_path,
        )));
        lines.push(Line::from(Span::styled(
            format!(
                "{} to open in editor ($VISUAL / $EDITOR)",
                key::OPEN_EDITOR.label
            ),
            theme::current().tool_dim,
        )));
    }
    lines
}
