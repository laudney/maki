use std::sync::Arc;

use maki_highlight::SegmentColor;
use maki_lua_macro::{lua_class, lua_fn};
use maki_types::snapshot::{
    DefaultColor, InlineStyle, SharedBuf, SnapshotLine, SnapshotSpan, SpanColor, SpanStyle,
};
use mlua::{Function, Lua, Result as LuaResult, Table, Value as LuaValue};

use super::blit;
use super::render::segment_color_to_lua;
use crate::buffer::api::util::convert::TableExt;
use crate::buffer::runtime::TaskHandle;
use crate::buffer::task::with_bufs;

#[derive(Default)]
pub struct BufferStore {
    slots: Vec<HandlerSlot>,
}

/// `buf:on(...)` roots a Lua closure from the Rust side, and that closure
/// usually captures its own handle. The Lua GC cannot see this cycle, so
/// each registration is tracked on the task that made it and `clear`
/// (TaskScope drop) breaks the cycle when the task retires.
pub(crate) enum HandlerSlot {
    Click(Arc<SharedBuf>),
    Change(Arc<SharedBuf>),
}

impl HandlerSlot {
    fn clear(&self) {
        match self {
            Self::Click(buf) => buf.clear_click(),
            Self::Change(buf) => buf.clear_on_change(),
        }
    }
}

fn track_slot(lua: &Lua, slot: HandlerSlot) {
    if let Some(h) = lua.app_data_ref::<TaskHandle>() {
        with_bufs(&h, |bufs| bufs.store.track(slot));
    }
}

impl BufferStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&mut self) -> BufHandle {
        BufHandle {
            buf: Arc::new(SharedBuf::new()),
        }
    }

    pub(crate) fn track(&mut self, slot: HandlerSlot) {
        self.slots.push(slot);
    }

    pub fn clear(&mut self) {
        for slot in self.slots.drain(..) {
            slot.clear();
        }
    }
}

/// The click handler lives on the `SharedBuf` itself, so every handle
/// wrapping the same buf (clones, replies, restores, foreign wrappers from
/// another task) routes clicks to the handler the owner registered.
#[derive(Clone)]
pub struct BufHandle {
    pub buf: Arc<SharedBuf>,
}

impl BufHandle {
    pub(crate) fn click_fn(&self) -> Option<Function> {
        click_fn(&self.buf)
    }

    fn set_click(&self, f: Function) {
        self.buf.set_click(Arc::new(f));
    }
}

pub fn click_fn(buf: &SharedBuf) -> Option<Function> {
    let f = buf.click()?.downcast::<Function>().ok()?;
    Some((*f).clone())
}

/// Appends a single line to the end of the buffer. You can pass a
/// plain string for unstyled text, or a table of `{text, style?}` spans
/// for rich content. Style can be a named string like "bold" or
/// "keyword", or an inline table `{fg?, bg?, bold?, italic?, underline?, dim?, strikethrough?, reversed?}`.
/// Colors accept "#rrggbb", a terminal color name like "blue" or "light-gray",
/// or a palette index as a string like "4". Names must be spelled exactly,
/// hyphens included. Named and indexed colors are left for the terminal to
/// resolve, so they follow the user's palette. To mix a named style with colors
/// of your own, `maki.ui.theme_style` hands back its parts.
///
/// @param line string|table Plain string, or a sequence of spans: `{ {text, style?}, ... }`.
/// @return
/// @example
/// buf:line("plain text")
/// buf:line({ { "ERROR", { fg = "#ff0000", bold = true } }, { " something broke" } })
#[lua_fn]
fn line(_lua: &Lua, this: &BufHandle, line: LuaValue) -> LuaResult<()> {
    let l = parse_line(&line)?;
    this.buf.append(l);
    Ok(())
}

/// Appends several lines at once. Each entry uses the same format as
/// `buf:line()`, so you can mix plain strings and styled spans.
///
/// @param lines table Sequence of line values, each the same format accepted by `buf:line`.
/// @return
/// @example
/// buf:lines({
///   "first line",
///   { { "styled ", "bold" }, { "second line" } },
///   "third line",
/// })
#[lua_fn]
fn lines(_lua: &Lua, this: &BufHandle, lines: Table) -> LuaResult<()> {
    let mut parsed = Vec::with_capacity(lines.raw_len());
    for i in 1..=lines.raw_len() {
        let val: LuaValue = lines.raw_get(i)?;
        parsed.push(parse_line(&val)?);
    }
    for l in parsed {
        this.buf.append(l);
    }
    Ok(())
}

