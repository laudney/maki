//! Tool renderers: what a plugin's UI entry registers to draw a call from its
//! data, the routing table the UI reads without the Lua thread, and the
//! lookup a renderer composes another tool's with.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use maki_lua_macro::lua_fn;
use mlua::{
    Function, Lua, LuaString, MultiValue, RegistryKey, Result as LuaResult, Table,
    Value as LuaValue,
};

use crate::api::util::call::{REWRITTEN_KEY, SUMMARY_KEY};
use crate::api::util::convert::reject_unknown_fields;
use crate::domains::ui::render::finish_inline;
use crate::runtime::{plugin_loaded, strip_traceback};

const REGISTER_RENDERER: &str = "register_renderer";
const HEADER_KEY: &str = "header";
const BODY_KEY: &str = "body";
const CLICK_KEY: &str = "click";
const SPEC_KEYS: &[&str] = &[HEADER_KEY, BODY_KEY, CLICK_KEY];
const EMPTY_TOOL_ERR: &str = "register_renderer: 'tool' must be a non-empty string";
const NO_DRAWING_ERR: &str = "register_renderer: give 'header', 'body' or both";
const NOT_A_FUNCTION_ERR: &str = "must be a function";

/// How the UI draws a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderRoute {
    /// The tool has a renderer.
    Renderer,
    /// The tool has no renderer.
    Default,
}

/// Tools with a renderer, published by the Lua thread so the UI can route
/// calls without asking the VM.
#[derive(Default)]
pub struct RendererRoutes {
    table: ArcSwap<HashSet<Arc<str>>>,
    generation: AtomicU64,
}

impl RendererRoutes {
    pub fn route(&self, tool: &str) -> RenderRoute {
        if self.table.load().contains(tool) {
            RenderRoute::Renderer
        } else {
            RenderRoute::Default
        }
    }

    /// Bumped on every publish, so the UI can tell that a route it chose may
    /// have changed without comparing the tables.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn publish(&self, table: HashSet<Arc<str>>) {
        self.table.store(Arc::new(table));
        self.generation.fetch_add(1, Ordering::Release);
    }
}

struct RendererRecord {
    plugin: Arc<str>,
    tool: Arc<str>,
    header: Option<RegistryKey>,
    body: Option<RegistryKey>,
    click: Option<RegistryKey>,
}

impl RendererRecord {
    fn release(self, lua: &Lua) {
        for key in [self.header, self.body, self.click].into_iter().flatten() {
            if let Err(e) = lua.remove_registry_value(key) {
                tracing::warn!(plugin = %self.plugin, tool = %self.tool, error = %e, "failed to drop renderer key");
            }
        }
    }
}

/// Every plugin's renderers, in registration order. The last record of a tool
/// draws it, so a later load takes the tool over and an unload gives it back.
pub(crate) struct RendererStore {
    records: Vec<RendererRecord>,
    routes: Arc<RendererRoutes>,
}

impl RendererStore {
    pub fn new(routes: Arc<RendererRoutes>) -> Self {
        Self {
            records: Vec::new(),
            routes,
        }
    }

    fn active(&self, tool: &str) -> Option<&RendererRecord> {
        self.records
            .iter()
            .rev()
            .find(|record| &*record.tool == tool)
    }

    /// The record {record} replaces, which the same plugin made for the tool.
    fn insert(&mut self, record: RendererRecord) -> Option<RendererRecord> {
        let replaced = self
            .records
            .iter()
            .position(|r| r.plugin == record.plugin && r.tool == record.tool)
            .map(|index| self.records.remove(index));
        if let Some(other) = self.active(&record.tool) {
            tracing::warn!(
                tool = %record.tool,
                plugin = %record.plugin,
                replaced = %other.plugin,
                "renderer takes the tool over from another plugin"
            );
        }
        self.records.push(record);
        self.publish();
        replaced
    }

    fn publish(&self) {
        let routes = self
            .records
            .iter()
            .map(|record| Arc::clone(&record.tool))
            .collect();
        self.routes.publish(routes);
    }
}

/// The active renderer of a tool.
pub(crate) struct Renderer {
    pub plugin: Arc<str>,
    pub header: Option<Function>,
    pub body: Option<Function>,
    pub click: Option<Function>,
}

pub(crate) fn find_renderer(lua: &Lua, tool: &str) -> Option<Renderer> {
    let store = lua.app_data_ref::<RendererStore>()?;
    let record = store.active(tool)?;
    let function = |key: &Option<RegistryKey>| {
        key.as_ref()
            .and_then(|key| lua.registry_value::<Function>(key).ok())
    };
    Some(Renderer {
        plugin: Arc::clone(&record.plugin),
        header: function(&record.header),
        body: function(&record.body),
        click: function(&record.click),
    })
}

pub(crate) fn clear_plugin_renderers(lua: &Lua, plugin: &str) {
    let Some(mut store) = lua.app_data_mut::<RendererStore>() else {
        return;
    };
    let gone: Vec<RendererRecord> = store
        .records
        .extract_if(.., |record| &*record.plugin == plugin)
        .collect();
    if gone.is_empty() {
        return;
    }
    store.publish();
    drop(store);
    for record in gone {
        record.release(lua);
    }
}

