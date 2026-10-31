//! Who draws each tool call: the UI renderer its tool registered, the default
//! renderer. The route is chosen from the first event that names its tool
//! and owns every later update
//! of the row, but a call whose output a hook changed draws with the default
//! from its done on. A call a renderer draws keeps the data the renderer
//! reads and has at most one request in flight; what arrives meanwhile is
//! owed.

use std::collections::{BTreeMap, HashMap};
use std::mem;
use std::path::Path;
use std::sync::Arc;

use flume::{Receiver, TryRecvError};
use maki_lua_ui::{RenderCall, RenderCtx, RenderReason, RenderReply, RenderRoute};
use maki_types::event::{ToolProgressEvent, ToolStartEvent};
use maki_types::tool::{ChildStatus, ToolChild, ToolData};
use serde_json::Value;

use super::MessagesPanel;
use crate::components::{DisplayMessage, DisplayRole, ToolStatus};
use crate::repaint::Dirty;

/// The session a call is drawn for, never the drawing process.
#[derive(Clone, Copy)]
pub(crate) struct RenderPlace<'a> {
    /// The session's working directory.
    pub cwd: &'a Path,
    /// The agent's home, known when it runs on this machine.
    pub home: Option<&'a Path>,
}

pub(crate) type CallData = (Arc<ToolData>, Vec<(Vec<u32>, String)>);

pub(super) struct RoutedCall {
    route: RenderRoute,
    /// The start facts, then the done data.
    data: Arc<ToolData>,
    /// Kept on every route, so a renderer that comes later draws it too.
    live: Live,
}

/// What a running call published since its start, by path: its own state
/// and text and each nested call's latest record, state and text. A record
/// never waits for its parent's or an earlier sibling's, and none of them
/// overwrites another's, until one settles: it carries its final children,
/// so it takes over its whole subtree. The done data replaces it all.
#[derive(Default)]
struct Live {
    child: Option<ToolChild>,
    state: Option<Value>,
    stream: String,
    children: BTreeMap<u32, Live>,
}

impl Live {
    fn finished(&self) -> bool {
        self.child
            .as_ref()
            .is_some_and(|child| child.status != ChildStatus::Running)
    }

    fn take(&mut self, event: ToolProgressEvent) {
        let mut live = self;
        for index in &event.path {
            if live.finished() {
                return;
            }
            live = live.children.entry(*index).or_default();
        }
        if live.finished() {
            return;
        }
        if let Some(child) = event.child.filter(|_| !event.path.is_empty()) {
            if child.status != ChildStatus::Running {
                *live = Self::default();
            }
            live.child = Some(*child);
        }
        if !live.finished() {
            if let Some(state) = event.state {
                live.state = Some(state);
            }
            live.stream.push_str(&event.append);
        }
    }

    /// Apply live records and state over the call's data. Stream paths stay
    /// invocation indices even when earlier siblings have no record yet.
    fn over(&self, data: &Arc<ToolData>) -> (Arc<ToolData>, Vec<(Vec<u32>, String)>) {
        let mut streamed = Vec::new();
        if self.state.is_none() && self.children.is_empty() {
            if !self.stream.is_empty() {
                streamed.push((Vec::new(), self.stream.clone()));
            }
            return (Arc::clone(data), streamed);
        }
        let mut data = ToolData::clone(data);
        self.overlay(&mut Vec::new(), &mut data, &mut streamed);
        (Arc::new(data), streamed)
    }

    fn overlay(
        &self,
        path: &mut Vec<u32>,
        data: &mut ToolData,
        streamed: &mut Vec<(Vec<u32>, String)>,
    ) {
        if let Some(state) = &self.state {
            data.state = state.clone();
        }
        if !self.stream.is_empty() {
            streamed.push((path.clone(), self.stream.clone()));
        }
        let mut children: BTreeMap<u32, ToolChild> = mem::take(&mut data.children)
            .into_iter()
            .map(|child| (child.index, child))
            .collect();
        for (&index, live) in &self.children {
            if let Some(child) = &live.child {
                children.insert(index, child.clone());
            }
        }
        for (index, child) in &mut children {
            if let Some(live) = self.children.get(index) {
                path.push(*index);
                live.overlay(path, &mut child.data, streamed);
                path.pop();
            }
        }
        data.children = children.into_values().collect();
    }
}