/// Replaces every line in the buffer with {lines}. Use this when you
/// want to redraw the whole buffer, for example after the user toggles
/// a view.
///
/// `head` counts the leading lines that say what the buffer is about, like
/// the command above a bash output. When the tool call moves to the
/// background, its row in the chat keeps only those lines, because the rest
/// streams in the task's tab. Without a head the row keeps just its title.
///
/// @param lines table Sequence of line values, each the same format accepted by `buf:line`.
/// @param opts table|nil Options: `head` = number of leading lines (default 0).
/// @return
/// @example
/// buf:set_lines({ "new content", "replaces everything" })
/// buf:set_lines({ "cargo test", "──────", "running 3 tests" }, { head = 1 })
#[lua_fn]
fn set_lines(_lua: &Lua, this: &BufHandle, lines: Table, opts: Option<Table>) -> LuaResult<()> {
    let mut parsed = Vec::with_capacity(lines.raw_len());
    for i in 1..=lines.raw_len() {
        let val: LuaValue = lines.raw_get(i)?;
        parsed.push(parse_line(&val)?);
    }
    let head = match opts {
        Some(opts) => opts.opt::<usize>("head")?.unwrap_or(0),
        None => 0,
    };
    this.buf.set_lines_with_head(parsed, head);
    Ok(())
}

/// Returns how many lines the buffer currently holds.
///
/// @return (integer) Line count.
/// @example
/// if buf:len() == 0 then
///   buf:line("(empty)")
/// end
#[lua_fn]
fn len(_lua: &Lua, this: &BufHandle) -> LuaResult<usize> {
    Ok(this.buf.len())
}

/// Returns all lines in the buffer as a Lua table. Each line is a
/// sequence of `{text, style?}` spans, the same format `buf:line()`
/// accepts. Useful for reading back content, copying it to another
/// buffer, or round-tripping through `set_lines()`.
///
/// @return (table) Sequence of lines.
/// @example
/// local lines = buf:get_lines()
/// buf:set_lines(lines) -- round-trip
#[lua_fn]
fn get_lines(lua: &Lua, this: &BufHandle) -> LuaResult<Table> {
    let ls = this.buf.read();
    let out = lua.create_table_with_capacity(ls.len(), 0)?;
    for (i, l) in ls.iter().enumerate() {
        out.raw_set(i + 1, line_to_lua(lua, l)?)?;
    }
    Ok(out)
}

/// Registers an event handler on the buffer.
///
/// Supported events:
/// - "click": fires when the user clicks a line. The handler receives
///   a click-event table and may yield or mutate the buffer.
/// - "change": fires synchronously after every mutation (`line`,
///   `lines`, `set_lines`). Must not yield.
///
/// Calling `on()` again for the same event replaces the previous handler.
///
/// @param event string Event name: "click" or "change".
/// @param callback function Handler function. For "click", receives a click-event table. For "change", receives no arguments.
/// @return
/// @example
/// buf:on("click", function(ev)
///   maki.ui.flash("Clicked row " .. ev.row)
/// end)
#[lua_fn]
fn on(lua: &Lua, this: &BufHandle, event: String, callback: Function) -> LuaResult<()> {
    match event.as_str() {
        "click" => {
            this.set_click(callback);
            track_slot(lua, HandlerSlot::Click(Arc::clone(&this.buf)));
            Ok(())
        }
        // Change callbacks fire inline from buf mutations, so they
        // must not yield or mutate this buffer.
        "change" => {
            this.buf.set_on_change(move || {
                if let Err(e) = callback.call::<()>(()) {
                    tracing::warn!(error = %e, "buf change callback failed");
                }
            });
            track_slot(lua, HandlerSlot::Change(Arc::clone(&this.buf)));
            Ok(())
        }
        _ => Err(mlua::Error::runtime(format!("unsupported event: {event}"))),
    }
}