fn spec_fn(spec: &Table, key: &str) -> LuaResult<Option<Function>> {
    match spec.raw_get::<LuaValue>(key)? {
        LuaValue::Nil => Ok(None),
        LuaValue::Function(f) => Ok(Some(f)),
        _ => Err(mlua::Error::runtime(format!(
            "{REGISTER_RENDERER}: '{key}' {NOT_A_FUNCTION_ERR}"
        ))),
    }
}

fn registry_key(lua: &Lua, function: Option<Function>) -> LuaResult<Option<RegistryKey>> {
    function.map(|f| lua.create_registry_value(f)).transpose()
}

/// Registers a renderer that draws calls of {tool} from their data, in the
/// UI runtime. The data is the same for a live call, a loaded session and a
/// remote agent, so a renderer never reads the agent's files or the drawing
/// process's directory: it draws from `data` and `ctx` alone.
///
/// A call without stored tool data, or whose renderer raises, draws with
/// the default renderer.
///
/// The renderers a plugin registers go when it reloads or unloads. When two
/// plugins register the same tool, the later registration draws it, and the
/// earlier one draws again once the later plugin unloads.
///
/// A renderer keeps its view state, such as which part is expanded, on the
/// `data` table. A click calls `click` with that same table, and `click`
/// only changes the state there: what it returns is ignored. The UI then
/// draws the call again with `header` and `body` from that table. A call
/// drawn long ago, or before a reload, is drawn once to set its state up,
/// then given its earlier clicks again in order, each followed by a draw, so
/// a click ends the same either way. The UI draws a call that way whenever
/// its data, the theme or the routing changes, so the clicks keep the state
/// they made. The clicks of one call run one at a time. Work that `header`,
/// `body` or `click` starts with `maki.async.run` finishes before the UI
/// shows the result.
///
/// `header` and `body` take `(data, ctx)`. The fields of `data`:
///
/// - `tool` (string) and `version` (integer) name the call's data.
/// - `input` (table) is the input the call ran with.
/// - `output` (table|nil) is the finished call's `ToolOutput` in its JSON form, such as `{ Plain = { text = "..." } }`.
/// - `state` (any) is the handler's state, its latest live state while it runs.
/// - `stream` (string|nil) is the text the handler streamed with `ctx:stream` while it runs.
/// - `status` (string) is `"running"`, `"done"`, `"error"` or `"cancelled"`.
/// - `summary` (string) and `annotations` (string[]) are the call's title and its annotations in order.
/// - `children` (table[]) holds the calls it made through `maki.agent.call_tool`, each with these fields, `usage` (string|nil) and `index` (integer). The list is in invocation order but leaves out a call that has sent nothing yet, so `index` is the call's 1-based invocation index, which pairs `children[k]` with `input.tool_calls[children[k].index]` for a tool such as `batch`.
/// - `rewritten` (boolean) is true when a `tool.<name>.output` layer changed the call's output. Its `state` is gone then, and the call draws with the default renderer, so a renderer only sees it as a child.
///
/// The fields of `ctx`:
///
/// - `cwd` (string) is the session's working directory.
/// - `home` (string|nil) is the agent's home directory.
/// - `tool_output_lines` (table) holds the UI's line limits per tool kind.
/// - `width` (integer) is the number of columns to draw in.
/// - `reason` (string) is `"live"` for a running or just finished call, `"load"` for a session being loaded and `"rerender"` for a call drawn again.
///
/// @param tool string Tool name.
/// @param spec table Renderer:
///   header   (function) Optional. `header(data, ctx)` returns the header line as a string, or a buffer.
///   body     (function) Optional. `body(data, ctx)` returns a buffer, or nil for no body.
///   click    (function) Optional. `click(data, ctx, row)` changes the state on `data` for a click on line `row` of the body, 1-based, or 0 outside it.
///   One of `header` and `body` is required. Throws on any other field.
/// @return
/// @example
/// maki.ui.register_renderer("greet", {
///   header = function(data) return "greet " .. data.input.name end,
///   body = function(data, ctx)
///     local buf = maki.ui.buf()
///     buf:line(data.state and data.state.greeting or "")
///     return buf
///   end,
/// })
#[lua_fn]
fn register_renderer(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    tool: String,
    spec: Table,
) -> LuaResult<()> {
    if tool.is_empty() {
        return Err(mlua::Error::runtime(EMPTY_TOOL_ERR));
    }
    reject_unknown_fields(&spec, SPEC_KEYS, REGISTER_RENDERER)?;
    // Every field is checked before any key is made, so a refused spec roots
    // nothing.
    let header = spec_fn(&spec, HEADER_KEY)?;
    let body = spec_fn(&spec, BODY_KEY)?;
    let click = spec_fn(&spec, CLICK_KEY)?;
    if header.is_none() && body.is_none() {
        return Err(mlua::Error::runtime(NO_DRAWING_ERR));
    }
    let record = RendererRecord {
        plugin,
        tool: Arc::from(tool),
        header: registry_key(lua, header)?,
        body: registry_key(lua, body)?,
        click: registry_key(lua, click)?,
    };
    // A handler from a load that is already gone would leave a renderer that
    // nothing ever takes away.
    if !plugin_loaded(lua, &record.plugin) {
        tracing::debug!(plugin = %record.plugin, tool = %record.tool, "renderer dropped: plugin unloaded");
        record.release(lua);
        return Ok(());
    }
    let replaced = lua
        .app_data_mut::<RendererStore>()
        .and_then(|mut store| store.insert(record));
    if let Some(replaced) = replaced {
        replaced.release(lua);
    }
    Ok(())
}