/// The renderer requests of one call.
#[derive(Default)]
pub(super) struct RenderJob {
    in_flight: Option<InFlight>,
    /// A draw owed for new data, a theme or a route. It replays every click,
    /// so it takes the owed clicks along.
    owed: Option<RenderReason>,
    /// The last clicks of the call's log, not sent yet.
    clicks_owed: usize,
}

impl RenderJob {
    fn ready(&self) -> bool {
        self.in_flight.is_none() && (self.owed.is_some() || self.clicks_owed > 0)
    }

    fn idle(&self) -> bool {
        self.in_flight.is_none() && self.owed.is_none() && self.clicks_owed == 0
    }
}

struct InFlight {
    reply: Receiver<RenderReply>,
    theme_gen: u64,
}

/// Whether {id} draws from its data, by its renderer or the default, so
/// nothing the legacy path sends may reach its row.
pub(super) fn draws_from_data(routes: &HashMap<String, RoutedCall>, id: &str) -> bool {
    routes
        .get(id)
        .is_some_and(|call| call.route != RenderRoute::Default)
}

fn child_status(status: ToolStatus) -> ChildStatus {
    match status {
        ToolStatus::InProgress => ChildStatus::Running,
        ToolStatus::Success => ChildStatus::Done,
        ToolStatus::Error => ChildStatus::Error,
    }
}

impl MessagesPanel {
    pub(super) fn routed_away(&self, id: &str) -> bool {
        draws_from_data(&self.routes, id)
    }

    pub(super) fn route_of(&self, id: &str) -> RenderRoute {
        self.routes
            .get(id)
            .map_or(RenderRoute::Default, |call| call.route)
    }

    pub(crate) fn call_data(&self, id: &str) -> Option<CallData> {
        self.routes.get(id).map(|call| call.live.over(&call.data))
    }

    pub(crate) fn show_child(
        &mut self,
        id: &str,
        child: &ToolChild,
        streams: &[(Vec<u32>, String)],
    ) {
        self.update_tool(id, |msg| {
            msg.tool_raw_input = Some(Arc::new(child.data.input.clone()));
            msg.tool_output = child.output.clone();
            let stream: String = streams
                .iter()
                .filter(|(path, _)| path.is_empty())
                .map(|(_, text)| text.as_str())
                .collect();
            if child.output.is_none() && !stream.is_empty() {
                msg.live_output = Some(stream.clone());
                let header = msg.text.lines().next().unwrap_or_default();
                msg.text = format!("{header}\n{stream}");
            }
            if let Some(output) = &child.output {
                let header = msg.text.lines().next().unwrap_or_default();
                msg.text = format!("{header}\n{}", output.as_text());
            }
        });
        self.route_data(id, Some(&Arc::new(child.data.clone())), RenderReason::Live);
        for (path, append) in streams {
            self.tool_progress(ToolProgressEvent {
                id: id.into(),
                path: path.clone(),
                append: append.clone(),
                state: None,
                child: None,
            });
        }
    }

    fn table_route(&self, tool: &str) -> RenderRoute {
        self.lua_event_handle.ui.renderer_routes().route(tool)
    }

    /// The table's route, but a call whose output a hook changed draws with
    /// the default where a renderer would draw it.
    fn data_route(&self, tool: &str, rewritten: bool) -> RenderRoute {
        match self.table_route(tool) {
            RenderRoute::Renderer if rewritten => RenderRoute::Default,
            route => route,
        }
    }

    /// Routes a call at its start, unless an earlier event already did.
    pub(super) fn route_start(&mut self, event: &ToolStartEvent) -> RenderRoute {
        if let Some(call) = self.routes.get(&event.id) {
            return call.route;
        }
        let route = self.table_route(&event.tool);
        let data = {
            let mut data = ToolData::new(
                Arc::clone(&event.tool),
                event.data_version,
                event.raw_input.clone().unwrap_or_default(),
                event.summary.clone(),
            );
            data.annotations.extend(event.annotation.clone());
            Arc::new(data)
        };
        self.routes.insert(
            event.id.clone(),
            RoutedCall {
                route,
                data,
                live: Live::default(),
            },
        );
        self.clear_snapshot(&event.id);
        self.owe_draw(&event.id, RenderReason::Live);
        route
    }

    /// The done data replaces the live state and the streamed text. A call
    /// with no start, such as one an input hook refused, routes here.
    pub(super) fn route_done(&mut self, id: &str, data: Option<&Arc<ToolData>>) {
        self.route_data(id, data, RenderReason::Live);
    }

