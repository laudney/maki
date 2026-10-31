use maki_highlight::{DEFAULT_COLOR_NAME, SegmentColor};
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, Result as LuaResult, Table};

use super::buf;
use crate::buffer::api::text;
use crate::buffer::api::util::convert::TableExt;
use crate::buffer::docs::FnDoc;
use crate::buffer::task::with_task_bufs;

/// `fg`, `bg` and six modifiers.
const UI_STYLE_FIELDS: usize = 8;

/// Creates a buffer for a tool renderer or plugin window. It belongs to
/// the task that made it, so this throws at a plugin's top level.
///
/// @return (Buf) Buffer handle.
/// @example
/// local out = maki.ui.buf()
/// out:line("hello world")
#[lua_fn]
fn buf(lua: &Lua) -> LuaResult<buf::BufHandle> {
    with_task_bufs(lua, |store| store.create())
}

/// Looks up a color the syntax theme names, such as "background",
/// "foreground" or "accent". For the styles the UI paints with, use
/// `maki.ui.theme_style`.
///
/// @param name string Syntax theme color name, e.g. "accent" or "background".
/// @return (string|nil) "#rrggbb" for a truecolor theme, a palette index as a
///   string like "4" when the theme names an ANSI color, or "default" for the
///   terminal's own color. Nil only when the name is unknown. Every form can be
///   passed straight to a span's `fg`/`bg`.
/// @example
/// local accent = maki.ui.theme_color("accent")
/// if accent then
///   buf:line({ { "note", { fg = accent, bold = true } } })
/// end
#[lua_fn]
fn theme_color(lua: &Lua, name: String) -> LuaResult<mlua::Value> {
    let Some(color) = maki_highlight::theme_color(&name) else {
        return Ok(mlua::Value::Nil);
    };
    Ok(mlua::Value::String(
        lua.create_string(segment_color_to_lua(color))?,
    ))
}

/// Looks up a named style from the current theme. The names are the ones a span
/// already takes as a string ("dim", "item_selected", "keybind_section",
/// "diff_old", ...), so `{ text, "dim" }` and `theme_style("dim")` paint the
/// same. Reach for the table when you need the parts, say to keep a style's
/// foreground over a background of your own.
///
/// @param name string Style name, the same spelling a span accepts.
/// @return (table|nil) `{fg?, bg?, bold?, italic?, underline?, dim?,
///   strikethrough?, reversed?}`, ready to use as a span style. Colors are
///   spelled as in `maki.ui.theme_color`. Nil when the name is unknown, and an
///   empty table when the theme leaves that style unset.
/// @example
/// local sel = maki.ui.theme_style("item_selected")
/// local dim = maki.ui.theme_style("dim")
/// buf:line({ { "note", { fg = dim.fg, bg = sel.bg } } })
#[lua_fn]
fn theme_style(lua: &Lua, name: String) -> LuaResult<mlua::Value> {
    let Some(style) = maki_highlight::ui_style(&name) else {
        return Ok(mlua::Value::Nil);
    };
    let tbl = lua.create_table_with_capacity(0, UI_STYLE_FIELDS)?;
    for (key, color) in [("fg", style.fg), ("bg", style.bg)] {
        if let Some(c) = color {
            tbl.raw_set(key, segment_color_to_lua(c))?;
        }
    }
    for (key, on) in [
        ("bold", style.bold),
        ("italic", style.italic),
        ("underline", style.underline),
        ("dim", style.dim),
        ("strikethrough", style.strikethrough),
        ("reversed", style.reversed),
    ] {
        if on {
            tbl.raw_set(key, true)?;
        }
    }
    Ok(mlua::Value::Table(tbl))
}