/// Programmatically fires the buffer's click handler with event {ev}.
/// Does nothing if no click handler is registered. Useful for testing
/// or simulating user interaction from code.
///
/// @param ev table Click event table passed to the handler.
/// @return
/// @example
/// buf:click({ row = 1 })
#[lua_fn]
async fn click(_lua: Lua, this: mlua::UserDataRef<BufHandle>, ev: LuaValue) -> LuaResult<()> {
    // Extract the handler before the await: holding the UserDataRef
    // across it would block the handler's own calls back into this buf
    // (mlua `send` borrows are exclusive).
    let f = this.click_fn();
    drop(this);
    match f {
        Some(f) => f.call_async::<()>(ev).await,
        None => Ok(()),
    }
}

/// Replaces the whole buffer with a pixel frame drawn as `"▀"` cells.
/// Each cell's foreground is the top pixel and its background the
/// bottom one, so one text line fits two pixel rows. When {height} is
/// odd the last line leaves its background unset and the terminal
/// default shows through.
///
/// {fb} is a Luau `buffer` of raw pixel bytes in row-major order,
/// top-left origin. Its size must be exactly
/// `width * height * bytes_per_pixel` for the chosen format, otherwise
/// the call throws. A mismatch usually means a wrong width or format,
/// and an early error beats hunting down a garbled frame.
///
/// Formats: "rgb" is the default at 3 bytes per pixel. "rgba" and
/// "bgra" take 4 bytes per pixel and ignore the 4th byte. "bgra" is
/// what a little-endian `uint32` holding `0xRRGGBB` looks like in
/// memory, the layout doomgeneric uses for its framebuffer.
///
/// `char` swaps the `"▀"` glyph for another one column wide string,
/// e.g. `"█"` when only the foreground color should show. The
/// foreground still comes from the top pixel and the background from
/// the bottom one, whatever the glyph.
///
/// @param fb buffer Raw pixel bytes.
/// @param width integer Frame width in pixels, > 0.
/// @param height integer Frame height in pixels, > 0.
/// @param opts table|nil Options: `format` = "rgb"|"rgba"|"bgra", `char` = one column wide string.
/// @return
/// @example
/// local fb = buffer.create(160 * 100 * 3)
/// buffer.writeu8(fb, (y * 160 + x) * 3, 255) -- red channel
/// buf:blit(fb, 160, 100)
/// buf:blit(fb32, 160, 100, { format = "bgra", char = "█" })
#[lua_fn]
fn blit(
    _lua: &Lua,
    this: &BufHandle,
    fb: mlua::Buffer,
    width: u32,
    height: u32,
    opts: Option<Table>,
) -> LuaResult<()> {
    let mut format = blit::DEFAULT_FORMAT.to_owned();
    let mut cell = blit::DEFAULT_CELL.to_owned();
    if let Some(opts) = opts {
        for pair in opts.pairs::<String, String>() {
            match pair? {
                (key, val) if key == "format" => format = val,
                (key, val) if key == "char" => cell = val,
                (key, _) => {
                    return Err(mlua::Error::runtime(format!(
                        "blit: unknown opts key {key:?}"
                    )));
                }
            }
        }
    }
    let fmt = blit::parse_format(&format).map_err(mlua::Error::external)?;
    let lines = blit::render(&fb.to_vec(), width as usize, height as usize, fmt, &cell)
        .map_err(mlua::Error::external)?;
    this.buf.set_lines(lines);
    Ok(())
}

lua_class! {
    /// A content buffer that holds styled lines of text. Create one with
    /// `maki.ui.buf()` and pass it to `maki.ui.open_win()` to show it in
    /// a floating or split window.
    ///
    /// ```lua
    /// local buf = maki.ui.buf()
    /// buf:line("hello")
    /// buf:line({ { "world", "bold" } })
    /// ```
    "maki.ui.Buf" => BufHandle, DOCS [line, lines, set_lines, len, get_lines, on, click, blit]
}