    /// Routes loaded calls with stored tool data. Older calls without it
    /// keep the default renderer and their text output.
    pub(crate) fn route_loaded(&mut self, tool_data: &HashMap<String, Arc<ToolData>>) {
        let loaded: Vec<_> = self
            .messages
            .iter()
            .filter_map(|msg| match &msg.role {
                DisplayRole::Tool(role) => tool_data
                    .get(&role.id)
                    .map(|data| (role.id.clone(), Arc::clone(data))),
                _ => None,
            })
            .collect();
        for (id, data) in loaded {
            self.route_data(&id, Some(&data), RenderReason::Load);
        }
    }

    /// A call routed to a renderer whose done data says a hook changed its
    /// output moves to the default.
    fn route_data(&mut self, id: &str, data: Option<&Arc<ToolData>>, reason: RenderReason) {
        match self.routes.get_mut(id) {
            Some(call) => {
                if let Some(data) = data {
                    call.data = Arc::clone(data);
                }
                call.live = Live::default();
                if call.route == RenderRoute::Renderer && data.is_some_and(|data| data.rewritten) {
                    self.move_route(id, RenderRoute::Default);
                    return;
                }
            }
            None => {
                let Some(data) = data.filter(|_| self.tool_msg(id).is_some()) else {
                    return;
                };
                let route = self.data_route(&data.tool, data.rewritten);
                self.routes.insert(
                    id.to_owned(),
                    RoutedCall {
                        route,
                        data: Arc::clone(data),
                        live: Live::default(),
                    },
                );
                self.clear_snapshot(id);
            }
        }
        self.owe_draw(id, reason);
    }

    /// Clear the previous renderer's snapshots.
    fn clear_snapshot(&mut self, id: &str) {
        if let Some(msg) = self.find_tool_msg_mut(id) {
            msg.render_header = None;
            msg.render_snapshot = None;
        }
    }

    /// Kept whatever the route, for the top-level call {event} names. Only
    /// a renderer draws it.
    pub fn tool_progress(&mut self, event: ToolProgressEvent) {
        let id = event.id.clone();
        let Some(call) = self.routes.get_mut(&id) else {
            return;
        };
        call.live.take(event);
        self.owe_draw(&id, RenderReason::Live);
    }

    pub(super) fn owe_draw(&mut self, id: &str, reason: RenderReason) {
        if self.route_of(id) == RenderRoute::Renderer {
            self.render_jobs.entry(id.to_owned()).or_default().owed = Some(reason);
        }
    }

    /// A draw again from what the call has, unless one is owed already.
    fn owe_redraw(&mut self, id: &str) {
        if self.route_of(id) == RenderRoute::Renderer {
            self.render_jobs
                .entry(id.to_owned())
                .or_default()
                .owed
                .get_or_insert(RenderReason::Rerender);
        }
    }

    /// Rows replayed from history draw as a load does.
    pub(super) fn draw_owed_as_load(&mut self) {
        for job in self.render_jobs.values_mut() {
            if job.owed == Some(RenderReason::Live) {
                job.owed = Some(RenderReason::Load);
            }
        }
    }

    /// Logs a click on line {row} of the body a renderer drew, for it to take
    /// in turn.
    pub(super) fn click_rendered(&mut self, id: &str, row: usize) {
        self.lua_clicks.entry(id.to_owned()).or_default().push(row);
        self.render_jobs
            .entry(id.to_owned())
            .or_default()
            .clicks_owed += 1;
    }

    /// Sends what each renderer-drawn call owes, one request at a time, after
    /// moving every call whose route a renderer reload changed.
    pub(crate) fn send_renders(&mut self, place: RenderPlace<'_>) {
        self.keep_place(place);
        self.reroute();
        let ready: Vec<String> = self
            .render_jobs
            .iter()
            .filter(|(_, job)| job.ready())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ready {
            let Some(call) = self.render_call(&id) else {
                self.render_jobs.remove(&id);
                continue;
            };
            let (owed, clicks_owed) = {
                let job = &self.render_jobs[&id];
                (job.owed, job.clicks_owed)
            };
            let clicks = self.lua_clicks.get(&id).map_or(&[][..], Vec::as_slice);
            let ui = &self.lua_event_handle.ui;
            let (reply, clicks_left) = match owed {
                Some(reason) => (
                    ui.redraw(call, self.render_ctx(place, reason), clicks.to_vec()),
                    0,
                ),
                None => {
                    let at = clicks.len().saturating_sub(clicks_owed);
                    let Some(&row) = clicks.get(at) else {
                        self.render_jobs.remove(&id);
                        continue;
                    };
                    let ctx = self.render_ctx(place, RenderReason::Rerender);
                    (
                        ui.click(call, ctx, row, clicks[..at].to_vec()),
                        clicks.len() - at - 1,
                    )
                }
            };
            let theme_gen = self.theme_generation;
            let job = self.render_jobs.get_mut(&id).expect("listed as ready");
            job.owed = None;
            job.clicks_owed = clicks_left;
            job.in_flight = Some(InFlight { reply, theme_gen });
        }
    }