/// Syntax-highlights a chunk of source code. Returns a table of styled
/// lines that you can feed into a buffer. Each line is a list of
/// `{text, style}` spans where style is a `{fg, bold?, italic?, underline?}` table.
///
/// `fg` is "#rrggbb" for a truecolor theme, a palette index as a string like
/// "4" when the theme names an ANSI color, or "default" for the terminal's own
/// color. Pass the span straight to `buf:line` and it resolves correctly in
/// every case.
///
/// @param code string Source text to highlight.
/// @param lang string Language identifier, e.g. "rust", "python".
/// @param opts table? Options. Fields:
///   - independent (boolean): highlight each line without cross-line context. Default false.
///   - prefix (string): preceding source used as token context. Only the nearest lines fitting within 16 KiB are parsed, so earlier comments or strings may affect coloring. Default "".
/// @return (table) Lines: `{ { {text, style}, ... }, ... }`. Each style is `{fg, bold?, italic?, underline?}`.
/// @example
/// local lines = maki.ui.highlight("fn main() {}", "rust")
/// for _, spans in ipairs(lines) do
///   buf:line(spans)
/// end
#[lua_fn]
async fn highlight(lua: Lua, code: String, lang: String, opts: Option<Table>) -> LuaResult<Table> {
    let independent = opts
        .as_ref()
        .and_then(|t| t.opt_bool("independent"))
        .unwrap_or(false);
    let prefix = opts
        .and_then(|t| t.get::<String>("prefix").ok())
        .unwrap_or_default();
    let segments = maki_highlight::pool::run_async(move || {
        if independent {
            maki_highlight::highlight_lines_independent(&lang, &code)
        } else {
            maki_highlight::highlight_code(&lang, &code, &prefix)
        }
    })
    .await;
    segments_to_lua_lines(&lua, &segments)
}

/// Renders Markdown into styled lines ready to display in a buffer.
/// Each span's style is either a named string ("bold", "heading",
/// "inline_code", etc.) or a `{fg, bold?, italic?, underline?}` table
/// for syntax-highlighted code blocks.
///
/// @param text string Markdown source.
/// @param width integer Wrap width in columns.
/// @return (table) Lines: `{ { {text, style}, ... }, ... }`.
/// @example
/// local size = maki.ui.terminal_size()
/// local lines = maki.ui.markdown("# Hello\n\nSome **bold** text.", size.cols)
/// for _, spans in ipairs(lines) do
///   buf:line(spans)
/// end
#[lua_fn]
async fn markdown(lua: Lua, text: String, width: u16) -> LuaResult<Table> {
    let lines =
        maki_highlight::pool::run_async(move || maki_markdown::render::render(&text, width)).await;
    markdown_lines_to_lua(&lua, &lines)
}

/// Returns the current terminal size. Handy for sizing floating windows
/// or wrapping text to fit the screen.
///
/// @return (table) `{cols, rows}`, terminal width and height in characters.
/// @example
/// local size = maki.ui.terminal_size()
/// local half_width = math.floor(size.cols / 2)
#[lua_fn]
fn terminal_size(lua: &Lua) -> LuaResult<Table> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let tbl = lua.create_table()?;
    tbl.set("cols", cols)?;
    tbl.set("rows", rows)?;
    Ok(tbl)
}

/// The `maki.text` helpers kept on `maki.ui` under the same names.
pub const TEXT_ALIASES: [&str; 3] = ["humantime", "display_width", "truncate_text"];

const fn text_alias(doc: FnDoc, desc: &'static str) -> FnDoc {
    FnDoc {
        desc,
        example: "",
        ..doc
    }
}

#[allow(non_upper_case_globals)]
const humantime__doc: FnDoc = text_alias(
    text::humantime__doc,
    "The same function as `maki.text.humantime`, which new code should use.",
);
#[allow(non_upper_case_globals)]
const display_width__doc: FnDoc = text_alias(
    text::display_width__doc,
    "The same function as `maki.text.display_width`, which new code should use.",
);
#[allow(non_upper_case_globals)]
const truncate_text__doc: FnDoc = text_alias(
    text::truncate_text__doc,
    "The same function as `maki.text.truncate_text`, which new code should use.",
);

/// Puts {text}'s [`TEXT_ALIASES`] on {ui}, so both names reach one function.
pub fn add_text_aliases(ui: &Table, text: &Table) -> LuaResult<()> {
    for name in TEXT_ALIASES {
        ui.set(name, text.get::<Function>(name)?)?;
    }
    Ok(())
}