pub fn parse_line(arg: &LuaValue) -> LuaResult<SnapshotLine> {
    match arg {
        LuaValue::String(s) => {
            let text = s.to_str().map_err(mlua::Error::external)?.to_owned();
            Ok(SnapshotLine {
                spans: vec![SnapshotSpan {
                    text,
                    style: SpanStyle::Default,
                }],
            })
        }
        LuaValue::Table(t) => {
            let mut spans = Vec::new();
            for i in 1..=t.raw_len() {
                let entry: LuaValue = t.raw_get(i)?;
                spans.push(parse_span(&entry)?);
            }
            Ok(SnapshotLine { spans })
        }
        _ => Err(mlua::Error::runtime(
            "line argument must be a string or table of spans",
        )),
    }
}

fn parse_span(val: &LuaValue) -> LuaResult<SnapshotSpan> {
    let LuaValue::Table(t) = val else {
        return Err(mlua::Error::runtime("span must be a table {text, style?}"));
    };
    let text_val: LuaValue = t.raw_get(1)?;
    let text = match &text_val {
        LuaValue::String(s) => s.to_str().map_err(mlua::Error::external)?.to_owned(),
        _ => return Err(mlua::Error::runtime("span[1] must be a string")),
    };
    let style_val: LuaValue = t.raw_get(2)?;
    let style = parse_style(&style_val)?;
    Ok(SnapshotSpan { text, style })
}

pub fn parse_style(val: &LuaValue) -> LuaResult<SpanStyle> {
    match val {
        LuaValue::Nil => Ok(SpanStyle::Default),
        v if v.is_null() => Ok(SpanStyle::Default),
        LuaValue::String(s) => {
            let name = s.to_str().map_err(mlua::Error::external)?.to_owned();
            Ok(SpanStyle::Named(name))
        }
        LuaValue::Table(t) => {
            let mut inline = InlineStyle::default();
            if let Ok(LuaValue::String(s)) = t.raw_get::<LuaValue>("fg") {
                inline.fg = parse_span_color(&s.to_str().map_err(mlua::Error::external)?);
            }
            if let Ok(LuaValue::String(s)) = t.raw_get::<LuaValue>("bg") {
                inline.bg = parse_span_color(&s.to_str().map_err(mlua::Error::external)?);
            }
            inline.bold = t.opt_bool("bold").unwrap_or(false);
            inline.italic = t.opt_bool("italic").unwrap_or(false);
            inline.underline = t.opt_bool("underline").unwrap_or(false);
            inline.dim = t.opt_bool("dim").unwrap_or(false);
            inline.strikethrough = t.opt_bool("strikethrough").unwrap_or(false);
            inline.reversed = t.opt_bool("reversed").unwrap_or(false);
            Ok(SpanStyle::Inline(inline))
        }
        _ => Err(mlua::Error::runtime(
            "style must be nil, a string name, or a table {fg?, bg?, bold?, ...}",
        )),
    }
}

pub fn line_to_lua(lua: &Lua, line: &SnapshotLine) -> LuaResult<Table> {
    let tbl = lua.create_table_with_capacity(line.spans.len(), 0)?;
    for (i, span) in line.spans.iter().enumerate() {
        tbl.raw_set(i + 1, span_to_lua(lua, span)?)?;
    }
    Ok(tbl)
}

fn span_to_lua(lua: &Lua, span: &SnapshotSpan) -> LuaResult<Table> {
    let tbl = lua.create_table_with_capacity(2, 0)?;
    tbl.raw_set(1, span.text.as_str())?;
    match &span.style {
        SpanStyle::Default => {}
        SpanStyle::Named(name) => tbl.raw_set(2, name.as_str())?,
        SpanStyle::Inline(inline) => tbl.raw_set(2, inline_to_lua(lua, inline)?)?,
    }
    Ok(tbl)
}

fn inline_to_lua(lua: &Lua, inline: &InlineStyle) -> LuaResult<Table> {
    let tbl = lua.create_table()?;
    if let Some(c) = inline.fg {
        tbl.raw_set("fg", segment_color_to_lua(segment_color_from_span(c)))?;
    }
    if let Some(c) = inline.bg {
        tbl.raw_set("bg", segment_color_to_lua(segment_color_from_span(c)))?;
    }
    for (key, on) in [
        ("bold", inline.bold),
        ("italic", inline.italic),
        ("underline", inline.underline),
        ("dim", inline.dim),
        ("strikethrough", inline.strikethrough),
        ("reversed", inline.reversed),
    ] {
        if on {
            tbl.raw_set(key, true)?;
        }
    }
    Ok(tbl)
}