/// The renderer that draws {tool}, for a renderer that draws
/// another tool's data, such as a child call. Each call of a returned
/// function draws with the renderer registered then, so a table kept across
/// a reload or a takeover draws with the current one. When no renderer is
/// registered, or `data.rewritten` is true, the default draws: its `header`
/// returns `data.summary`, its `body` returns nil and its `click` does
/// nothing, and so does a part the registered renderer leaves out.
///
/// `click(data, ctx, row)` gives the click to the renderer's `click`, which
/// changes the state on `data`, and returns nothing: draw `header` and `body`
/// from `data` again to show what it changed.
///
/// No function raises: an error is logged and returns nil.
///
/// @param tool string Tool name, such as a child's `tool`.
/// @return (table) `{ header = function(data, ctx), body = function(data, ctx), click = function(data, ctx, row) }`.
/// @example
/// for _, child in ipairs(data.children) do
///   local draw = maki.ui.renderer(child.tool)
///   local header = draw.header(child, ctx)
///   if type(header) == "string" then buf:line(header) end
/// end
#[lua_fn]
fn renderer(lua: &Lua, tool: String) -> LuaResult<Table> {
    let tool: Arc<str> = Arc::from(tool);
    let table = lua.create_table()?;
    for part in [Part::Header, Part::Body, Part::Click] {
        table.set(part.key(), composed(lua, Arc::clone(&tool), part)?)?;
    }
    Ok(table)
}

/// Whether {tool} has a registered renderer now, so
/// `maki.ui.renderer(tool)` draws with it rather than with the
/// default. A call whose `data.rewritten` is true still draws with the
/// default.
///
/// @param tool string Tool name.
/// @return (boolean)
/// @example
/// if maki.ui.has_renderer(child.tool) then
///   local header = maki.ui.renderer(child.tool).header(child, ctx)
/// end
#[lua_fn]
fn has_renderer(lua: &Lua, tool: String) -> LuaResult<bool> {
    Ok(lua
        .app_data_ref::<RendererStore>()
        .is_some_and(|store| store.active(&tool).is_some()))
}

#[derive(Debug, Clone, Copy)]
enum Part {
    Header,
    Body,
    Click,
}

impl Part {
    fn key(self) -> &'static str {
        match self {
            Self::Header => HEADER_KEY,
            Self::Body => BODY_KEY,
            Self::Click => CLICK_KEY,
        }
    }

    fn of(self, renderer: Renderer) -> Option<(Arc<str>, Function)> {
        let function = match self {
            Self::Header => renderer.header,
            Self::Body => renderer.body,
            Self::Click => renderer.click,
        }?;
        Some((renderer.plugin, function))
    }

    /// The default renderer's part: the summary of the data for a header,
    /// nothing for a body or a click.
    fn default(self, args: &MultiValue) -> LuaValue {
        match (self, args.front()) {
            (Self::Header, Some(LuaValue::Table(data))) => data
                .raw_get::<Option<LuaString>>(SUMMARY_KEY)
                .ok()
                .flatten()
                .map_or(LuaValue::Nil, LuaValue::String),
            _ => LuaValue::Nil,
        }
    }

    /// What the part answers for {value}, what its renderer returned. A
    /// click's return is ignored, as it is for a call the UI draws.
    fn answer(self, value: LuaValue) -> LuaValue {
        match self {
            Self::Click => LuaValue::Nil,
            Self::Header | Self::Body => value,
        }
    }
}

/// Whether the data a composed part was given says a hook changed its
/// call's output.
fn rewritten(args: &MultiValue) -> bool {
    match args.front() {
        Some(LuaValue::Table(data)) => data
            .raw_get::<Option<bool>>(REWRITTEN_KEY)
            .ok()
            .flatten()
            .unwrap_or_default(),
        _ => false,
    }
}