    fn render_call(&self, id: &str) -> Option<RenderCall> {
        let call = self.routes.get(id)?;
        let (data, streamed) = call.live.over(&call.data);
        let msg = self.tool_msg(id)?;
        let DisplayRole::Tool(role) = &msg.role else {
            return None;
        };
        let status = child_status(role.status);
        let output = msg
            .tool_output
            .clone()
            .filter(|_| status != ChildStatus::Running);
        Some(RenderCall {
            id: id.to_owned(),
            data,
            output,
            status,
            streamed,
        })
    }

    fn render_ctx(&self, place: RenderPlace<'_>, reason: RenderReason) -> RenderCtx {
        RenderCtx {
            cwd: place.cwd.to_path_buf(),
            home: place.home.map(Path::to_path_buf),
            tool_output_lines: self.tool_output_lines,
            width: self.viewport_width,
            reason,
        }
    }

    fn tool_msg(&self, id: &str) -> Option<&DisplayMessage> {
        self.messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == id))
    }

    /// Draws what the renderers answered. A host gone mid-request leaves the
    /// default.
    pub(super) fn poll_renders(&mut self) -> Dirty {
        let mut answered = Vec::new();
        for (id, job) in &mut self.render_jobs {
            let Some(in_flight) = job.in_flight.as_ref() else {
                continue;
            };
            let reply = match in_flight.reply.try_recv() {
                Ok(reply) => reply,
                Err(TryRecvError::Empty) => continue,
                Err(TryRecvError::Disconnected) => RenderReply::Default,
            };
            answered.push((id.clone(), reply, in_flight.theme_gen));
            job.in_flight = None;
        }
        self.render_jobs.retain(|_, job| !job.idle());
        let dirty = Dirty::from(!answered.is_empty());
        for (id, reply, theme_gen) in answered {
            self.apply_render(&id, reply, theme_gen);
        }
        dirty
    }

    /// A part the renderer left out, or all of a call it handed back, draws
    /// with the default, over anything drawn before. A reply drawn for an
    /// older theme is drawn again instead.
    fn apply_render(&mut self, id: &str, reply: RenderReply, theme_gen: u64) {
        if self.route_of(id) != RenderRoute::Renderer {
            return;
        }
        if theme_gen < self.theme_generation {
            self.owe_redraw(id);
            return;
        }
        let (header, body) = match reply {
            RenderReply::Drawn { header, body } => (header, body),
            RenderReply::Default => (None, None),
        };
        let Some(msg) = self.find_tool_msg_mut(id) else {
            return;
        };
        msg.render_header = header;
        msg.render_snapshot = body;
        msg.snapshot_theme_gen = theme_gen;
        self.rebuild_tool_segment(id);
    }

    /// Once per publish of the routing table: moves each call it now routes
    /// elsewhere, and draws every other renderer row again, since a reload
    /// or a takeover may have changed the renderer behind it.
    fn reroute(&mut self) {
        let generation = self.lua_event_handle.ui.renderer_routes().generation();
        if generation == self.routes_seen {
            return;
        }
        self.routes_seen = generation;
        let mut moved = Vec::new();
        let mut kept = Vec::new();
        for (id, call) in &self.routes {
            let route = self.data_route(&call.data.tool, call.data.rewritten);
            if route != call.route {
                moved.push((id.clone(), route));
            } else if route == RenderRoute::Renderer {
                kept.push(id.clone());
            }
        }
        for (id, route) in moved {
            self.move_route(&id, route);
        }
        for id in kept {
            self.owe_redraw(&id);
        }
    }

    /// A changed renderer replaces the previous view and its click history.
    fn move_route(&mut self, id: &str, route: RenderRoute) {
        self.render_jobs.remove(id);
        self.lua_clicks.remove(id);
        self.clear_snapshot(id);
        if let Some(call) = self.routes.get_mut(id) {
            call.route = route;
        }
        match route {
            RenderRoute::Renderer => self.owe_draw(id, RenderReason::Rerender),
            RenderRoute::Default => {}
        }
        self.rebuild_tool_segment(id);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use maki_config::UiConfig;
    use maki_types::event::ToolDoneEvent;
    use maki_types::tool::BASH_TOOL_NAME;

    use maki_types::tool::ToolOutput;
    use ratatui::backend::TestBackend;

    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;

    use crate::lua_handle::test_support::{RequestProbe, probed_event_handle};

    const WAIT: Duration = Duration::from_secs(5);
    const PLUGIN: &str = "probe-ui";
    const LATE_PLUGIN: &str = "late-ui";
    const TOOL: &str = "probe";
    const LATE_TOOL: &str = "late";
    const LISTED: u32 = 1;
    const CALL: &str = "call-1";
    const CWD: &str = "/session/dir";
    const SUMMARY: &str = "probe summary";

    const LATE_HEADER: &str = "late header";
    const FIRST_TEXT: &str = "ab";
    const SECOND_TEXT: &str = "cd";
    const CHILD_TEXT: &str = "child text";
    const OUTPUT: &str = "done output";
    const VIEW_WIDTH: u16 = 80;
    const VIEW_HEIGHT: u16 = 24;
    const NEVER_ANSWERED: &str = "the renderer never answered";
    const NO_ROW: &str = "the call has no row";
    const DRAW: &str = "draw";
    const BOOM: &str = "boom";

    const STREAM: &str = "stream";
    const STATE: &str = "state";
    const CHILD: &str = "child";
    const CHILDREN: &str = "children";
    const TAKEOVER_PLUGIN: &str = "takeover-ui";
    const TAKEN: &str = "taken over";
    const FIRST_CHILD: &str = "first";
    const SECOND_CHILD: &str = "second";
    const GRANDCHILD: &str = "grand";
    const USAGE: &str = "1k";
    const NO_STATE: &str = "nil";
    const CLICKS: &str = "clicks";
    const ESCAPED: &str = "escaped";
    /// What the fixture draws after [`ESCAPED`], and what reaches the screen.
    const RAW_ESCAPE: &str = r"a\27[31mb";
    const SHOWN_ESCAPE: &str = "ab";
    const ESCAPE_SEQUENCE: &str = "[31m";
    const RUNNING: &str = "running";

    const RESULT: &str = "result";

    /// `probe` counts its draws in its header, raises on a `boom` state, and
    /// draws in its body what it read and the clicks it took, with a control
    /// sequence a terminal would act on.
    fn fixture_source() -> String {
        format!(
            r#"
            local draws = 0
            maki.ui.register_renderer("{TOOL}", {{
                header = function(data, ctx)
                    draws = draws + 1
                    if data.state and data.state.{BOOM} then error("{BOOM}") end
                    return "{DRAW} " .. draws .. " " .. ctx.reason .. " " .. data.status .. " " .. ctx.cwd
                end,
                body = function(data)
                    data.clicks = data.clicks or {{}}
                    local child = data.children[1]
                    local buf = maki.ui.buf()
                    buf:line("{STREAM} " .. (data.stream or ""))
                    buf:line("{STATE} " .. tostring(data.state and data.state.{STATE}))
                    buf:line("{CHILD} " .. tostring(child and child.stream))
                    buf:line("{CLICKS} " .. table.concat(data.clicks, ","))
                    local kids = {{}}
                    for _, kid in ipairs(data.children) do
                        local grand = kid.children[1]
                        local grand_state = grand and grand.state and grand.state.{STATE}
                        table.insert(kids, kid.index .. ":" .. kid.summary .. "/" .. #kid.children .. "/" .. tostring(grand_state))
                    end
                    buf:line("{CHILDREN} " .. table.concat(kids, ","))
                    buf:line("{ESCAPED} {RAW_ESCAPE}")
                    local output = data.output and data.output.Plain
                    buf:line("{RESULT} " .. (output and output.text or ""):gsub("\n", "/"))
                    return buf
                end,
                click = function(data, ctx, row) table.insert(data.clicks, row) end,
            }})
            "#
        )
    }

    /// Draws {header}, the streamed text and the state for {tool}.
    fn header_source(tool: &str, header: &str) -> String {
        format!(
            "maki.ui.register_renderer('{tool}', {{ \
             header = function(data) return '{header} ' .. tostring(data.stream) \
             .. ' ' .. tostring(data.state and data.state.{STATE}) end }})"
        )
    }

    fn late_source() -> String {
        header_source(LATE_TOOL, LATE_HEADER)
    }

    fn new_host() -> maki_lua_ui::UiHost {
        let host = maki_lua_ui::UiHost::bare();
        host.load_source(PLUGIN, &fixture_source()).unwrap();
        host
    }

    /// The renderers run on {host}; what the panel asks the bridge lands in
    /// the probe.
    fn panel_on(host: &maki_lua_ui::UiHost) -> (MessagesPanel, RequestProbe) {
        let (mut handle, probe) = probed_event_handle();
        handle.ui = host.ui_handle();
        let panel = MessagesPanel::new(UiConfig::default(), handle);
        (panel, probe)
    }

    fn place() -> RenderPlace<'static> {
        RenderPlace {
            cwd: Path::new(CWD),
            home: None,
        }
    }

    impl MessagesPanel {
        /// Sends what every renderer row owes for {place} and draws each
        /// answer, until nothing is owed.
        pub(in crate::components::messages) fn settle_renders(&mut self, place: RenderPlace<'_>) {
            loop {
                self.send_renders(place);
                let sent: Vec<(String, InFlight)> = self
                    .render_jobs
                    .iter_mut()
                    .filter_map(|(id, job)| job.in_flight.take().map(|sent| (id.clone(), sent)))
                    .collect();
                if sent.is_empty() {
                    return;
                }
                for (id, sent) in sent {
                    let reply = sent.reply.recv_timeout(WAIT).expect(NEVER_ANSWERED);
                    self.apply_render(&id, reply, sent.theme_gen);
                }
                self.render_jobs.retain(|_, job| !job.idle());
            }
        }
    }

    fn settle(panel: &mut MessagesPanel) {
        panel.settle_renders(place());
    }

    fn view(panel: &mut MessagesPanel) {
        let mut terminal =
            ratatui::Terminal::new(TestBackend::new(VIEW_WIDTH, VIEW_HEIGHT)).unwrap();
        terminal
            .draw(|f| panel.view(f, f.area(), false, true, &[]))
            .unwrap();
    }

    /// The row as the terminal gets it.
    fn row_text(panel: &mut MessagesPanel, id: &str) -> String {
        view(panel);
        let seg = panel.cache.find_by_tool_id(id).expect(NO_ROW);
        panel
            .cache
            .get(seg)
            .unwrap()
            .lines()
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| &*span.content)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn start(id: &str, tool: &str, version: u32) -> ToolStartEvent {
        ToolStartEvent {
            id: id.into(),
            tool: tool.into(),
            data_version: version,
            summary: SUMMARY.into(),
            annotation: None,
            input: None,
            raw_input: Some(json!({})),
            output: None,
        }
    }

    fn data(tool: &str, version: u32, state: Value) -> Arc<ToolData> {
        let mut data = ToolData::new(Arc::from(tool), version, json!({}), SUMMARY.into());
        data.state = state;
        Arc::new(data)
    }

    fn done(id: &str, tool: &str, data: Option<Arc<ToolData>>) -> ToolDoneEvent {
        ToolDoneEvent {
            call: None,
            id: id.into(),
            tool: tool.into(),
            output: Arc::new(ToolOutput::Plain(OUTPUT.into())),
            is_error: false,
            annotation: None,
            written_path: None,
            data,

            backgrounded: false,
        }
    }

    fn progress(id: &str, path: Vec<u32>, append: &str, state: Option<Value>) -> ToolProgressEvent {
        ToolProgressEvent {
            id: id.into(),
            path,
            append: append.into(),
            state,
            child: None,
        }
    }

    fn msg<'a>(panel: &'a MessagesPanel, id: &str) -> &'a DisplayMessage {
        panel.tool_msg(id).expect(NO_ROW)
    }

    /// The header the fixture draws on its {draws}th draw.
    fn drew(draws: u32, reason: RenderReason, status: &str) -> String {
        format!("{DRAW} {draws} {} {status} {CWD}", <&str>::from(reason))
    }

    fn line(label: &str, value: impl std::fmt::Display) -> String {
        format!("{label} {value}")
    }

    fn child(index: u32, summary: &str, status: ChildStatus) -> ToolChild {
        ToolChild {
            index,
            status,
            data: ToolData::new(Arc::from(BASH_TOOL_NAME), LISTED, json!({}), summary.into()),
            output: None,
            usage: None,
        }
    }

    fn child_record(path: Vec<u32>, summary: &str, status: ChildStatus) -> ToolProgressEvent {
        let index = path.last().copied().unwrap_or_default();
        let mut event = progress(CALL, path, "", None);
        event.child = Some(Box::new(child(index, summary, status)));
        event
    }

    /// One child as the fixture lists it: its 1-based index, summary,
    /// grandchild count and first grandchild's state.
    fn kid(index: u32, summary: &str, grandchildren: usize, state: &str) -> String {
        format!("{index}:{summary}/{grandchildren}/{state}")
    }

    fn state(n: u32) -> Value {
        json!({ STATE: n })
    }

    /// A progress that lands while a draw is in flight is drawn once after
    /// it, with everything that arrived, a child's record and text included.
    #[test_case(false, 2, 0 ; "overlapping_progress_coalesces")]
    #[test_case(true, 3, 1 ; "sparse_child_streams_before_its_sibling")]
    fn progress_draws_the_streamed_text_and_the_latest_state(
        settle_between: bool,
        draws: u32,
        child_index: u32,
    ) {
        let host = new_host();
        let (mut panel, _probe) = panel_on(&host);
        panel.tool_start(start(CALL, TOOL, LISTED));
        panel.send_renders(place());
        panel.tool_progress(progress(CALL, Vec::new(), FIRST_TEXT, Some(state(1))));
        if settle_between {
            settle(&mut panel);
        }
        let mut child = progress(CALL, vec![child_index], CHILD_TEXT, None);
        child.child = Some(Box::new(self::child(child_index, "", ChildStatus::Running)));
        panel.tool_progress(child);
        panel.tool_progress(progress(CALL, Vec::new(), SECOND_TEXT, Some(state(2))));
        settle(&mut panel);
        let row = row_text(&mut panel, CALL);
        assert!(
            row.contains(&drew(draws, RenderReason::Live, RUNNING)),
            "{row}"
        );
        assert!(
            row.contains(&line(STREAM, format!("{FIRST_TEXT}{SECOND_TEXT}"))),
            "{row}"
        );
        assert!(row.contains(&line(STATE, 2)), "{row}");
        assert!(row.contains(&line(CHILD, CHILD_TEXT)), "{row}");
    }

    #[test]
    fn a_raising_renderer_draws_the_default_and_clears_its_snapshot() {
        let host = new_host();
        let (mut panel, _probe) = panel_on(&host);
        panel.tool_start(start(CALL, TOOL, LISTED));
        settle(&mut panel);
        assert!(msg(&panel, CALL).render_snapshot.is_some());
        panel.tool_progress(progress(CALL, Vec::new(), "", Some(json!({ BOOM: true }))));
        settle(&mut panel);
        let drawn = msg(&panel, CALL);
        assert!(drawn.render_header.is_none() && drawn.render_snapshot.is_none());
        assert!(row_text(&mut panel, CALL).contains(SUMMARY));
    }

    #[test]
    fn a_renderer_snapshot_draws_sanitized() {
        let host = new_host();
        let (mut panel, _probe) = panel_on(&host);
        panel.tool_start(start(CALL, TOOL, LISTED));
        settle(&mut panel);
        let row = row_text(&mut panel, CALL);
        assert!(row.contains(&line(ESCAPED, SHOWN_ESCAPE)), "{row}");
        assert!(!row.contains(ESCAPE_SEQUENCE), "{row}");
    }

    /// The second child finishes before the first sends anything, and still
    /// draws, first in the list.
    #[test]
    fn a_child_that_finishes_before_an_earlier_sibling_draws() {
        let host = new_host();
        let (mut panel, _probe) = panel_on(&host);
        panel.tool_start(start(CALL, TOOL, LISTED));
        panel.tool_progress(child_record(vec![1], SECOND_CHILD, ChildStatus::Done));
        settle(&mut panel);
        let row = row_text(&mut panel, CALL);
        assert!(
            row.contains(&line(CHILDREN, kid(2, SECOND_CHILD, 0, NO_STATE))),
            "{row}"
        );
    }

    /// A parent's live record carries none of the grandchildren the agent
    /// sends apart, even before its own record, so a usage update keeps them
    /// and their state.
    #[test]
    fn a_parent_update_keeps_its_grandchildren() {
        let host = new_host();
        let (mut panel, _probe) = panel_on(&host);
        panel.tool_start(start(CALL, TOOL, LISTED));
        panel.tool_progress(child_record(vec![1, 2], GRANDCHILD, ChildStatus::Running));
        panel.tool_progress(progress(CALL, vec![1, 2], "", Some(state(5))));
        panel.tool_progress(child_record(vec![1], FIRST_CHILD, ChildStatus::Running));
        let mut usage = child_record(vec![1], FIRST_CHILD, ChildStatus::Running);
        if let Some(child) = usage.child.as_mut() {
            child.usage = Some(USAGE.into());
        }
        panel.tool_progress(usage);
        settle(&mut panel);
        let row = row_text(&mut panel, CALL);
        assert!(
            row.contains(&line(CHILDREN, kid(2, FIRST_CHILD, 1, "5"))),
            "{row}"
        );
    }

    /// A legacy call keeps its progress, so the renderer that comes later
    /// draws the text and the state it streamed before.
    #[test]
    fn progress_before_a_renderer_comes_reaches_its_first_draw() {
        let host = new_host();
        let (mut panel, _probe) = panel_on(&host);
        panel.tool_start(start(CALL, LATE_TOOL, LISTED));
        panel.tool_progress(progress(CALL, Vec::new(), FIRST_TEXT, Some(state(3))));
        settle(&mut panel);
        assert_eq!(panel.route_of(CALL), RenderRoute::Default);
        host.load_source(LATE_PLUGIN, &late_source()).unwrap();
        settle(&mut panel);
        let row = row_text(&mut panel, CALL);
        assert!(
            row.contains(&format!("{LATE_HEADER} {FIRST_TEXT} 3")),
            "{row}"
        );
    }

    /// The route stays a renderer's, but the renderer behind it changed.
    #[test]
    fn a_takeover_draws_finished_rows_again() {
        let host = new_host();
        let (mut panel, _probe) = panel_on(&host);
        panel.tool_start(start(CALL, TOOL, LISTED));
        panel.tool_done(done(CALL, TOOL, Some(data(TOOL, LISTED, Value::Null))));
        settle(&mut panel);
        host.load_source(TAKEOVER_PLUGIN, &header_source(TOOL, TAKEN))
            .unwrap();
        settle(&mut panel);
        assert_eq!(panel.route_of(CALL), RenderRoute::Renderer);
        let row = row_text(&mut panel, CALL);
        assert!(row.contains(TAKEN), "{row}");
    }

    /// The theme moves while the first draw is in flight: its reply is
    /// dropped and the call drawn again for the new theme.
    #[test]
    fn a_reply_for_an_older_theme_is_drawn_again() {
        let host = new_host();
        let (mut panel, _probe) = panel_on(&host);
        panel.tool_start(start(CALL, TOOL, LISTED));
        panel.send_renders(place());
        panel.theme_generation += 1;
        settle(&mut panel);
        assert_eq!(panel.snapshot_gen_of(CALL), Some(panel.theme_generation));
        let row = row_text(&mut panel, CALL);
        assert!(
            row.contains(&drew(2, RenderReason::Rerender, RUNNING)),
            "{row}"
        );
    }

    /// A settled child carries its final children: the live grandchild it
    /// had goes, so it shows once, and a late update under it counts for
    /// nothing.
    #[test]
    fn a_settled_child_takes_over_its_subtree() {
        let host = new_host();
        let (mut panel, _probe) = panel_on(&host);
        panel.tool_start(start(CALL, TOOL, LISTED));
        panel.tool_progress(child_record(vec![0], FIRST_CHILD, ChildStatus::Running));
        panel.tool_progress(child_record(vec![0, 0], GRANDCHILD, ChildStatus::Running));
        panel.tool_progress(progress(CALL, vec![0, 0], "", Some(state(5))));
        let mut settled = child_record(vec![0], FIRST_CHILD, ChildStatus::Done);
        if let Some(record) = settled.child.as_mut() {
            record.data.children = vec![child(0, GRANDCHILD, ChildStatus::Done)];
        }
        panel.tool_progress(settled);
        panel.tool_progress(progress(CALL, vec![0, 0], "", Some(state(9))));
        panel.tool_progress(child_record(vec![0, 1], GRANDCHILD, ChildStatus::Running));
        settle(&mut panel);
        let row = row_text(&mut panel, CALL);
        let expected = kid(1, FIRST_CHILD, 1, NO_STATE);
        assert!(row.contains(&line(CHILDREN, expected)), "{row}");
    }
}