lua_table! {
    /// Functions for building interactive UI. Create buffers to hold
    /// content, open floating or split windows to display them, highlight
    /// code, render markdown, and show status hints.
    ///
    /// ```lua
    /// local buf = maki.ui.buf()
    /// buf:line("hello from my plugin!")
    /// local win = maki.ui.open_win(buf, { title = "Greeting", width = "50%", height = 5 })
    /// ```
    ///
    /// Without a UI (`maki -p`, the sdk, ACP), buffers and the text helpers
    /// still work. The calls that need a screen behave like this:
    ///
    /// - `input`, `input_edit`, `input_decorate`, `input_submit`, and `action`
    ///   on a built-in name return `nil, "no interactive UI attached"`.
    ///   Plugin actions still run.
    /// - `open_editor` returns -1.
    /// - `flash` writes to the log.
    /// - `open_win`, `set_status_hint`, `set_window_title`, `set_mode` and `input_rules` have no effect.
    ///   `input_rules` still throws on a bad rule, so a plugin's tests catch it.
    ///
    /// ### Chat input
    ///
    /// Plugins can read, style, change and send the chat input:
    ///
    /// | Job | Call |
    /// | --- | --- |
    /// | Read the text, the cursor and the selection | [`input`](#maki-ui-input) |
    /// | Style text by what it says, like a `$skill:go` chip | [`input_rules`](#maki-ui-input_rules) |
    /// | Style text from plugin state, like ghost text or diagnostics | [`input_decorate`](#maki-ui-input_decorate) |
    /// | Change the text, the cursor or the selection | [`input_edit`](#maki-ui-input_edit) |
    /// | Send the input, as Enter does | [`input_submit`](#maki-ui-input_submit) |
    /// | React when the input changes | the `InputChanged` [event](#maki-api-create_autocmd) |
    /// | Switch to normal mode for a vim mode | [`set_mode`](#maki-ui-set_mode), see [Modes](/docs/lua-api/#modes) |
    ///
    /// A key that types `TODO: ` at the cursor:
    ///
    /// ```lua
    /// maki.keymap.set("i", "<M-t>", function(ev)
    ///   local st = ev.input
    ///   maki.ui.input_edit({
    ///     start = st.cursor, stop = st.cursor, text = "TODO: ",
    ///     version = st.version, session_id = st.session_id,
    ///   })
    /// end)
    /// ```
    ///
    /// When the look follows from the text, prefer `input_rules`. A rule runs
    /// no Lua per keystroke, and its marks are right again after undo, redo,
    /// a history recall or a chat switch.
    ///
    /// The calls other than `input_rules` wait for the UI to answer. They work
    /// from any callback, like a key handler or an autocmd. While your plugin
    /// loads, they return `nil` and an error.
    ///
    /// Moving the cursor or the selection needs the `input`
    /// [permission](#plugin-permissions), because your next key lands at the
    /// cursor. Changing the text needs `agent` and `session_write` as well,
    /// what sending a message needs, because your next Enter sends it. This
    /// covers `input_edit` and the built-in [actions](#maki-ui-action) that
    /// edit, like `undo` or a history recall. `input_edit` needs
    /// `session_read` too. Without them they raise `permission denied`.
    ///
    /// Reading the input needs `session_read`. This covers `maki.ui.input`,
    /// `ev.input` and `ev.text` in a key handler, and a binding on a key that
    /// types text, see
    /// [`maki.keymap.set`](#maki-keymap-set). Styling the input needs no
    /// permission.
    ///
    /// A plugin that may change the text may write any text. Before Enter
    /// runs or sends it, it checks each plugin that wrote it:
    ///
    /// | Text starts with | Each plugin that wrote it needs |
    /// | --- | --- |
    /// | `!` | `run` |
    /// | a `/` command | what that command needs from a plugin, see [`maki.api.run_command`](#maki-api-run_command). Only the user can run some commands, like `/yolo` |
    /// | anything else | `agent` and `session_write`, like [`input_submit`](#maki-ui-input_submit) |
    ///
    /// If one of them falls short, Enter leaves the text where it is and the
    /// status bar names that plugin. The text becomes the user's again once
    /// it is sent or cleared, once another text replaces it, like a history
    /// recall, or once the user replaces all of it, like selecting everything
    /// and typing.
    ///
    /// A draft a plugin wrote, restored after a restart, counts as written by
    /// an unnamed plugin that holds `agent` and `session_write` at most,
    /// which the status bar calls `a plugin`. So does a rewound or forked
    /// turn that starts with `/` or `!`, or is `exit`.
    ///
    /// Upgrading: `input_edit` needs `input` and `session_read` now, plus
    /// `agent` and `session_write` to change the text, and the editing
    /// built-in actions need `input`, `agent` and `session_write`. A binding
    /// on a key that types text, like `a` or `<Paste>`, needs `session_read`
    /// in any mode. A `plugin.toml` you wrote yourself grants them unless it
    /// sets them to `false`. A package needs its author to add them, and you to approve
    /// the package again.
    ///
    /// ### Offsets and versions
    ///
    /// Offsets count bytes from 0, and a range leaves out `stop`. So the
    /// range `start, stop` is `text:sub(start + 1, stop)`, and
    /// `text:sub(1, cursor)` is everything before the cursor. A newline is
    /// one byte.
    ///
    /// A tab is one open session, named by its `session_id`. Its chats, the
    /// main agent's and each subagent's, share one chat input, and each chat
    /// keeps its own draft.
    ///
    /// The calls work on the input of the focused tab. `input`, the
    /// `InputChanged` event and `ev.input` of a key handler report its
    /// `version` and `session_id`. Pass both to `input_edit`,
    /// `input_decorate` and `input_submit`. Each tab counts versions from
    /// zero, so a version alone cannot tell two tabs apart.
    ///
    /// The user may type between your read and your call. `input_edit` and
    /// `input_decorate` then move your offsets past what was typed, and
    /// `input_submit` fails. All three fail when:
    ///
    /// - `session_id` names a tab that is not focused.
    /// - The text was replaced since: a history recall, a chat switch that
    ///   swapped in another draft, a change made in `$EDITOR`, or a send.
    /// - So much changed since `version` that the offsets can no longer be
    ///   moved.
    ///
    /// `input_edit` and `input_submit` also fail while the chat input is
    /// hidden, since the user could not read what gets sent. It is hidden
    /// while a permission prompt, the plan form or a `below` split takes its
    /// place, or while a picker, a modal or a focused plugin window covers
    /// it. `input_decorate` still works then.
    ///
    /// A failed call returns `nil` and an error message. Read the input again
    /// and retry. Bad arguments, like a missing or an unknown field, throw.
    ///
    /// ### Input marks
    ///
    /// A mark styles a byte range of the input. `input_rules` puts one on
    /// every match, and `input_decorate` and `input_edit` push marks at the
    /// offsets you give. Marks and rules live in namespaces, named by `ns`.
    /// Each plugin has its own namespaces, so two plugins can use the same
    /// name. A mark has these fields:
    ///
    /// | Field | Type | Effect |
    /// | --- | --- | --- |
    /// | `start`, `stop` | integer | The range, for pushed marks only. Without `stop` the mark is a point, which only `virt_text` shows. |
    /// | `hl` | string or table | A theme style name like `"accent"`, or `{ fg, bg, bold, italic, underline, dim, strikethrough, reversed }`. |
    /// | `conceal` | string | Text drawn in place of the range. `""` hides it. |
    /// | `reveal` | string | `"cursor"` shows the raw text while the cursor is inside the range or on its edge. |
    /// | `atomic` | boolean | The cursor steps over the range, a selection takes it whole, and a delete that cuts into it removes all of it. |
    /// | `virt_text` | table | Spans drawn but never part of the text, `{ { text, hl? }, ... }`. |
    /// | `virt_text_pos` | string | `"eol"` (default) draws `virt_text` after the line, `"inline"` before `start`. |
    /// | `lang` | string | Highlights the range as code in this language. |
    ///
    /// The text of `conceal` and `virt_text` must not hold control
    /// characters, newlines included. A mark never edits the text, so a send
    /// carries the raw text, concealed ranges included.
    ///
    /// Where marks overlap:
    ///
    /// - A pushed mark draws over every rule, and a mark pushed later over
    ///   one pushed earlier.
    /// - A rule whose `ns` was first set later draws over one set earlier.
    ///   Replacing a rule keeps its place.
    /// - Within a tree-sitter rule, a later pattern draws over an earlier
    ///   one, and `markdown_inline` over `markdown`.
    /// - Within one pattern, a capture whose name first appears later in the
    ///   query draws over one that appears earlier. In
    ///   `(inline_link (link_text) @text) @link`, `@link` draws over `@text`.
    ///
    /// Pushed marks move with the text: a range keeps covering the same
    /// words, and a point stays after text typed at it. A range is dropped
    /// once its text is gone, or when an edit lands inside it while it
    /// conceals or is atomic. Undo brings back the text without its marks, so
    /// undoing the delete of a chip shows its raw text. Marks stay with their
    /// draft across chat switches, go away when the plugin unloads, and are
    /// not saved.
    extend "maki.ui" => pub fn add_bridge_fns(), DOCS [
        buf, theme_color, theme_style, highlight, markdown, manual humantime, terminal_size,
        manual display_width, manual truncate_text,
    ]
}