/// {part} of the renderer registered for {tool} when it is
/// called. Async so a renderer can yield, as it may in a request.
fn composed(lua: &Lua, tool: Arc<str>, part: Part) -> LuaResult<Function> {
    lua.create_async_function(move |lua, args: MultiValue| {
        let tool = Arc::clone(&tool);
        async move {
            let Some((plugin, function)) = (!rewritten(&args))
                .then(|| find_renderer(&lua, &tool))
                .flatten()
                .and_then(|renderer| part.of(renderer))
            else {
                return Ok(part.default(&args));
            };
            match function.call_async::<LuaValue>(args).await {
                Ok(value) => {
                    finish_inline(&lua).await;
                    Ok(part.answer(value))
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = %plugin,
                        tool = %tool,
                        part = part.key(),
                        error = %strip_traceback(&e),
                        "composed renderer failed"
                    );
                    Ok(LuaValue::Nil)
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    use maki_config::ToolOutputLines;
    use maki_lua_core::facade::SessionRequest;
    use maki_types::snapshot::BufferSnapshot;
    use maki_types::tool::{ChildStatus, DEFAULT_DATA_VERSION, ToolChild, ToolData, ToolOutput};
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::UiHost;
    use crate::api::util::command::UiAction;
    use crate::domains::ui::{RenderCall, RenderCtx, RenderReason, RenderReply, WARM_CALL_CAP};

    const WAIT: Duration = Duration::from_secs(5);
    const PLUGIN: &str = "fixture-ui";
    const OTHER_PLUGIN: &str = "other-ui";
    const TOOL: &str = "fixture";
    const RAISER: &str = "raiser";
    const PARENT: &str = "parent";
    const MISSING: &str = "missing";
    const CMD: &str = "ls";
    const CWD: &str = "/session/dir";
    const SUMMARY: &str = "list files";
    const STATE_LINE: &str = "3 entries";
    const OUTPUT_TEXT: &str = "two files";
    const WIDTH: u16 = 80;
    const DREW: &str = "drew";
    const CACHER: &str = "cacher-ui";
    const CACHED: &str = "cached";
    const TAKEN_OVER: &str = "taken over";
    const CALL_ID: &str = "call-1";
    const TOGGLE: &str = "toggle";
    const BAD_CLICK: &str = "bad-click";
    const LATER: &str = "later";
    const OPEN: &str = "open";
    const CLOSED: &str = "closed";
    const CLICKS: &str = "clicks";
    const CLICKED: &str = "clicked";
    const LATE_LINE: &str = "late";
    const LATER_LINE: &str = "later still";
    const COUNTER: &str = "counter";
    const LAYOUT: &str = "layout";
    const COLD_CALL_ID: &str = "cold-call";
    const GATE_PLUGIN: &str = "gate-ui";
    const GATED: &str = "gated";
    const RELOADED: &str = "reloaded";
    const NEVER_FLASHED: &str = "the renderer never flashed";
    const COMPOSER: &str = "composer";
    const NO_BODY: &str = "none";

    /// `fixture` draws its header and body from the data and the context and
    /// flashes when called, `raiser` raises, and `parent` draws each
    /// child through `maki.ui.renderer`, one line per child.
    fn fixture_source() -> String {
        format!(
            r#"
            maki.ui.register_renderer("{TOOL}", {{
                header = function(data, ctx)
                    maki.ui.flash("{DREW}")
                    return data.input.cmd .. " in " .. ctx.cwd
                end,
                body = function(data, ctx)
                    local buf = maki.ui.buf()
                    buf:line(data.state.line)
                    buf:line(data.output.Plain.text)
                    buf:line(ctx.reason .. " " .. ctx.width)
                    return buf
                end,
            }})
            assert(maki.ui.has_renderer("{TOOL}"))
            assert(not maki.ui.has_renderer("{MISSING}"))
            maki.ui.register_renderer("{RAISER}", {{
                header = function() error("boom") end,
            }})
            maki.ui.register_renderer("{PARENT}", {{
                body = function(data, ctx)
                    local buf = maki.ui.buf()
                    for _, child in ipairs(data.children) do
                        local draw = maki.ui.renderer(child.tool)
                        buf:line(tostring(draw.header(child, ctx)) .. " " .. tostring(draw.body(child, ctx) ~= nil))
                    end
                    return buf
                end,
            }})
            "#
        )
    }

    fn renderer_source(tool: &str, header: &str) -> String {
        format!(
            "maki.ui.register_renderer('{tool}', {{ \
             header = function() return '{header}' end }})"
        )
    }

    /// `toggle` keeps the rows it was clicked on in `data`, draws its body
    /// from them and flashes each row it takes. `bad-click` raises on a
    /// click. `later` fills its body from inline tasks, one queued by
    /// another.
    fn click_source() -> String {
        format!(
            r#"
            maki.ui.register_renderer("{TOGGLE}", {{
                body = function(data)
                    data.clicks = data.clicks or {{}}
                    local state = #data.clicks % 2 == 1 and "{OPEN}" or "{CLOSED}"
                    local buf = maki.ui.buf()
                    buf:set_lines({{ state, "{CLICKS} " .. table.concat(data.clicks, ",") }})
                    return buf
                end,
                click = function(data, ctx, row)
                    maki.ui.flash("{CLICKED} " .. row)
                    table.insert(data.clicks, row)
                    return "ignored"
                end,
            }})
            maki.ui.register_renderer("{BAD_CLICK}", {{
                body = function() return maki.ui.buf() end,
                click = function() error("boom") end,
            }})
            maki.ui.register_renderer("{COUNTER}", {{
                header = function(data) return tostring(data.count or 0) end,
                click = function(data)
                    local count = data.count or 0
                    maki.async.sleep(0)
                    data.count = count + 1
                end,
            }})
            maki.ui.register_renderer("{LAYOUT}", {{
                body = function(data)
                    data.open = data.open or {{}}
                    data.rows = {{}}
                    local lines = {{}}
                    for _, item in ipairs({{ "a", "b" }}) do
                        table.insert(lines, item)
                        data.rows[#lines] = item
                        if data.open[item] then table.insert(lines, item .. " detail") end
                    end
                    local buf = maki.ui.buf()
                    buf:set_lines(lines)
                    return buf
                end,
                click = function(data, ctx, row)
                    local item = data.rows[row]
                    if item then data.open[item] = not data.open[item] end
                end,
            }})
            maki.ui.register_renderer("{LATER}", {{
                body = function(data)
                    local buf = maki.ui.buf()
                    maki.async.run(function()
                        buf:line("{LATE_LINE}")
                        maki.async.run(function() buf:line("{LATER_LINE}") end)
                    end)
                    if data.clicked then buf:line("{CLICKED}") end
                    return buf
                end,
                click = function(data)
                    maki.async.run(function() data.clicked = true end)
                end,
            }})
            "#
        )
    }

    /// `cached` keeps the table `maki.ui.renderer` gave it at load and draws
    /// its first child's header through it.
    fn cacher_source() -> String {
        format!(
            r#"
            local draw = maki.ui.renderer("{TOOL}")
            maki.ui.register_renderer("{CACHED}", {{
                header = function(data, ctx) return tostring(draw.header(data.children[1], ctx)) end,
            }})
            "#
        )
    }

    /// `gated` counts its clicks in its header. Its first click parks on a
    /// UI round trip, which the test answers when it chooses. A reload
    /// draws {header} instead.
    fn gated_source(header: Option<&str>) -> String {
        let header = header.map_or("tostring(data.count or 0)".to_owned(), |text| {
            format!("'{text}'")
        });
        format!(
            r#"
            maki.ui.register_renderer("{GATED}", {{
                header = function(data) return {header} end,
                click = function(data)
                    if (data.count or 0) == 0 then maki.session.list() end
                    data.count = (data.count or 0) + 1
                end,
            }})
            "#
        )
    }

    fn host_with_fixture() -> UiHost {
        let host = UiHost::bare();
        host.load_source(PLUGIN, &fixture_source()).unwrap();
        host
    }

    fn tool_data(tool: &str) -> ToolData {
        let mut data = ToolData::new(
            Arc::from(tool),
            DEFAULT_DATA_VERSION,
            json!({ "cmd": CMD }),
            SUMMARY.to_owned(),
        );
        data.state = json!({ "line": STATE_LINE });
        data
    }

    fn output() -> Arc<ToolOutput> {
        Arc::new(ToolOutput::Plain(OUTPUT_TEXT.into()))
    }

    fn call(id: &str, data: ToolData) -> RenderCall {
        RenderCall {
            id: id.to_owned(),
            data: Arc::new(data),
            output: Some(output()),
            status: ChildStatus::Done,
            streamed: Vec::new(),
        }
    }

    fn ctx() -> RenderCtx {
        RenderCtx {
            cwd: PathBuf::from(CWD),
            home: None,
            tool_output_lines: ToolOutputLines::default(),
            width: WIDTH,
            reason: RenderReason::Live,
        }
    }

    fn render(host: &UiHost, data: ToolData) -> RenderReply {
        render_call(host, CALL_ID, data)
    }

    fn render_call(host: &UiHost, id: &str, data: ToolData) -> RenderReply {
        host.ui_handle()
            .render(call(id, data), ctx())
            .recv_timeout(WAIT)
            .unwrap()
    }

    fn click_call(
        host: &UiHost,
        id: &str,
        data: ToolData,
        row: usize,
        earlier: &[usize],
    ) -> RenderReply {
        host.ui_handle()
            .click(call(id, data), ctx(), row, earlier.to_vec())
            .recv_timeout(WAIT)
            .unwrap()
    }

    fn click(host: &UiHost, data: ToolData, row: usize, earlier: &[usize]) -> RenderReply {
        click_call(host, CALL_ID, data, row, earlier)
    }

    fn clicked_body(host: &UiHost, row: usize, earlier: &[usize]) -> String {
        drawn_text(click(host, tool_data(TOGGLE), row, earlier))
            .1
            .expect("the toggle renderer always draws a body")
    }

    /// The toggle's body after {clicks}: open after an odd count, and every
    /// row in the order the clicks reached it.
    fn toggle_body(clicks: &[usize]) -> String {
        let state = if clicks.len() % 2 == 1 { OPEN } else { CLOSED };
        let rows: Vec<String> = clicks.iter().map(usize::to_string).collect();
        format!("{state}\n{CLICKS} {}", rows.join(","))
    }

    fn host_with_clicks() -> UiHost {
        let host = UiHost::bare();
        host.load_source(PLUGIN, &click_source()).unwrap();
        host
    }

    fn drawn_text(reply: RenderReply) -> (Option<String>, Option<String>) {
        let RenderReply::Drawn { header, body } = reply else {
            panic!("expected the renderer to draw, got {reply:?}");
        };
        let text = |snapshot: Option<BufferSnapshot>| snapshot.map(|s| s.text());
        (text(header), text(body))
    }

    fn flashed(host: &UiHost) -> String {
        let actions = host.ui_action_rx();
        loop {
            match actions.recv_timeout(WAIT).expect(NEVER_FLASHED) {
                UiAction::Flash(text) => break text,
                _ => continue,
            }
        }
    }

    #[test]
    fn a_renderer_draws_its_header_and_body_from_the_data() {
        let host = host_with_fixture();
        let (header, body) = drawn_text(render(&host, tool_data(TOOL)));
        assert_eq!(header.as_deref(), Some(format!("{CMD} in {CWD}").as_str()));
        assert_eq!(
            body.as_deref(),
            Some(format!("{STATE_LINE}\n{OUTPUT_TEXT}\nlive {WIDTH}").as_str())
        );
    }

    #[test_case(RAISER ; "raising_renderer")]
    #[test_case(MISSING ; "no_renderer")]
    fn a_failing_or_missing_renderer_draws_the_default(tool: &str) {
        let host = host_with_fixture();
        assert_eq!(render(&host, tool_data(tool)), RenderReply::Default);
    }

    fn rewritten(tool: &str) -> ToolData {
        let mut data = tool_data(tool);
        data.mark_rewritten();
        data
    }

    /// A registered child draws through its renderer; a missing one, and one whose output a hook changed, through the
    /// default; a raising one gives nil.
    #[test]
    fn a_renderer_composes_its_children() {
        let host = host_with_fixture();
        let mut parent = tool_data(PARENT);
        parent.children = [
            tool_data(TOOL),
            tool_data(MISSING),
            tool_data(RAISER),
            rewritten(TOOL),
        ]
        .map(done_child)
        .into();
        let (header, body) = drawn_text(render(&host, parent));
        assert_eq!(header, None);
        let expected = [
            format!("{CMD} in {CWD} true"),
            format!("{SUMMARY} false"),
            "nil false".to_owned(),
            format!("{SUMMARY} false"),
        ]
        .join("\n");
        assert_eq!(body.as_deref(), Some(expected.as_str()));
    }

    /// `composer` hands each click to every child through
    /// `maki.ui.renderer`, keeps what each composed click answered in its
    /// header, and draws each child's body lines, or `none`, below.
    fn composer_source() -> String {
        format!(
            r#"
            local function text(line)
                local parts = {{}}
                for _, span in ipairs(line) do table.insert(parts, span[1]) end
                return table.concat(parts)
            end
            maki.ui.register_renderer("{COMPOSER}", {{
                header = function(data) return table.concat(data.answers or {{}}, ",") end,
                body = function(data, ctx)
                    local lines = {{}}
                    for _, child in ipairs(data.children) do
                        local drawn = maki.ui.renderer(child.tool).body(child, ctx)
                        if drawn then
                            for _, line in ipairs(drawn:get_lines()) do table.insert(lines, text(line)) end
                        else
                            table.insert(lines, "{NO_BODY}")
                        end
                    end
                    local buf = maki.ui.buf()
                    buf:set_lines(lines)
                    return buf
                end,
                click = function(data, ctx, row)
                    data.answers = {{}}
                    for _, child in ipairs(data.children) do
                        local answer = maki.ui.renderer(child.tool).click(child, ctx, row)
                        table.insert(data.answers, tostring(answer))
                    end
                end,
            }})
            "#
        )
    }

    fn done_child(data: ToolData) -> ToolChild {
        ToolChild {
            index: 0,
            status: ChildStatus::Done,
            data,
            output: Some(output()),
            usage: None,
        }
    }

    /// Only the registered child takes the click and draws what it
    /// changed; a missing or rewritten one does nothing. Every
    /// composed click answers nil, though `toggle`'s click returns a value.
    #[test]
    fn a_composed_click_reaches_only_a_listed_child() {
        const ROW: usize = 4;
        let host = host_with_clicks();
        host.load_source(OTHER_PLUGIN, &composer_source()).unwrap();
        let mut parent = tool_data(COMPOSER);
        parent.children = [tool_data(TOGGLE), tool_data(MISSING), rewritten(TOGGLE)]
            .map(done_child)
            .into();
        let (header, body) = drawn_text(click(&host, parent, ROW, &[]));
        assert_eq!(header.as_deref(), Some(["nil"; 3].join(",").as_str()));
        let expected = [toggle_body(&[ROW]), [NO_BODY; 2].join("\n")].join("\n");
        assert_eq!(body.as_deref(), Some(expected.as_str()));
        let flashes: Vec<String> = host
            .ui_action_rx()
            .drain()
            .filter_map(|action| match action {
                UiAction::Flash(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(flashes, [format!("{CLICKED} {ROW}")]);
    }

    /// A rewritten call skips its callbacks; the next ordinary call still runs.
    #[test_case(TOOL, None, DREW.to_owned() ; "render")]
    #[test_case(TOGGLE, Some(1), format!("{CLICKED} 1") ; "click")]
    fn a_rewritten_call_draws_the_default_without_calling_the_renderer(
        tool: &str,
        row: Option<usize>,
        flash: String,
    ) {
        let host = host_with_fixture();
        host.load_source(OTHER_PLUGIN, &click_source()).unwrap();
        let send = |data| match row {
            Some(row) => click(&host, data, row, &[]),
            None => render(&host, data),
        };
        assert_eq!(send(rewritten(tool)), RenderReply::Default);
        drawn_text(send(tool_data(tool)));
        assert_eq!(flashed(&host), flash);
    }

    fn cached_header(host: &UiHost) -> Option<String> {
        let mut parent = tool_data(CACHED);
        parent.children = vec![ToolChild {
            index: 0,
            status: ChildStatus::Done,
            data: tool_data(TOOL),
            output: Some(output()),
            usage: None,
        }];
        drawn_text(render(host, parent)).0
    }

    #[derive(Debug, Clone, Copy)]
    enum Change {
        Unload,
        Takeover,
    }

    /// A kept `maki.ui.renderer` table draws with the renderer registered
    /// when it is called, not the one registered when it was made.
    #[test_case(Change::Unload, SUMMARY ; "unload")]
    #[test_case(Change::Takeover, TAKEN_OVER ; "takeover")]
    fn a_kept_composed_renderer_follows_the_current_one(change: Change, expected: &str) {
        let host = host_with_fixture();
        host.load_source(CACHER, &cacher_source()).unwrap();
        assert_eq!(
            cached_header(&host).as_deref(),
            Some(format!("{CMD} in {CWD}").as_str())
        );
        match change {
            Change::Unload => host.unload(PLUGIN).unwrap(),
            Change::Takeover => host
                .load_source(
                    OTHER_PLUGIN,
                    &format!(
                        "maki.ui.register_renderer('{TOOL}', {{ \
                         header = function() return '{TAKEN_OVER}' end }})"
                    ),
                )
                .unwrap(),
        }
        assert_eq!(cached_header(&host).as_deref(), Some(expected));
    }

    /// The earlier clicks a warm call is given are not replayed: its data
    /// holds them already.
    #[test]
    fn a_click_on_a_warm_call_changes_its_body_in_place() {
        let host = host_with_clicks();
        drawn_text(render_call(&host, CALL_ID, tool_data(TOGGLE)));
        assert_eq!(clicked_body(&host, 1, &[]), toggle_body(&[1]));
        assert_eq!(clicked_body(&host, 2, &[7, 7]), toggle_body(&[1, 2]));
    }

    /// The cap's worth of later calls pushes the call out of the cache, and
    /// the replay lands where the warm path did. The flashes show the
    /// replay: the warm click, then both clicks again.
    #[test]
    fn a_click_after_eviction_replays_to_the_same_body() {
        let host = host_with_clicks();
        drawn_text(render_call(&host, CALL_ID, tool_data(TOGGLE)));
        assert_eq!(clicked_body(&host, 1, &[]), toggle_body(&[1]));
        for other in 0..WARM_CALL_CAP {
            drawn_text(render_call(&host, &other.to_string(), tool_data(TOGGLE)));
        }
        assert_eq!(clicked_body(&host, 2, &[1]), toggle_body(&[1, 2]));
        let rows: Vec<String> = (0..3).map(|_| flashed(&host)).collect();
        assert_eq!(rows, [1, 1, 2].map(|row| format!("{CLICKED} {row}")));
    }

    #[test]
    fn a_cold_click_replays_the_earlier_clicks_in_order() {
        let host = host_with_clicks();
        assert_eq!(clicked_body(&host, 2, &[3, 1]), toggle_body(&[3, 1, 2]));
    }

    #[test]
    fn a_reload_clears_the_warm_calls() {
        let host = host_with_clicks();
        drawn_text(render_call(&host, CALL_ID, tool_data(TOGGLE)));
        clicked_body(&host, 1, &[]);
        host.load_source(PLUGIN, &click_source()).unwrap();
        assert_eq!(clicked_body(&host, 2, &[5]), toggle_body(&[5, 2]));
    }

    #[test]
    fn a_raising_click_draws_the_default() {
        let host = host_with_clicks();
        drawn_text(render_call(&host, CALL_ID, tool_data(BAD_CLICK)));
        assert_eq!(
            click(&host, tool_data(BAD_CLICK), 1, &[]),
            RenderReply::Default
        );
    }

    /// Each click yields between reading the count and writing it, so two
    /// clicks running at once would both count from zero.
    #[test]
    fn overlapping_clicks_of_a_call_take_turns() {
        let host = host_with_clicks();
        drawn_text(render(&host, tool_data(COUNTER)));
        let handle = host.ui_handle();
        let clicks: Vec<_> = (0..2)
            .map(|_| handle.click(call(CALL_ID, tool_data(COUNTER)), ctx(), 1, Vec::new()))
            .collect();
        let headers: Vec<Option<String>> = clicks
            .into_iter()
            .map(|reply| drawn_text(reply.recv_timeout(WAIT).unwrap()).0)
            .collect();
        assert_eq!(headers, [Some("1".to_owned()), Some("2".to_owned())]);
    }

    /// The second click's row exists only in the layout the first click's
    /// draw left, so a replay must draw between the clicks too.
    #[test]
    fn a_replay_reads_each_click_against_the_layout_before_it() {
        let host = host_with_clicks();
        drawn_text(render(&host, tool_data(LAYOUT)));
        click(&host, tool_data(LAYOUT), 1, &[]);
        let warm = drawn_text(click(&host, tool_data(LAYOUT), 3, &[1])).1;
        let cold = drawn_text(click_call(&host, COLD_CALL_ID, tool_data(LAYOUT), 3, &[1])).1;
        assert_eq!(warm.as_deref(), Some("a\na detail\nb\nb detail"));
        assert_eq!(cold, warm);
    }

    /// The second click waits for the first one's turn when the reload
    /// arrives. Both are served by the renderer they were sent to before
    /// the reload replaces it.
    #[test]
    fn a_reload_waits_for_a_click_queued_for_its_turn() {
        let host = UiHost::bare();
        host.load_source(GATE_PLUGIN, &gated_source(None)).unwrap();
        drawn_text(render(&host, tool_data(GATED)));
        let handle = host.ui_handle();
        let gated_click = || handle.click(call(CALL_ID, tool_data(GATED)), ctx(), 1, Vec::new());
        let first = gated_click();
        let actions = host.ui_action_rx();
        let parked = loop {
            match actions.recv_timeout(WAIT).unwrap() {
                UiAction::Session {
                    req: SessionRequest::List,
                    reply_tx,
                } => break reply_tx,
                _ => continue,
            }
        };
        let second = gated_click();
        let reload = host.send_load_source(GATE_PLUGIN, &gated_source(Some(RELOADED)));
        // Taken means the reload waits at its barrier, and the first click
        // is still parked behind it.
        while host.untaken_requests() > 0 {
            thread::yield_now();
        }
        parked.send(Ok(json!([]))).unwrap();
        let header =
            |reply: flume::Receiver<RenderReply>| drawn_text(reply.recv_timeout(WAIT).unwrap()).0;
        assert_eq!(header(first).as_deref(), Some("1"));
        assert_eq!(header(second).as_deref(), Some("2"));
        reload.recv_timeout(WAIT).unwrap().unwrap();
        let (header, _) = drawn_text(render(&host, tool_data(GATED)));
        assert_eq!(header.as_deref(), Some(RELOADED));
    }

    #[test]
    fn inline_tasks_of_a_click_finish_before_the_redraw() {
        let host = host_with_clicks();
        assert_eq!(
            drawn_text(render(&host, tool_data(LATER))).1,
            Some(format!("{LATE_LINE}\n{LATER_LINE}"))
        );
        let (_, body) = drawn_text(click(&host, tool_data(LATER), 1, &[]));
        assert_eq!(body, Some(format!("{CLICKED}\n{LATE_LINE}\n{LATER_LINE}")));
    }

    /// A running call and its child read what they streamed, and a child
    /// whose record has not arrived yet takes nothing.
    #[test]
    fn streamed_text_reaches_the_call_and_its_children() {
        const INVOCATION: u32 = 2;
        let host = UiHost::bare();
        host.load_source(
            PLUGIN,
            &format!(
                "maki.ui.register_renderer('{PARENT}', {{ \
                 header = function(data) \
                   return data.stream .. ' ' .. data.children[1].stream .. ' ' .. data.children[1].index end }})"
            ),
        )
        .unwrap();
        let mut parent = tool_data(PARENT);
        parent.children = vec![ToolChild {
            index: INVOCATION,
            status: ChildStatus::Running,
            data: tool_data(TOOL),
            output: None,
            usage: None,
        }];
        let mut running = call(CALL_ID, parent);
        running.streamed = vec![
            (Vec::new(), STATE_LINE.to_owned()),
            (vec![INVOCATION], OUTPUT_TEXT.to_owned()),
            (vec![1], LATE_LINE.to_owned()),
        ];
        let reply = host
            .ui_handle()
            .render(running, ctx())
            .recv_timeout(WAIT)
            .unwrap();
        assert_eq!(
            drawn_text(reply).0,
            Some(format!("{STATE_LINE} {OUTPUT_TEXT} {}", INVOCATION + 1))
        );
    }

    /// A redraw starts from fresh data and replays the clicks over it, warm
    /// or not.
    #[test_case(false ; "cold")]
    #[test_case(true ; "warm")]
    fn a_redraw_replays_the_clicks(warm: bool) {
        let host = host_with_clicks();
        if warm {
            drawn_text(click(&host, tool_data(TOGGLE), 1, &[]));
        }
        let reply = host
            .ui_handle()
            .redraw(call(CALL_ID, tool_data(TOGGLE)), ctx(), vec![1, 2])
            .recv_timeout(WAIT)
            .unwrap();
        assert_eq!(drawn_text(reply).1, Some(toggle_body(&[1, 2])));
    }

    #[test_case(true ; "reload")]
    #[test_case(false ; "unload")]
    fn a_reload_or_unload_clears_the_plugins_renderers(reload: bool) {
        let host = host_with_fixture();
        if reload {
            host.load_source(PLUGIN, "").unwrap();
        } else {
            host.unload(PLUGIN).unwrap();
        }
        let handle = host.ui_handle();
        assert_eq!(handle.renderer_routes().route(TOOL), RenderRoute::Default);
        assert_eq!(render(&host, tool_data(TOOL)), RenderReply::Default);
    }

    #[test]
    fn a_later_plugin_takes_a_tool_over_until_it_unloads() {
        let host = UiHost::bare();
        host.load_source(PLUGIN, &renderer_source(TOOL, PLUGIN))
            .unwrap();
        host.load_source(OTHER_PLUGIN, &renderer_source(TOOL, OTHER_PLUGIN))
            .unwrap();
        assert_eq!(
            drawn_text(render(&host, tool_data(TOOL))).0.as_deref(),
            Some(OTHER_PLUGIN)
        );
        host.unload(OTHER_PLUGIN).unwrap();
        assert_eq!(
            drawn_text(render(&host, tool_data(TOOL))).0.as_deref(),
            Some(PLUGIN)
        );
    }

    #[test_case("''", "{ body = function() end }", EMPTY_TOOL_ERR ; "empty_tool")]
    #[test_case("'t'", "{ }", NO_DRAWING_ERR ; "nothing_to_draw")]
    #[test_case("'t'", "{ header = 'x' }", NOT_A_FUNCTION_ERR ; "header_not_a_function")]
    #[test_case("'t'", "{ header = function() end, click = 'x' }", NOT_A_FUNCTION_ERR ; "click_not_a_function")]
    fn registration_rejects(tool: &str, spec: &str, expected: &str) {
        let host = UiHost::bare();
        let err = host
            .load_source(
                PLUGIN,
                &format!("maki.ui.register_renderer({tool}, {spec})"),
            )
            .unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
    }
}