/// Plugins speak the same `#rrggbb | index | name | default` grammar as
/// themes, so both directions go through [`SegmentColor`] and cannot drift.
/// The orphan rule rules out `From`, both types are foreign here.
fn parse_span_color(s: &str) -> Option<SpanColor> {
    Some(match SegmentColor::parse(s)? {
        SegmentColor::Rgb(rgb) => SpanColor::Rgb(rgb),
        SegmentColor::Ansi(i) => SpanColor::Ansi(i),
        SegmentColor::Default => SpanColor::Default(DefaultColor::Default),
    })
}

fn segment_color_from_span(c: SpanColor) -> SegmentColor {
    match c {
        SpanColor::Rgb(rgb) => SegmentColor::Rgb(rgb),
        SpanColor::Ansi(i) => SegmentColor::Ansi(i),
        SpanColor::Default(_) => SegmentColor::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("42" ; "line_type")]
    #[test_case("{true}" ; "span_type")]
    #[test_case("{{42}}" ; "text_type")]
    #[test_case("{{'text', 42}}" ; "style_type")]
    fn invalid_lines_are_refused(source: &str) {
        let lua = test_lua();
        let value = lua
            .load(format!("return {source}"))
            .eval::<LuaValue>()
            .unwrap();
        assert!(parse_line(&value).is_err());
    }

    #[test_case("{}" ; "empty_style")]
    #[test_case("{fg = 'not_a_color'}" ; "invalid_color")]
    fn unusable_style_fields_fall_back_to_defaults(source: &str) {
        let lua = test_lua();
        let value = lua
            .load(format!("return {source}"))
            .eval::<LuaValue>()
            .unwrap();
        assert_eq!(
            parse_style(&value).unwrap(),
            SpanStyle::Inline(InlineStyle::default())
        );
    }

    #[test]
    fn buf_click_can_replace_a_handler_and_mutate_its_buffer() {
        let lua = test_lua();
        maki_lua_core::runtime::block_on(
            lua.load(
                r#"
            buf:click({ row = 3 })
            buf:on("click", function() error("replaced handler ran") end)
            buf:on("click", function(ev) buf:set_lines({ tostring(ev.row) }) end)
            buf:click({ row = 3 })
        "#,
            )
            .exec_async(),
        )
        .unwrap();
        assert_eq!(
            lua.load("return buf:get_lines()[1][1][1]")
                .eval::<String>()
                .unwrap(),
            "3"
        );
    }

    fn test_lua() -> mlua::Lua {
        let lua = mlua::Lua::new();
        lua.set_app_data(BufferStore::new());
        set_buf_global(&lua);
        lua
    }

    #[test_case(r#"buf:set_lines({ "cmd", "out" }, { head = 1 })"#, 1 ; "kept")]
    #[test_case(r#"buf:set_lines({ "cmd" }, { head = 1 }) buf:set_lines({ "out" })"#, 0 ; "cleared_on_redraw")]
    fn set_lines_head_names_the_leading_lines(script: &str, head: usize) {
        let lua = test_lua();
        lua.load(script).exec().unwrap();
        let buf = lua.globals().get::<mlua::AnyUserData>("buf").unwrap();
        assert_eq!(buf.borrow::<BufHandle>().unwrap().buf.take().head, head);
    }

    #[test]
    fn get_lines_round_trips_styles() {
        let lua = test_lua();

        lua.load(
            r##"
            buf:line("first")
            buf:lines({ "second", "third" })
            assert(buf:len() == 3)
            buf:set_lines({
                "plain",
                { { "named ", "tool" }, { "rest" } },
                { { "inline", { fg = "#ff8000", bg = "#001122", bold = true, dim = true } } },
                {},
            })
            assert(buf:len() == 4)
            buf:set_lines(buf:get_lines())
            "##,
        )
        .exec()
        .unwrap();

        let ud: mlua::AnyUserData = lua.globals().get("buf").unwrap();
        let lines = ud.borrow::<BufHandle>().unwrap().buf.read();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].spans[0].text, "plain");
        assert_eq!(lines[0].spans[0].style, SpanStyle::Default);
        assert_eq!(lines[1].spans[0].style, SpanStyle::Named("tool".into()));
        assert_eq!(lines[1].spans[1].style, SpanStyle::Default);
        assert_eq!(
            lines[2].spans[0].style,
            SpanStyle::Inline(InlineStyle {
                fg: Some(SpanColor::Rgb((255, 128, 0))),
                bg: Some(SpanColor::Rgb((0, 17, 34))),
                bold: true,
                dim: true,
                ..InlineStyle::default()
            })
        );
        assert!(lines[3].spans.is_empty());
    }

    #[test]
    fn buf_on_unsupported_event_errors() {
        let lua = test_lua();

        let result = lua.load(r#"buf:on("hover", function() end)"#).exec();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unsupported event"), "got: {err}");
    }

    fn set_buf_global(lua: &mlua::Lua) {
        let handle = {
            let mut store = lua.app_data_mut::<BufferStore>().unwrap();
            store.create()
        };
        let ud = lua.create_userdata(handle).unwrap();
        lua.globals().set("buf", ud).unwrap();
    }

    #[test]
    fn blit_replaces_content_with_rendered_frame() {
        let lua = test_lua();

        lua.load(
            r#"
            buf:set_lines({ "a", "b", "c" })
            local fb = buffer.create(2 * 2 * 3)
            buffer.writeu8(fb, 0, 255)
            buffer.writeu8(fb, 4, 255)
            buffer.writeu8(fb, 8, 255)
            buf:blit(fb, 2, 2)
            "#,
        )
        .exec()
        .unwrap();

        let mut bytes = [0u8; 12];
        (bytes[0], bytes[4], bytes[8]) = (255, 255, 255);
        let fmt = blit::parse_format(blit::DEFAULT_FORMAT).unwrap();
        let expected = blit::render(&bytes, 2, 2, fmt, blit::DEFAULT_CELL).unwrap();
        let ud: mlua::AnyUserData = lua.globals().get("buf").unwrap();
        assert_eq!(*ud.borrow::<BufHandle>().unwrap().buf.read(), expected);
    }

    #[test_case(r#"buf:blit(buffer.create(3), 1, 1, { fromat = "bgra" })"#, "unknown opts key" ; "opts_key_typo")]
    #[test_case(r#"buf:blit(buffer.create(3), 1, 1, { format = "argb" })"#, "unknown format" ; "unknown_format")]
    #[test_case(r#"buf:blit(buffer.create(5), 1, 1)"#, "needs exactly 3" ; "wrong_size")]
    fn blit_throws(code: &str, expected: &str) {
        let lua = test_lua();

        let err = lua.load(code).exec().unwrap_err().to_string();
        assert!(err.contains(expected), "expected {expected:?} in: {err}");
    }

    #[test]
    fn buf_on_change_fires_for_mutations_without_recursion() {
        let lua = test_lua();

        lua.load(
            r#"
            changes = 0
            buf:on("change", function()
                changes = changes + 1
                -- Mutating the watched buf must not recurse.
                if changes == 1 then buf:line("from-watcher") end
            end)
            buf:line("a")
            buf:lines({ "b", "c" })
            buf:set_lines({ "x" })
            "#,
        )
        .exec()
        .unwrap();

        // line + 2x lines + set_lines = 4; the recursive append was dropped.
        let changes: i64 = lua.globals().get("changes").unwrap();
        assert_eq!(changes, 4);
    }
}

#[cfg(test)]
mod palette_roundtrip {
    use super::*;
    use test_case::test_case;

    #[test_case(SpanColor::Ansi(4); "palette index")]
    #[test_case(SpanColor::Default(DefaultColor::Default); "terminal default")]
    #[test_case(SpanColor::Rgb((0x6f, 0xb3, 0xd2)); "rgb")]
    fn colors_survive_the_lua_round_trip(color: SpanColor) {
        let text = segment_color_to_lua(segment_color_from_span(color));
        assert_eq!(parse_span_color(&text), Some(color));
    }
}