/// Palette colors stay symbolic (`"4"`, `"12"`, `"default"`) so the terminal
/// picks the actual shade; only true RGB is written as hex. Every spelling
/// here round-trips back through a span's `fg`/`bg`.
pub(crate) fn segment_color_to_lua(c: SegmentColor) -> String {
    match c {
        SegmentColor::Rgb((r, g, b)) => format!("#{r:02x}{g:02x}{b:02x}"),
        SegmentColor::Ansi(i) => i.to_string(),
        SegmentColor::Default => DEFAULT_COLOR_NAME.to_owned(),
    }
}

fn segments_to_lua_lines(
    lua: &Lua,
    lines: &[Vec<maki_highlight::StyledSegment>],
) -> LuaResult<Table> {
    let result = lua.create_table_with_capacity(lines.len(), 0)?;
    for (i, segs) in lines.iter().enumerate() {
        let line_tbl = lua.create_table_with_capacity(segs.len(), 0)?;
        for (j, seg) in segs.iter().enumerate() {
            let span = lua.create_table_with_capacity(2, 0)?;
            span.raw_set(1, seg.text.as_str())?;
            let style = lua.create_table_with_capacity(0, 4)?;
            style.raw_set("fg", segment_color_to_lua(seg.fg))?;
            if seg.bold {
                style.raw_set("bold", true)?;
            }
            if seg.italic {
                style.raw_set("italic", true)?;
            }
            if seg.underline {
                style.raw_set("underline", true)?;
            }
            span.raw_set(2, style)?;
            line_tbl.raw_set(i32::try_from(j + 1).unwrap(), span)?;
        }
        result.raw_set(i32::try_from(i + 1).unwrap(), line_tbl)?;
    }
    Ok(result)
}

/// Most spans become a named style string. `Highlight` tokens carry their
/// own rgb, so they become an inline `{fg, bold, italic, underline}` table.
///
/// No wildcard arm on `StyleToken`: adding a variant is a compile error
/// here, so we can't forget to map it.
fn span_style_to_lua(lua: &Lua, span: &maki_markdown::render::Span) -> LuaResult<mlua::Value> {
    use maki_markdown::render::StyleToken;

    let v = match &span.style {
        StyleToken::Text => {
            let name = emphasis_style_name(span.emphasis);
            mlua::Value::String(lua.create_string(name)?)
        }
        StyleToken::InlineCode => mlua::Value::String(lua.create_string("inline_code")?),
        StyleToken::Highlight {
            fg,
            bold,
            italic,
            underline,
        } => {
            let tbl = lua.create_table()?;
            tbl.set("fg", segment_color_to_lua(*fg))?;
            if *bold {
                tbl.set("bold", true)?;
            }
            if *italic {
                tbl.set("italic", true)?;
            }
            if *underline {
                tbl.set("underline", true)?;
            }
            mlua::Value::Table(tbl)
        }
        StyleToken::CodeBar => mlua::Value::String(lua.create_string("code_gutter")?),
        StyleToken::Heading => mlua::Value::String(lua.create_string("heading")?),
        StyleToken::ListMarker => mlua::Value::String(lua.create_string("list_marker")?),
        StyleToken::TableBorder => mlua::Value::String(lua.create_string("table_border")?),
        StyleToken::HorizontalRule => mlua::Value::String(lua.create_string("horizontal_rule")?),
    };
    Ok(v)
}

/// Flatten `Emphasis` to a single named style. Strike wins over bold/italic
/// (the Lua theme has no combined slot). Underline only appears in
/// `Highlight` tokens, not here.
fn emphasis_style_name(e: maki_markdown::Emphasis) -> &'static str {
    if e.strike {
        "strikethrough"
    } else if e.bold && e.italic {
        "bold_italic"
    } else if e.bold {
        "bold"
    } else if e.italic {
        "italic"
    } else {
        ""
    }
}

fn markdown_lines_to_lua(lua: &Lua, lines: &[maki_markdown::render::Line]) -> LuaResult<Table> {
    let result = lua.create_table_with_capacity(lines.len(), 0)?;
    for (i, rendered) in lines.iter().enumerate() {
        let line_tbl = lua.create_table_with_capacity(rendered.spans.len(), 0)?;
        for (j, sp) in rendered.spans.iter().enumerate() {
            let span_tbl = lua.create_table_with_capacity(2, 0)?;
            span_tbl.raw_set(1, sp.text.as_str())?;
            span_tbl.raw_set(2, span_style_to_lua(lua, sp)?)?;
            line_tbl.raw_set(i32::try_from(j + 1).unwrap(), span_tbl)?;
        }
        result.raw_set(i32::try_from(i + 1).unwrap(), line_tbl)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{
        emphasis_style_name, markdown_lines_to_lua, segments_to_lua_lines, span_style_to_lua,
    };
    use maki_highlight::{SegmentColor, StyledSegment};
    use maki_markdown::Emphasis;
    use maki_markdown::render::{self, Span, StyleToken};
    use mlua::{Lua, LuaSerdeExt, Table, Value as LuaValue};
    use serde_json::{Value, json};
    use test_case::test_case;

    const ORANGE_HEX: &str = "#ff8000";
    const MD_WIDTH: u16 = 80;
    const STYLE_CODE_BAR: &str = "code_gutter";

    #[test]
    fn highlighted_lines_keep_text_order_color_and_optional_flags() {
        let lua = Lua::new();
        let segment = |text: &str, enabled| StyledSegment {
            text: text.into(),
            fg: SegmentColor::Rgb((255, 128, 0)),
            bold: enabled,
            italic: enabled,
            underline: enabled,
        };
        let lines = vec![
            vec![segment("fn ", true), segment("main", false)],
            vec![segment("héllo 🦀 ✨", false)],
        ];
        let result = segments_to_lua_lines(&lua, &lines).unwrap();
        assert_eq!(
            lua.from_value::<Value>(LuaValue::Table(result)).unwrap(),
            json!([
                [["fn ", {"fg": ORANGE_HEX, "bold": true, "italic": true, "underline": true}], ["main", {"fg": ORANGE_HEX}]],
                [["héllo 🦀 ✨", {"fg": ORANGE_HEX}]],
            ])
        );
    }

    fn render_markdown(lua: &Lua, input: &str) -> Table {
        markdown_lines_to_lua(lua, &render::render(input, MD_WIDTH)).unwrap()
    }

    fn span_style(line: &Table, idx: usize) -> String {
        line.get::<Table>(idx).unwrap().get(2).unwrap()
    }

    #[test_case("hello **world**", &[("hello ", ""), ("world", "bold")] ; "bold")]
    #[test_case("***x***", &[("x", "bold_italic")] ; "bold_italic")]
    #[test_case("*italic*", &[("italic", "italic")] ; "italic")]
    #[test_case("~~gone~~", &[("gone", "strikethrough")] ; "strike")]
    #[test_case("x `y` z", &[("x ", ""), ("y", "inline_code"), (" z", "")] ; "inline_code")]
    #[test_case("**`code`**", &[("code", "inline_code")] ; "code_overrides_emphasis")]
    #[test_case("# hello **world**", &[("hello ", "heading"), ("world", "heading")] ; "heading_overrides_emphasis")]
    #[test_case("# foo `bar`", &[("foo ", "heading"), ("bar", "inline_code")] ; "code_in_heading")]
    #[test_case("- **item**", &[("• ", "list_marker"), ("item", "bold")] ; "unordered_list")]
    #[test_case("1. **item**", &[("1. ", "list_marker"), ("item", "bold")] ; "ordered_list")]
    fn markdown_keeps_span_text_and_style(input: &str, expected: &[(&str, &str)]) {
        let lua = Lua::new();
        let line: Table = render_markdown(&lua, input).get(1).unwrap();
        let spans: Vec<(String, String)> = line
            .sequence_values::<Table>()
            .map(|span| {
                let span = span.unwrap();
                (span.get(1).unwrap(), span.get(2).unwrap())
            })
            .collect();
        assert_eq!(
            spans,
            expected
                .iter()
                .map(|(text, style)| (text.to_string(), style.to_string()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn markdown_unknown_constructs_stay_plain() {
        let lua = Lua::new();
        let line: Table = render_markdown(&lua, "a*b").get(1).unwrap();
        for i in 1..=line.raw_len() {
            assert_eq!(span_style(&line, i), "");
        }
    }

    #[test]
    fn horizontal_rules_fill_the_requested_width() {
        let lua = Lua::new();
        let line: Table = render_markdown(&lua, "---").get(1).unwrap();
        assert_eq!(line.raw_len(), 1);
        assert_eq!(span_style(&line, 1), "horizontal_rule");
        assert_eq!(
            line.get::<Table>(1).unwrap().get::<String>(1).unwrap(),
            "─".repeat(MD_WIDTH.into())
        );
    }

    #[test]
    fn markdown_tables_keep_border_and_content_styles() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "| col1 | col2 |\n|------|------|\n| a | b |");
        let styles: Vec<String> = result
            .sequence_values::<Table>()
            .flat_map(|line| line.unwrap().sequence_values::<Table>().collect::<Vec<_>>())
            .map(|span| span.unwrap().get(2).unwrap())
            .collect();
        assert!(styles.iter().any(|style| style == "table_border"));
        assert!(styles.iter().any(String::is_empty));
    }

    #[test_case(false, false, false, true, "" ; "underline_has_no_named_style")]
    #[test_case(true, true, true, false, "strikethrough" ; "strike_overrides_other_emphasis")]
    fn unsupported_emphasis_combinations_keep_their_fallback(
        bold: bool,
        italic: bool,
        strike: bool,
        underline: bool,
        expected: &str,
    ) {
        assert_eq!(
            emphasis_style_name(Emphasis {
                bold,
                italic,
                strike,
                underline
            }),
            expected
        );
    }
    #[test]
    fn markdown_code_fence_emits_code_bar_prefix_with_highlight_span_tables() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "```rust\nfn x() {}\n```");
        let lines = result.len().unwrap();
        let code_line: Table = (1..=lines)
            .find_map(|i| {
                let line: Table = result.get(i).ok()?;
                (line.len().ok()? > 0
                    && line
                        .get::<Table>(1)
                        .and_then(|s| s.get::<String>(2))
                        .ok()
                        .is_some_and(|s| s == STYLE_CODE_BAR))
                .then_some(line)
            })
            .expect("code bar line");
        assert_eq!(span_style(&code_line, 1), STYLE_CODE_BAR);
        let content_span: Table = code_line.get(2).unwrap();
        let style = content_span.get::<mlua::Value>(2).unwrap();
        assert!(
            matches!(style, LuaValue::Table(_)),
            "highlight span style must be an inline table"
        );
    }

    #[test]
    fn span_style_to_lua_highlight() {
        let lua = Lua::new();
        for (bold, italic, underline) in [(true, false, false), (true, true, true)] {
            let span = Span {
                text: "tok".into(),
                style: StyleToken::Highlight {
                    fg: SegmentColor::Rgb((255, 128, 0)),
                    bold,
                    italic,
                    underline,
                },
                emphasis: Emphasis::default(),
            };
            let val = span_style_to_lua(&lua, &span).unwrap();
            let tbl = match val {
                LuaValue::Table(t) => t,
                other => panic!("expected table, got {other:?}"),
            };
            assert_eq!(tbl.get::<String>("fg").unwrap(), ORANGE_HEX);
            assert_eq!(tbl.get::<bool>("bold").unwrap(), bold);
            assert_eq!(
                tbl.get::<Option<bool>>("italic").unwrap().unwrap_or(false),
                italic
            );
            assert_eq!(
                tbl.get::<Option<bool>>("underline")
                    .unwrap()
                    .unwrap_or(false),
                underline
            );
        }
    }
}
