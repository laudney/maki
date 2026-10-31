use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use maki_lua_macro::lua_fn;
use maki_storage::id::MakiId;
use maki_types::snapshot::UiWaker;
use mlua::{Lua, Result as LuaResult, Table};

use crate::api::action::run_plugin_action;
use crate::api::keymap::{InputMode, accept_key, switch_mode};
use crate::api::util::command::{
    Anchor, Border, BuiltinAction, Dimension, FloatConfig, HintEntries, HintEntry, HintWriter,
    InputDecorate, InputEdit, InputMarks, InputRequest, InputSubmit, Split, TabRef, TextEdit,
    TitlePos, UiAction, WinCommand, WinEvent, ui_json_roundtrip, ui_roundtrip, ui_send,
};
use crate::api::util::convert::{TableExt, reject_unknown_fields, required};
use crate::api::util::pair::{Pair, err_pair, try_pair};
use crate::docs::{DocKind, FnDoc, ModuleDoc, ParamDoc};
use crate::key::Key;
use crate::plugin_permissions::{PluginReach, SENDS_PROMPT, require};
pub(crate) mod marks;
pub(crate) mod renderer;
pub(crate) mod syntax;
pub(crate) mod view;
pub(crate) mod win;

pub(crate) use crate::buffer::buf;

use crate::runtime::{Lineage, plugin_loaded};
use marks::{InputRule, InputRuleStore, PatternRule, parse_mark, parse_pushed_mark};
use renderer::{
    has_renderer__doc, has_renderer__register, register_renderer__doc, register_renderer__register,
    renderer__doc, renderer__register,
};
use syntax::parse_syntax;
use win::{WinHandle, WinSender};

const INPUT_EDIT: &str = "input_edit";
const INPUT_DECORATE: &str = "input_decorate";
const INPUT_SUBMIT: &str = "input_submit";
const SET_STATUS_HINT: &str = "set_status_hint";
const SESSION_OPT: &str = "session";
const SINGLE_EDIT_KEYS: [&str; 3] = ["start", "stop", "text"];
const NOT_A_LIST_ERR: &str = "is a list of tables, so wrap a single one in {}";
const MIXED_EDIT_SHAPES_ERR: &str =
    "input_edit: pass either 'edits' or 'start'/'stop'/'text', not both";
const MARKS_HALVES_ERR: &str = "input_edit: 'marks' and 'ns' go together";
/// A handler can outlive its load, and the unload that clears its marks has
/// already reached the UI, so marks it pushed then would stay for good.
const UNLOADED_MARKS_ERR: &str = "the plugin is unloaded, so it can push no marks";
const EDIT_OPTIONS: &[&str] = &[
    "edits",
    "start",
    "stop",
    "text",
    "version",
    "session_id",
    "cursor",
    "anchor",
    "strict",
    "marks",
    "ns",
];
const DECORATE_OPTIONS: &[&str] = &["session_id", "version", "ns", "marks"];
/// A `text` field would read as "send this", but what goes out is always the
/// text on screen, so it is refused.
const SUBMIT_OPTIONS: &[&str] = &["session_id", "version"];
const INPUT_RULES: &str = "input_rules";
const RULE_OPTIONS: &[&str] = &["ns", "pattern", "mark", "treesitter"];
const RULE_HALVES_ERR: &str = "input_rules: 'pattern' and 'mark' go together";
const SYNTAX_ALONE_ERR: &str =
    "input_rules: 'treesitter' takes the place of 'pattern' and 'mark', not both";

/// A focused window is handed every key the host does not answer itself, so a
/// list of keys to take on top of that claims nothing it does not already
/// have. Refused rather than ignored: a plugin writing one has the wrong model
/// of who is reading the keyboard, and that is worth an error it can read.
const FOCUSED_CLAIM_ERR: &str =
    "a focused window already receives every key, so `keys` belongs to `focus = false` windows";

/// Each plugin gets one slot that shows in every tab, and one more for each
/// session it names. One plugin serves all the tabs, so without the session
/// in the key a hint about one chat would show up in all of them.
pub(crate) struct HintStore {
    hints: BTreeMap<HintSlot, Vec<(String, String)>>,
}

/// A `None` session shows in every tab.
type HintSlot = (Arc<str>, Option<TabRef>);

impl HintStore {
    pub fn new() -> Self {
        Self {
            hints: BTreeMap::new(),
        }
    }

    /// Empty {spans} clear. Without a session they wipe every slot of the
    /// plugin, so a plain `set_status_hint(nil)` still hides all it shows,
    /// like it did back when a plugin had only one hint.
    pub fn set(&mut self, plugin: Arc<str>, session: Option<TabRef>, spans: Vec<(String, String)>) {
        match (spans.is_empty(), session) {
            (false, session) => {
                self.hints.insert((plugin, session), spans);
            }
            (true, Some(session)) => {
                self.hints.remove(&(plugin, Some(session)));
            }
            (true, None) => self.clear_plugin(&plugin),
        }
    }

    pub fn clear_plugin(&mut self, plugin: &str) {
        self.hints.retain(|(owner, _), _| owner.as_ref() != plugin);
    }

    pub fn clear_session(&mut self, session: MakiId) {
        self.hints
            .retain(|(_, owner), _| *owner != Some(TabRef::from(session)));
    }

    pub fn snapshot_entries(&self) -> HintEntries {
        self.hints
            .iter()
            .map(|((plugin, session), spans)| HintEntry {
                plugin: Arc::clone(plugin),
                session: session.clone(),
                spans: spans.clone(),
            })
            .collect()
    }
}

/// The windows each plugin has open, so unloading it can take them down.
///
/// A window is the one thing a plugin puts on screen that the host cannot
/// otherwise revoke. The handle lives in the plugin's module table, and a
/// module nobody references any more goes only when the Lua collector next
/// runs. Until then the float is on screen with its key loop cancelled along
/// with the load, and every key it claimed is swallowed on the way to a
/// channel nobody reads: `/reload` would leave the user's `<CR>` doing
/// nothing until a collection they cannot ask for.
#[derive(Default)]
pub(crate) struct WinStore {
    open: BTreeMap<Arc<str>, Vec<WinSender>>,
}

impl WinStore {
    /// Drops the windows that have already gone on the way in, so a plugin
    /// opening and closing one per keystroke does not grow this for the run.
    fn track(&mut self, plugin: Arc<str>, cmd_tx: WinSender) {
        let windows = self.open.entry(plugin).or_default();
        windows.retain(|tx| !tx.is_disconnected());
        windows.push(cmd_tx);
    }

    /// The same command the plugin's own `win:close()` sends, so the window
    /// leaves by the one path every other window leaves by and the plugin's
    /// loop still hears the close it is waiting on.
    pub fn close_plugin(&mut self, plugin: &str) {
        for tx in self.open.remove(plugin).unwrap_or_default() {
            tx.send(WinCommand::Close);
        }
    }
}

/// Hints leave with their session. A tab that moves on to a new chat would
/// inherit them otherwise, and a closed tab would leave them here for good.
pub(crate) fn clear_session_hints(lua: &Lua, session: MakiId) {
    if let Some(mut store) = lua.app_data_mut::<HintStore>() {
        store.clear_session(session);
    }
    publish_hint_snapshot(lua);
}

fn session_opt(opts: &Table) -> LuaResult<Option<TabRef>> {
    Ok(opts
        .opt::<String>(SESSION_OPT)?
        .map(|id| TabRef(Arc::from(id))))
}

fn publish_hint_snapshot(lua: &Lua) {
    if let Some(store) = lua.app_data_ref::<HintStore>() {
        let entries = store.snapshot_entries();
        if let Some(writer) = lua.app_data_ref::<HintWriter>() {
            writer.publish(entries);
        }
    }
}

pub(crate) fn parse_footer(tbl: &Table) -> LuaResult<Vec<(String, String)>> {
    let footer_tbl: Table = match tbl.get("footer") {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };
    footer_tbl
        .sequence_values::<Table>()
        .map(|entry| {
            let entry = entry?;
            Ok((entry.get(1)?, entry.get(2)?))
        })
        .collect()
}

/// Shows a short-lived message in the status bar, such as "copied!" or a
/// transient warning. Without a UI, the message goes to the log.
///
/// @param msg string Message text.
/// @return
/// @example
/// maki.ui.flash("Copied to clipboard!")
#[lua_fn]
fn flash(
    _lua: &Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    #[ctx] plugin: Arc<str>,
    msg: String,
) -> LuaResult<()> {
    if ui_send(tx.as_ref(), UiAction::Flash(msg.clone())).is_err() {
        tracing::info!(plugin = %plugin, "{msg}");
    }
    Ok(())
}

/// Sets the terminal emulator's window title. Pass an empty string to
/// clear it.
///
/// The title passes through tmux, GNU screen, and zellij untouched, and
/// control characters are stripped, so model text cannot inject escape
/// sequences into the terminal. On exit maki hands the title back to the
/// shell, on terminals that support the title stack.
///
/// @param title string New window title, e.g. `"● 3/5 tests"`.
/// @return
/// @example
/// maki.ui.set_window_title("maki: " .. session_name)
/// -- Give the title back to the shell:
/// maki.ui.set_window_title("")
#[lua_fn]
fn set_window_title(
    _lua: &Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    title: String,
) -> LuaResult<()> {
    let _ = ui_send(tx.as_ref(), UiAction::SetWindowTitle(title));
    Ok(())
}

/// Switches the chat input to normal mode (`"n"`) or insert mode (`"i"`),
/// like leaving and entering insert mode in Neovim.
///
/// The mode is global and stays until a plugin switches it again, apart from
/// the times [Modes](/docs/lua-api/#modes) lists. It applies at once:
/// `maki.fn.mode()` returns it and the next key is matched against it. If
/// your plugin unloads while the input is in normal mode, maki switches it
/// back to insert mode.
///
/// Throws on any mode but `"n"` and `"i"`.
///
/// @param mode string `"n"` or `"i"`.
/// @return
/// @example
/// maki.keymap.set("n", "i", function() maki.ui.set_mode("i") end)
#[lua_fn]
fn set_mode(lua: &Lua, #[ctx] plugin: Arc<str>, mode: String) -> LuaResult<()> {
    switch_mode(lua, InputMode::parse(&mode)?, &plugin);
    Ok(())
}

/// Runs a built-in or plugin action by name, and returns whether it handled
/// the request.
///
/// A built-in does what its default key does, so you can move it to another
/// key:
///
/// | Built-in | What it does | `false` when |
/// | --- | --- | --- |
/// | `"file_picker"` | Opens the file picker | |
/// | `"search"` | Opens message search | |
/// | `"help"` | Opens or closes `/help` | |
/// | `"model_picker"` | Opens the model picker | |
/// | `"plan_toggle"` | Opens or closes the plan form | no plan is ready |
/// | `"plan_editor"` | Opens the plan in your editor | there is no plan |
/// | `"edit_input"` | Opens the chat input in your editor | |
/// | `"pop_queue"` | Removes the first queued message | |
/// | `"prev_chat"`, `"next_chat"` | Shows the previous or next chat, with its own draft | |
/// | `"input_raw"` | Switches the input between drawing what plugins conceal and showing the text under it, which is what gets sent | |
/// | `"undo"`, `"redo"` | Undoes or redoes one input edit | nothing to undo or redo |
/// | `"history_prev"`, `"history_next"` | Recalls the previous or next prompt | at either end of the history |
/// | `"newline"` | Inserts a new line | |
/// | `"background"` | Moves running calls to the background, like `Ctrl+B` | no call is running |
///
/// `"undo"`, `"redo"`, `"history_prev"`, `"history_next"` and `"newline"`
/// edit the chat input, also in normal mode. They need the `input`, `agent`
/// and `session_write` permissions, see [chat input](#chat-input), and throw
/// without them like any gated call. They return `false` in a finished subagent's chat, which has
/// no input. `"pop_queue"` needs `session_write`, because it throws away a
/// queued prompt. `"background"` needs `agent`, because each call it moves
/// wakes the chat with its result. The other built-ins need no permission.
///
/// A built-in runs in the UI after your earlier `maki.ui` calls, and
/// `action` waits for its answer. While your plugin loads, it returns `nil`
/// and an error at once.
///
/// A plugin action has a dot in its name, like `"completion.accept"`. It
/// comes from [`maki.api.register_action`](#maki-api-register_action), which
/// also says what permissions a caller needs. It runs at once, also while
/// your plugin loads, and `action` returns what it returned.
///
/// `action` returns `nil` and an error, which an `if` treats as not handled,
/// when:
///
/// - The action's plugin is not loaded.
/// - The action threw.
/// - Your plugin lacks a permission a plugin action needs. The status bar
///   shows this too.
/// - A built-in runs without a UI.
///
/// It throws on a name that cannot be right: no dot and not a built-in, or
/// one its loaded plugin never registered.
///
/// To try several actions on one key, see
/// [Composing keys](/docs/keybindings/#composing-keys). To send the input,
/// use `maki.ui.input_submit`. For slash commands, see `maki.api.run_command`.
///
/// @param name string Action name, e.g. `"file_picker"` or `"completion.accept"`.
/// @return (boolean|nil, string|nil) Whether the action handled it, or nil and an error.
/// @example
/// -- A vim plugin's undo in normal mode, which needs `input`, `agent` and `session_write`:
/// maki.keymap.set("n", "u", function() maki.ui.action("undo") end)
///
/// -- A key for a built-in that has none:
/// maki.keymap.set({ "n", "i" }, "<M-r>", function() maki.ui.action("input_raw") end)
#[lua_fn]
async fn action(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    #[ctx] plugin: Arc<str>,
    #[ctx] caller: PluginReach,
    name: String,
) -> LuaResult<Pair<bool>> {
    let Ok(builtin) = name.parse::<BuiltinAction>() else {
        return run_plugin_action(&lua, tx.as_ref(), &name, &plugin, &caller).await;
    };
    for &perm in builtin.needs() {
        require(&lua, caller.permissions(), perm)?;
    }
    let reply = try_pair!(
        ui_roundtrip(tx.as_ref(), |reply_tx| UiAction::Builtin {
            action: builtin,
            caller: caller.now(&lua),
            reply_tx,
        })
        .await
    );
    Ok((Some(reply), None))
}

async fn input_roundtrip(
    lua: Lua,
    tx: Option<&flume::Sender<UiAction>>,
    req: InputRequest,
) -> LuaResult<Pair<mlua::Value>> {
    ui_json_roundtrip(&lua, tx, |reply_tx| UiAction::Input { req, reply_tx }).await
}

fn input_marks(func: &str, ns: String, marks: Table) -> LuaResult<InputMarks> {
    let marks = table_list(marks, func, "marks")?
        .iter()
        .map(parse_pushed_mark)
        .collect::<LuaResult<_>>()?;
    Ok(InputMarks { ns, marks })
}

/// Reads the chat input of the focused tab.
///
/// The returned table has:
///
/// - `text` (string) the whole value, newlines included.
/// - `cursor` (integer) byte offset of the cursor.
/// - `anchor` (integer|nil) byte offset of the other end of the selection,
///   nil when nothing is selected. The selected bytes run from
///   `math.min(anchor, cursor)` to `math.max(anchor, cursor)`.
/// - `version` (integer) and `session_id` (string), for the calls that
///   write. See [offsets and versions](#offsets-and-versions).
///
/// The table has no line or column. The example works them out. To open a
/// window at the cursor, pass `anchor = "input_caret"` to
/// [`open_win`](#maki-ui-open_win), an option unrelated to the selection
/// `anchor` here.
///
/// @return (table|nil, string|nil) The input state, or nil and an error without a UI.
/// @example
/// local st = maki.ui.input()
/// local before = st.text:sub(1, st.cursor)
/// local line = select(2, before:gsub("\n", "")) -- 0-based
/// local col = #before:match("[^\n]*$") -- in bytes
#[lua_fn(guard = SessionRead)]
async fn input(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
) -> LuaResult<Pair<mlua::Value>> {
    input_roundtrip(lua, tx.as_ref(), InputRequest::Read).await
}

/// Read as a `Vec`, a table keeps only its list part, so a single item
/// passed in place of the list {key} of {func} would quietly read as none.
fn table_list(list: Table, func: &str, key: &str) -> LuaResult<Vec<Table>> {
    if list.pairs::<mlua::Value, mlua::Value>().count() != list.raw_len() {
        return Err(mlua::Error::runtime(format!(
            "{func}: '{key}' {NOT_A_LIST_ERR}"
        )));
    }
    list.sequence_values().collect()
}

/// Replaces byte ranges of the chat input, as if the user had selected each
/// one and typed its `text`. All edits land as one change, with one new
/// version and one undo step.
///
/// Plan every range against the text at `version`, without counting the
/// other edits in the list. Ranges must not overlap, and no two may start
/// at the same offset. With `edits = {}` the call only moves the cursor or
/// the selection, or replaces the marks in `ns`, so it needs `cursor`,
/// `anchor` or `marks`. It makes no new version and no undo step.
///
/// Besides the cases in [offsets and versions](#offsets-and-versions), an
/// edit fails when:
///
/// - The user typed at or next to one of its ranges since `version`. Two
///   inserts at the same offset count, because no order is right for both.
/// - `cursor` or `anchor` lands inside text changed since `version`.
/// - `strict` is set and `version` is not current.
/// - A range ends past the end of the text, or an offset falls inside a
///   multi-byte character.
///
/// Enter checks the text you write before it runs it, see
/// [chat input](#chat-input). For marks alone, use `input_decorate`, which
/// needs no permission.
///
/// `text` is cleaned the way a paste is: tabs become spaces, carriage
/// returns become newlines, and other control characters are dropped. With
/// `cursor`, `anchor` or `marks` given, text that needs cleaning fails
/// instead, since their offsets would no longer match. `ev.text` of
/// `<Paste>` is already clean.
///
/// It also needs `session_read`, like `maki.ui.input`. Its answer and its
/// errors tell where the user typed since `version`, so a plugin could
/// learn the input from them.
///
/// @param opts table Options:
///   `edits` (table|nil) list of `{ start, stop, text }` tables, `{}` for none. Without it, `start`, `stop` and `text` are read from {opts} as one edit.
///   `start` (integer) byte offset where the range starts.
///   `stop` (integer) byte offset where it ends. Equal to `start` to insert.
///   `text` (string) the new text, `""` to delete the range. Required.
///   `version` (integer) the version the offsets were planned against.
///   `session_id` (string) the tab the offsets were read from.
///   `cursor` (integer|nil) byte offset to leave the cursor at, counted in the text the edits produce, as if nobody else had typed. Default is the end of the new text of the edit with the highest `start`, or the cursor as it is when `edits` is empty.
///   `anchor` (integer|nil) byte offset to select from, up to `cursor`, counted like `cursor`. Without it nothing stays selected.
///   `strict` (boolean|nil) accept only the current version, instead of moving the edits past newer typing. Default false.
///   `marks` (table|nil) [marks](#input-marks) to replace namespace `ns` with, counted like `cursor`. They appear together with the text. A mark that does not fit fails the whole edit.
///   `ns` (string|nil) the namespace for `marks`. Required with `marks`.
/// @return (table|nil, string|nil) `{ version, cursor }` after the edit, or nil and an error. Pass this `version` to a call that follows.
/// @example
/// -- Insert each paste under a chip, in a namespace of its own.
/// local pastes = 0
/// maki.keymap.set("i", "<Paste>", function(ev)
///   local st = ev.input
///   pastes = pastes + 1
///   local done = maki.ui.input_edit({
///     start = st.cursor, stop = st.cursor, text = ev.text,
///     ns = "paste" .. pastes,
///     marks = { { start = st.cursor, stop = st.cursor + #ev.text, conceal = "[pasted text]", atomic = true } },
///     version = st.version, session_id = st.session_id,
///   })
///   if not done then ev.fallback() end
/// end)
///
/// -- Wrap the selection in bold and keep the same words selected:
/// local st = maki.ui.input()
/// if not st.anchor then return end
/// local s, e = math.min(st.anchor, st.cursor), math.max(st.anchor, st.cursor)
/// maki.ui.input_edit({
///   edits = {
///     { start = s, stop = s, text = "**" },
///     { start = e, stop = e, text = "**" },
///   },
///   cursor = e + 2,
///   anchor = s + 2,
///   version = st.version,
///   session_id = st.session_id,
/// })
#[lua_fn(guard = [Input, SessionRead])]
async fn input_edit(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    #[ctx] plugin: Arc<str>,
    #[ctx] caller: PluginReach,
    opts: Table,
) -> LuaResult<Pair<mlua::Value>> {
    reject_unknown_fields(&opts, EDIT_OPTIONS, INPUT_EDIT)?;
    let edits = match opts
        .opt::<Table>("edits")?
        .map(|edits| table_list(edits, INPUT_EDIT, "edits"))
        .transpose()?
    {
        None => vec![text_edit(&opts)?],
        Some(_)
            if SINGLE_EDIT_KEYS
                .iter()
                .any(|key| opts.contains_key(*key).unwrap_or(false)) =>
        {
            return Err(mlua::Error::runtime(MIXED_EDIT_SHAPES_ERR));
        }
        Some(edits) => edits
            .iter()
            .map(|edit| {
                reject_unknown_fields(edit, &SINGLE_EDIT_KEYS, INPUT_EDIT)?;
                text_edit(edit)
            })
            .collect::<LuaResult<_>>()?,
    };
    if edits.iter().any(TextEdit::changes_text) {
        for &perm in SENDS_PROMPT {
            require(&lua, caller.permissions(), perm)?;
        }
    }
    let marks = match (opts.opt("ns")?, opts.opt("marks")?) {
        (None, None) => None,
        (Some(ns), Some(marks)) => Some(input_marks(INPUT_EDIT, ns, marks)?),
        _ => return Err(mlua::Error::runtime(MARKS_HALVES_ERR)),
    };
    if marks.is_some() && !plugin_loaded(&lua, &plugin) {
        return Ok(err_pair(UNLOADED_MARKS_ERR));
    }
    let req = InputRequest::Edit(InputEdit {
        edits,
        cursor: opts.opt("cursor")?,
        anchor: opts.opt("anchor")?,
        version: required(&opts, INPUT_EDIT, "version")?,
        strict: opts.opt_bool("strict").unwrap_or_default(),
        session_id: required(&opts, INPUT_EDIT, "session_id")?,
        caller: caller.now(&lua),
        marks,
    });
    input_roundtrip(lua, tx.as_ref(), req).await
}

/// Replaces all the marks your plugin has in namespace `ns` of the chat
/// input. Use it for looks that come from plugin state. When the look follows
/// from the text, a [rule](#maki-ui-input_rules) fits better.
///
/// Keep a look that tracks something live, like ghost text at the cursor, in
/// one namespace and replace it on `InputChanged`. Marks placed once, like
/// the chip over a paste, each need a namespace of their own.
///
/// A mark that the typing since `version` would have
/// [dropped](#input-marks) is skipped, so typing alone never fails the call.
///
/// Besides the cases in [offsets and versions](#offsets-and-versions), the
/// call fails when a mark ends past the end of the text, an offset falls
/// inside a multi-byte character, or `start` is past `stop`.
///
/// @param opts table Options:
///   `ns` (string) the namespace to replace.
///   `marks` (table) list of [marks](#input-marks), `{}` to clear the namespace.
///   `version` (integer) the version the offsets were planned against.
///   `session_id` (string) the tab the offsets were read from.
/// @return (table|nil, string|nil) `{ version }`, the version the marks were placed on, or nil and an error.
/// @example
/// maki.api.create_autocmd("InputChanged", {
///   callback = function(ev)
///     maki.ui.input_decorate({
///       ns = "ghost",
///       marks = { { start = ev.data.cursor, virt_text = { { "complete this", "dim" } }, virt_text_pos = "inline" } },
///       version = ev.data.version,
///       session_id = ev.data.session_id,
///     })
///   end,
/// })
#[lua_fn]
async fn input_decorate(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    #[ctx] plugin: Arc<str>,
    opts: Table,
) -> LuaResult<Pair<mlua::Value>> {
    reject_unknown_fields(&opts, DECORATE_OPTIONS, INPUT_DECORATE)?;
    let decorate = InputDecorate {
        version: required(&opts, INPUT_DECORATE, "version")?,
        session_id: required(&opts, INPUT_DECORATE, "session_id")?,
        marks: input_marks(
            INPUT_DECORATE,
            required(&opts, INPUT_DECORATE, "ns")?,
            required(&opts, INPUT_DECORATE, "marks")?,
        )?,
        plugin,
    };
    if !plugin_loaded(&lua, &decorate.plugin) {
        return Ok(err_pair(UNLOADED_MARKS_ERR));
    }
    input_roundtrip(lua, tx.as_ref(), InputRequest::Decorate(decorate)).await
}

/// Sends the chat input the way Enter does, `/` commands, `!` lines, images,
/// the queue and the history included. Only a `\` before the cursor differs:
/// Enter turns it into a new line, and `input_submit` sends it as it is.
///
/// Use it to send what a key handler wrote, since `ev.fallback()` on `<CR>`
/// refuses text that changed after the press, see
/// [`maki.keymap.set`](/docs/lua-api/#maki-keymap-set).
///
/// It sends only while the input still holds `version`, so what goes out is
/// the text your plugin planned against. After your own edit, pass the
/// version `input_edit` returned. In a key handler, `ev.input.version` is
/// the version at the press. Any other change makes the version stale, like
/// an edit of another plugin or a key that landed after the one second keys
/// wait for a handler.
///
/// A send needs the [plugin permissions](/docs/lua-api/#plugin-permissions)
/// that doing the same thing directly needs:
///
/// - A message needs `agent` and `session_write`, like
///   `maki.session.prompt`, also when it waits in the queue or is the retry
///   Enter sends after a login.
/// - A `!` or `!!` line needs `run`.
/// - A `/` command needs what `maki.api.run_command` needs for it, so
///   `/yolo`, `/trust`, `/packupdate` and `/packdel` always fail.
///
/// Besides the cases in [offsets and versions](#offsets-and-versions), the
/// call fails, and the input stays as it is, when:
///
/// - The message queue has the keyboard after `/queue`.
/// - There is nothing to send, or the command palette is open with no
///   command to run.
/// - Your plugin lacks a permission listed above.
/// - A plugin changed the text, and Enter would refuse it for that plugin,
///   see [chat input](#chat-input).
///
/// @param opts table Options:
///   `version` (integer) the version of the text to send, from `maki.ui.input`, `ev.input` or `input_edit`.
///   `session_id` (string) the tab the version was read from.
/// @return (boolean|nil, string|nil) `true` once sent, or nil and an error.
/// @example
/// -- Spell out `lgtm` before sending:
/// maki.keymap.set("i", "<CR>", function(ev)
///   local st = ev.input
///   local s, e = st.text:find("lgtm", 1, true)
///   local version = st.version
///   if s then
///     local r = maki.ui.input_edit({
///       start = s - 1, stop = e, text = "looks good to me",
///       version = st.version, session_id = st.session_id,
///     })
///     if not r then return end
///     version = r.version
///   end
///   maki.ui.input_submit({ version = version, session_id = st.session_id })
/// end)
#[lua_fn]
async fn input_submit(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    #[ctx] caller: PluginReach,
    opts: Table,
) -> LuaResult<Pair<mlua::Value>> {
    reject_unknown_fields(&opts, SUBMIT_OPTIONS, INPUT_SUBMIT)?;
    let req = InputRequest::Submit(InputSubmit {
        version: required(&opts, INPUT_SUBMIT, "version")?,
        session_id: required(&opts, INPUT_SUBMIT, "session_id")?,
        caller: caller.now(&lua),
    });
    input_roundtrip(lua, tx.as_ref(), req).await
}

/// Sets a rule that marks the chat input in every tab, from a regex or a
/// tree-sitter query. See [chat input](#chat-input) for when to use one.
///
/// Each `ns` of your plugin holds one rule. Calling again with the same
/// `ns` replaces it, and a call with only `ns` removes it. Rules go away
/// when the plugin unloads. A bad pattern, query or mark throws. Where
/// marks overlap, [input marks](#input-marks) says which draws on top.
///
/// A regex rule marks every match of `pattern` with `mark`. The pattern
/// uses [Rust regex syntax](https://docs.rs/regex/latest/regex/#syntax).
/// Matching runs one line at a time, so a match never spans lines. The
/// pattern still sees the text before its line: `\A` is the start of the
/// input, `$` the end of the line and `(?m)^` the start of one. An empty
/// match marks a point. In `conceal` and the text of `virt_text`, `$1` is
/// capture group 1, `$0` the whole match and `$$` a plain `$`.
///
/// A tree-sitter rule parses the input as `lang` and runs `query` on it, as
/// the `treesitter` fields below describe. Parsing runs in the background,
/// and until it catches up with typing, its marks move with the text like
/// pushed marks.
///
/// @param opts table Options:
///   `ns` (string) the namespace.
///   `pattern` (string|nil) the regex to match.
///   `mark` (table|nil) the [mark](#input-marks) each match gets, without `start` and `stop`. Required with `pattern`.
///   `treesitter` (table|nil) a tree-sitter rule, in place of `pattern` and `mark`.
///   `treesitter.lang` (string) the language the input is parsed as, like `"markdown"`.
///   `treesitter.query` (string|table) the query for `lang`, or a table of language = query that gives each language in the input its own. The predicates are those of `maki.treesitter.query`, and `#set!` only takes `injection.language`.
///     - Code captured as `@injection.content` is in the language `@injection.language` caught, or the one `#set! injection.language` names. When the rule has a query for that language, the code is parsed again and that query runs there. Otherwise the code gets a `lang` mark.
///     - A `markdown` rule runs its `markdown_inline` query inside blocks by itself.
///   `treesitter.captures` (table|nil) capture name = template, with the fields of `mark`. Templates are drawn as written, `$` included.
///     - Each capture needs a template, and each template a capture.
///     - `@injection.content` may go without one. `@injection.language` and names starting with `_` take none, since they only name the injected language or feed predicates.
/// @return
/// @example
/// -- Draw `$skill:go` as a ` go ` chip the cursor steps over:
/// maki.ui.input_rules({
///   ns = "skill",
///   pattern = [[\$skill:([\w-]+)]],
///   mark = { conceal = " $1 ", hl = "accent", atomic = true },
/// })
/// -- And remove it again:
/// maki.ui.input_rules({ ns = "skill" })
/// -- Bold text without its stars, unless the cursor is on it:
/// maki.ui.input_rules({
///   ns = "md",
///   treesitter = {
///     lang = "markdown",
///     query = { markdown_inline = [[ (strong_emphasis (emphasis_delimiter) @hidden) @strong ]] },
///     captures = {
///       hidden = { conceal = "", reveal = "cursor" },
///       strong = { hl = { bold = true } },
///     },
///   },
/// })
#[lua_fn]
fn input_rules(lua: &Lua, #[ctx] plugin: Arc<str>, opts: Table) -> LuaResult<()> {
    let fail = |msg: String| mlua::Error::runtime(format!("{INPUT_RULES}: {msg}"));
    reject_unknown_fields(&opts, RULE_OPTIONS, INPUT_RULES)?;
    let ns: String = required(&opts, INPUT_RULES, "ns")?;
    let rule = match (
        opts.opt::<String>("pattern")?,
        opts.opt::<Table>("mark")?,
        opts.opt::<Table>("treesitter")?,
    ) {
        (None, None, None) => None,
        (Some(pattern), Some(mark), None) => Some(InputRule::Pattern(
            PatternRule::new(&pattern, parse_mark(&mark)?).map_err(fail)?,
        )),
        (None, None, Some(syntax)) => Some(InputRule::Syntax(Arc::new(
            parse_syntax(&syntax)?.map_err(fail)?,
        ))),
        (_, _, Some(_)) => return Err(mlua::Error::runtime(SYNTAX_ALONE_ERR)),
        _ => return Err(mlua::Error::runtime(RULE_HALVES_ERR)),
    };
    // A handler from a load that is already gone would leave a rule that
    // nothing ever takes away.
    if !plugin_loaded(lua, &plugin) {
        tracing::debug!(ns = %ns, plugin = %plugin, "input rule dropped: plugin unloaded");
        return Ok(());
    }
    if let Some(mut store) = lua.app_data_mut::<InputRuleStore>() {
        store.set(plugin, ns, rule);
    }
    Ok(())
}

fn text_edit(edit: &Table) -> LuaResult<TextEdit> {
    Ok(TextEdit {
        start: required(edit, INPUT_EDIT, "start")?,
        stop: required(edit, INPUT_EDIT, "stop")?,
        text: required(edit, INPUT_EDIT, "text")?,
    })
}

/// Opens {path} in the user's `$EDITOR` (e.g. vim, nano) and suspends the
/// TUI until the editor exits. An exit code of 0 does not mean the user
/// saved: read the file back to see what changed.
///
/// @param path string File to open.
/// @return (integer) Editor exit code, or -1 if the editor failed to start or there is no UI.
/// @example
/// local code = maki.ui.open_editor("/tmp/scratch.lua")
/// if code ~= 0 then
///   maki.ui.flash("editor exited with " .. code)
/// end
#[lua_fn]
async fn open_editor(
    _lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    path: String,
) -> LuaResult<i32> {
    let path = PathBuf::from(path);
    Ok(ui_roundtrip(tx.as_ref(), |reply_tx| UiAction::OpenEditor {
        path,
        reply_tx,
    })
    .await
    .unwrap_or(-1))
}

/// The keys an unfocused window takes while it is on screen, read through the
/// same gate `maki.keymap.set` reads, so the two can never drift.
///
/// Every key is parsed before the window is opened, so a typo leaves the
/// plugin with no window rather than a window holding half a list.
fn parse_claimed_keys(opts: &Table, focus: bool) -> LuaResult<Vec<Key>> {
    let Some(keys) = opts.opt::<Table>("keys")? else {
        return Ok(Vec::new());
    };
    if focus {
        return Err(mlua::Error::runtime(FOCUSED_CLAIM_ERR));
    }
    keys.sequence_values::<String>()
        .map(|lhs| accept_key(&lhs?))
        .collect()
}

/// Opens a floating or split window that displays the contents of {buf}.
/// Returns a Win handle you can use to receive events, update layout,
/// and close the window when you are done.
///
/// A window belongs to one session and shows only while that session's tab is
/// focused. It closes when the session leaves its tab (`/new`, loading another
/// session, closing the tab), or right away if the session is not in any tab.
///
/// @param buf Buf Buffer to display.
/// @param opts table Float configuration. Fields:
///   - width (integer|string): window width. Integer for absolute columns; "N%" for percent of terminal width. Default "60%".
///   - height (integer|string): window height. Integer for absolute rows; "N%" for percent of terminal height. Default "70%".
///   - row (integer?): row offset from the anchor corner. Negative values move up.
///   - col (integer?): column offset from the anchor corner.
///   - anchor (string): corner the (row, col) offset is relative to. One of "NW" (default), "NE", "SW", "SE". Or "input_caret", which sits the window beside the chat input cursor: the host takes the roomier side of the cursor, trims the height to what fits there, keeps the whole width on screen, and re-places it on every redraw, so it follows wraps, resizes and any modal taking focus. `row` and `col` shift the window off that spot, and `stack` grows the next one away from the cursor. With no cursor on screen, because a form, a permission prompt or a `below` split has taken the input box, it falls back to the centred default, `row` and `col` still applying.
///   - border (string): border style. One of "rounded" (default), "single", "double", "none".
///   - title (string): text shown in the top border. Default "".
///   - title_pos (string): title alignment. One of "left" (default), "center", "right".
///   - footer (table): key-hint pairs shown in the bottom border. Each entry is {key, label}. A bordered float is widened to fit its title and footer, up to the screen width.
///   - zindex (integer): stacking order. Default 50.
///   - cursor_line (boolean): highlight the focused row. Default false.
///   - reserved_top (integer): rows reserved at the top of the content area. Default 0.
///   - reserved_bottom (integer): rows reserved at the bottom of the content area. Default 0.
///   - split (string): dock the window to an edge instead of floating. One of "above", "below", "left", "right", "panel", or "" (floating, default).
///   - order (integer): paint order among split windows at the same edge. Default 50.
///   - focus (boolean): whether the window takes keyboard focus on open. Default true.
///   - keys (table): keys this window takes while it is on screen, in `maki.keymap` notation, e.g. `{ "<Tab>", "<CR>" }`. `maki.keymap.set` with `{ buffer = buf }` is the newer way to give a popup keys, with a handler per key, a mode, and `ev.fallback()`. Requires `focus = false`, since a focused window already gets every key. A claimed key goes to this window's `recv` and never reaches the chat input or global `maki.keymap.set` bindings. The window's buffer bindings are asked first. Claims are released automatically when the window closes, and a hidden or zero-size window claims nothing. Host pickers and the slash command palette take keys first while open over the window. `<C-c>` and `<C-z>` are refused.
///   - visible (boolean): whether the window is initially visible. Default true. See `win:hide()` for what hiding does.
///   - needs_input (boolean): whether the window means the session needs user input. Default false.
///   - stack (boolean): offset the window past the other stacked windows sharing its anchor, in open order, with a one row gap. Closing one moves the rest up. Floating windows only. Default false.
///   - session (string): id of the session that owns the window, e.g. `ev.data.session_id` or `ctx:session_id()`. Default: the session of the running tool call, else the focused tab's. A `maki.async.run` task keeps the tool call's session, a `maki.async.spawn` task does not.
/// @return (Win) Window handle.
/// @example
/// local buf = maki.ui.buf()
/// buf:line("Pick an option:")
/// local win = maki.ui.open_win(buf, {
///   title = "Menu",
///   width = "50%",
///   height = 10,
///   cursor_line = true,
///   footer = { { "q", "quit" }, { "Enter", "select" } },
/// })
#[lua_fn]
fn open_win(
    lua: &Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    #[ctx] plugin: Arc<str>,
    buf: mlua::AnyUserData,
    opts: Table,
) -> LuaResult<WinHandle> {
    let buf_handle = buf.borrow::<buf::BufHandle>()?;
    let session = session_opt(&opts)?.or_else(|| Lineage::current(lua).session.map(TabRef::from));
    let title: String = opts.get("title").unwrap_or_default();
    let cursor_line = opts.opt_bool("cursor_line").unwrap_or(false);
    let footer = parse_footer(&opts)?;
    let reserved_bottom: usize = opts.get("reserved_bottom").unwrap_or(0);
    let reserved_top: usize = opts.get("reserved_top").unwrap_or(0);
    let focus = opts.opt_bool("focus").unwrap_or(true);
    let keys = parse_claimed_keys(&opts, focus)?;
    let zindex: u16 = opts.get("zindex").unwrap_or(50);

    let width = parse_dimension(&opts, "width", Dimension::Percent(60));
    let height = parse_dimension(&opts, "height", Dimension::Percent(70));
    let row: Option<i16> = opts.opt("row").ok().flatten();
    let col: Option<i16> = opts.opt("col").ok().flatten();
    let anchor = parse_anchor(&opts);
    let border = parse_border(&opts);
    let title_pos = parse_title_pos(&opts);
    let split = parse_split(&opts);
    let order: u16 = opts.get("order").unwrap_or(50);
    let visible = opts.opt_bool("visible").unwrap_or(true);
    let needs_input = opts.opt_bool("needs_input").unwrap_or(false);
    let stack = opts.opt_bool("stack").unwrap_or(false);

    let config = FloatConfig {
        width,
        height,
        row,
        col,
        anchor,
        border,
        title,
        title_pos,
        footer,
        zindex,
        cursor_line,
        reserved_bottom,
        reserved_top,
        split,
        order,
        visible,
        needs_input,
        stack,
        keys,
    };

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let border_chrome = match config.border {
        Border::None => 0,
        _ => 2,
    };
    let est_w = config
        .width
        .resolve(term_cols)
        .saturating_sub(border_chrome);
    let est_h = config
        .height
        .resolve(term_rows)
        .saturating_sub(border_chrome);

    // Unbounded on both sides: a full channel would silently drop keys or,
    // worse, a Close command, leaving a zombie modal. Producers are human- or
    // plugin-rate and both ends are drained every tick, so growth is bounded
    // in practice.
    let (event_tx, event_rx) = flume::unbounded::<WinEvent>();
    let (cmd_tx, cmd_rx) = flume::unbounded::<WinCommand>();
    let waker = lua.app_data_ref::<UiWaker>().map(|waker| waker.clone());
    if let Some(waker) = &waker {
        buf_handle.buf.wake_on_change(waker);
    }
    let cmd_tx = WinSender::new(cmd_tx, waker);

    let _ = ui_send(
        tx.as_ref(),
        UiAction::OpenWin {
            session,
            buf: buf_handle.buf.clone(),
            config,
            focus,
            event_tx,
            cmd_rx,
        },
    );

    // Stamped with the plugin that opened it so unloading that plugin closes
    // it, the way its keymaps and hints are cleared. Without the stamp the
    // host has no name on the window and nothing to revoke.
    if let Some(mut store) = lua.app_data_mut::<WinStore>() {
        store.track(plugin, cmd_tx.clone());
    }

    Ok(WinHandle::new(event_rx, cmd_tx, est_w, est_h, visible))
}

#[allow(non_upper_case_globals)]
pub(crate) const set_status_hint__doc: FnDoc = FnDoc {
    name: "set_status_hint",
    args: "{spans}, {opts?}",
    desc: "Shows your plugin's key hints in the status bar. Other plugins' hints are not affected. By default the hints show in every tab, even when set from a tool call. With `opts.session` they show only in that session's tab, replacing your every-tab hints there, and are cleared when the session leaves the tab.",
    params: &[
        ParamDoc {
            name: "{spans}",
            ty: "table|nil",
            desc: "Sequence of {key, label} pairs, e.g. `{{\"q\", \"quit\"}, {\"j\", \"down\"}}`. nil clears your hints for `opts.session`, or all your hints if `opts.session` is not set.",
        },
        ParamDoc {
            name: "{opts}",
            ty: "table?",
            desc: "`session` (string): id of the session whose tab shows the hints, e.g. `ev.data.session_id` or `ctx:session_id()`. Default: every tab.",
        },
    ],
    returns: "",
    guard: None,
    value: false,
    example: "maki.ui.set_status_hint({ {\"q\", \"quit\"}, {\"j\", \"down\"} })\n-- later, clear them:\nmaki.ui.set_status_hint(nil)\n\n-- Only in the tab of the session that is running a turn:\nmaki.api.create_autocmd(\"TurnStart\", {\n  callback = function(ev)\n    maki.ui.set_status_hint({ {\"Esc\", \"cancel\"} }, { session = ev.data.session_id })\n  end,\n})\nmaki.api.create_autocmd(\"TurnEnd\", {\n  callback = function(ev)\n    maki.ui.set_status_hint(nil, { session = ev.data.session_id })\n  end,\n})",
};

/// The UI's part of `maki.ui`, registered by [`add_ui_fns`], ranked right
/// after the render domain's `maki.ui` docs so the reference keeps its order.
pub(crate) const UI_DOCS: ModuleDoc = ModuleDoc {
    name: "maki.ui",
    kind: DocKind::Table,
    desc: "",
    fns: &[
        set_mode__doc,
        flash__doc,
        action__doc,
        open_editor__doc,
        open_win__doc,
        set_status_hint__doc,
        set_window_title__doc,
        input__doc,
        input_rules__doc,
        input_decorate__doc,
        input_edit__doc,
        input_submit__doc,
        register_renderer__doc,
        renderer__doc,
        has_renderer__doc,
    ],
};

pub(crate) fn add_ui_fns(
    t: &Table,
    lua: &Lua,
    ui_action_tx: Option<flume::Sender<UiAction>>,
    plugin: Arc<str>,
    reach: PluginReach,
) -> LuaResult<()> {
    flash__register(t, lua, ui_action_tx.clone(), Arc::clone(&plugin))?;
    set_window_title__register(t, lua, ui_action_tx.clone())?;
    set_mode__register(t, lua, Arc::clone(&plugin))?;
    action__register(
        t,
        lua,
        ui_action_tx.clone(),
        Arc::clone(&plugin),
        reach.clone(),
    )?;
    open_editor__register(t, lua, ui_action_tx.clone())?;
    input__register(t, lua, reach.permissions(), ui_action_tx.clone())?;
    input_edit__register(
        t,
        lua,
        reach.permissions(),
        ui_action_tx.clone(),
        Arc::clone(&plugin),
        reach.clone(),
    )?;
    input_decorate__register(t, lua, ui_action_tx.clone(), Arc::clone(&plugin))?;
    input_rules__register(t, lua, Arc::clone(&plugin))?;
    input_submit__register(t, lua, ui_action_tx.clone(), reach)?;
    open_win__register(t, lua, ui_action_tx, Arc::clone(&plugin))?;
    register_renderer__register(t, lua, Arc::clone(&plugin))?;
    renderer__register(t, lua)?;
    has_renderer__register(t, lua)?;

    let p = Arc::clone(&plugin);
    t.set(
        SET_STATUS_HINT,
        lua.create_function(move |lua, (value, opts): (mlua::Value, Option<Table>)| {
            let session = match &opts {
                Some(opts) => {
                    reject_unknown_fields(opts, &[SESSION_OPT], SET_STATUS_HINT)?;
                    session_opt(opts)?
                }
                None => None,
            };
            let spans = match value {
                mlua::Value::Nil => Vec::new(),
                mlua::Value::Table(tbl) => tbl
                    .sequence_values::<Table>()
                    .map(|entry| {
                        let entry = entry?;
                        Ok((
                            entry.get::<String>(1)?,
                            entry.get::<String>(2).unwrap_or_default(),
                        ))
                    })
                    .collect::<LuaResult<_>>()?,
                _ => {
                    return Err(mlua::Error::runtime(
                        "set_status_hint expects a table or nil",
                    ));
                }
            };
            if let Some(mut store) = lua.app_data_mut::<HintStore>() {
                store.set(Arc::clone(&p), session, spans);
            }
            publish_hint_snapshot(lua);
            Ok(())
        })?,
    )?;

    Ok(())
}

#[cfg(test)]
pub(crate) fn create_ui_table(
    lua: &Lua,
    ui_action_tx: Option<flume::Sender<UiAction>>,
    plugin: Arc<str>,
    reach: PluginReach,
) -> LuaResult<Table> {
    let t = lua.create_table()?;
    crate::buffer::render::add_bridge_fns(&t, lua)?;
    add_ui_fns(&t, lua, ui_action_tx, plugin, reach)?;
    Ok(t)
}

pub(crate) fn try_parse_dimension(tbl: &Table, key: &str) -> Option<Dimension> {
    if let Ok(s) = tbl.get::<String>(key)
        && let Some(pct) = s.strip_suffix('%')
        && let Ok(v) = pct.parse::<u16>()
    {
        return Some(Dimension::Percent(v));
    }
    if let Ok(v) = tbl.get::<u16>(key) {
        return Some(Dimension::Abs(v));
    }
    None
}

pub(crate) fn parse_dimension(tbl: &Table, key: &str, default: Dimension) -> Dimension {
    try_parse_dimension(tbl, key).unwrap_or(default)
}

fn parse_anchor(tbl: &Table) -> Anchor {
    tbl.get::<String>("anchor")
        .map(|s| Anchor::parse(&s))
        .unwrap_or_default()
}

fn parse_split(tbl: &Table) -> Split {
    tbl.get::<String>("split")
        .map(|s| Split::parse(&s))
        .unwrap_or_default()
}

fn parse_border(tbl: &Table) -> Border {
    tbl.get::<String>("border")
        .map(|s| Border::parse(&s))
        .unwrap_or_default()
}

fn parse_title_pos(tbl: &Table) -> TitlePos {
    tbl.get::<String>("title_pos")
        .map(|s| TitlePos::parse(&s))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ui::marks::CONTROL_CHAR_ERR;
    use crate::api::util::command::{NO_UI_ERR, UiReply};
    use crate::api::util::convert::{REQUIRED_ERR, UNKNOWN_FIELD_ERR, lua_to_json};
    use crate::plugin_permissions::{Permission, PluginPermissions, denied_error};
    use crate::runtime::mark_loaded;
    use mlua::Lua;
    use test_case::test_case;

    fn opts(lua: &Lua, fields: &str) -> Table {
        lua.load(format!("return {{ {fields} }}")).eval().unwrap()
    }

    #[test_case("", &[] ; "missing")]
    #[test_case("footer = 'not a table'", &[] ; "not_a_table")]
    #[test_case("footer = {{'q', 'quit'}, {'j', 'down'}}", &[("q", "quit"), ("j", "down")] ; "keeps_order")]
    fn footer_entries(fields: &str, expected: &[(&str, &str)]) {
        let lua = Lua::new();
        let parsed = parse_footer(&opts(&lua, fields)).unwrap();
        let expected: Vec<_> = expected
            .iter()
            .map(|(key, label)| (key.to_string(), label.to_string()))
            .collect();
        assert_eq!(parsed, expected);
    }

    #[test_case("footer = {{'q'}}" ; "missing_label")]
    #[test_case("footer = {{'q', {}}}" ; "non_string_label")]
    fn footer_rejects_incomplete_entries(fields: &str) {
        let lua = Lua::new();
        assert!(parse_footer(&opts(&lua, fields)).is_err());
    }

    fn claim_opts(lua: &Lua, claims: &[&str]) -> Table {
        let opts = lua.create_table().unwrap();
        opts.raw_set(
            "keys",
            lua.create_sequence_from(claims.iter().copied()).unwrap(),
        )
        .unwrap();
        opts
    }

    #[test_case("keys = {'<Tab>', '<C-n>'}", &["<Tab>", "<C-n>"] ; "keymap_notation")]
    #[test_case("", &[] ; "no_keys")]
    fn claimed_keys_are_parsed_in_the_notation_keymaps_use(fields: &str, keys: &[&str]) {
        let lua = Lua::new();
        let expected: Vec<_> = keys.iter().map(|key| Key::parse(key).unwrap()).collect();
        assert_eq!(
            parse_claimed_keys(&opts(&lua, fields), keys.is_empty()).unwrap(),
            expected
        );
    }

    /// The same two keys `maki.keymap.set` refuses, from the same list. A
    /// window claiming one would publish a key the host answers first, which
    /// is a popup that never sees it and a user who cannot quit.
    #[test_case("<C-c>" ; "quit")]
    #[test_case("<C-z>" ; "suspend")]
    fn a_window_cannot_claim_a_key_the_host_reserves(lhs: &str) {
        let lua = Lua::new();
        let opts = claim_opts(&lua, &[lhs]);

        let err = parse_claimed_keys(&opts, false).unwrap_err().to_string();
        assert!(
            err.contains(lhs),
            "the error has to name the key, got: {err}"
        );
    }

    /// A focused window is handed every key already, so a list of keys to take
    /// on top of that is an author with the wrong model of who is reading the
    /// keyboard, not a redundant option to quietly drop.
    #[test]
    fn claiming_keys_on_a_focused_window_is_an_error() {
        let lua = Lua::new();
        let opts = claim_opts(&lua, &["<Tab>"]);

        let err = parse_claimed_keys(&opts, true).unwrap_err().to_string();
        assert!(err.contains(FOCUSED_CLAIM_ERR), "got: {err}");
    }

    #[test_case("width = 42", Some(Dimension::Abs(42)) ; "absolute")]
    #[test_case("width = '200%'", Some(Dimension::Percent(200)) ; "percent")]
    #[test_case("", None ; "missing")]
    #[test_case("width = 'abc'", None ; "invalid_string")]
    #[test_case("width = 'xx%'", None ; "invalid_percent")]
    fn dimensions(fields: &str, expected: Option<Dimension>) {
        let lua = Lua::new();
        let table = opts(&lua, fields);
        let default = Dimension::Percent(60);
        assert_eq!(try_parse_dimension(&table, "width"), expected);
        assert_eq!(
            parse_dimension(&table, "width", default),
            expected.unwrap_or(default)
        );
    }

    #[test_case("", Anchor::NW, Border::Rounded, TitlePos::Left ; "defaults")]
    #[test_case("anchor='invalid', border='invalid', title_pos='invalid'", Anchor::NW, Border::Rounded, TitlePos::Left ; "invalid")]
    #[test_case("anchor='NE', border='none', title_pos='center'", Anchor::NE, Border::None, TitlePos::Center ; "top_right")]
    #[test_case("anchor='SW', border='single', title_pos='right'", Anchor::SW, Border::Single, TitlePos::Right ; "bottom_left")]
    #[test_case("anchor='SE', border='double'", Anchor::SE, Border::Double, TitlePos::Left ; "bottom_right")]
    #[test_case("anchor='input_caret'", Anchor::InputCaret, Border::Rounded, TitlePos::Left ; "input_caret")]
    #[test_case("anchor='NW', border='rounded', title_pos='left'", Anchor::NW, Border::Rounded, TitlePos::Left ; "explicit_defaults")]
    #[test_case("title_pos=''", Anchor::NW, Border::Rounded, TitlePos::Left ; "empty_title_position")]
    fn window_enums(fields: &str, anchor: Anchor, border: Border, title: TitlePos) {
        let lua = Lua::new();
        let table = opts(&lua, fields);
        assert_eq!(parse_anchor(&table), anchor);
        assert_eq!(parse_border(&table), border);
        assert_eq!(parse_title_pos(&table), title);
    }

    #[test_case(false, &[] ; "without_session_clears_every_slot")]
    #[test_case(true, &[None] ; "with_session_clears_only_its_slot")]
    fn hint_store_empty_spans_clear(with_session: bool, left: &[Option<TabRef>]) {
        let mut store = HintStore::new();
        let session = TabRef::from(MakiId::generate());
        store.set(Arc::from("plug"), None, spans("a"));
        store.set(Arc::from("plug"), Some(session.clone()), spans("b"));

        store.set(
            Arc::from("plug"),
            with_session.then_some(session),
            Vec::new(),
        );

        let sessions: Vec<_> = store
            .snapshot_entries()
            .into_iter()
            .map(|e| e.session)
            .collect();
        assert_eq!(sessions, left);
    }

    #[test]
    fn hint_store_deterministic_order() {
        let mut store = HintStore::new();
        store.set(Arc::from("zzz"), None, spans("z"));
        store.set(Arc::from("aaa"), None, spans("a"));
        let entries = store.snapshot_entries();
        assert_eq!(entries[0].plugin.as_ref(), "aaa");
        assert_eq!(entries[1].plugin.as_ref(), "zzz");
    }

    fn spans(text: &str) -> Vec<(String, String)> {
        vec![(text.into(), String::new())]
    }

    const WIN_PLUGIN: &str = "plug";
    const OTHER_WIN_PLUGIN: &str = "other";
    const WINDOW_LEFT_OPEN: &str =
        "unloading a plugin has to close the window it left on screen holding keys";
    const WINDOW_TAKEN_DOWN: &str = "another plugin's window must survive the unload";

    /// What `/reload` costs without this: the float stays up with its key loop
    /// cancelled, so every key it claimed is taken from the user and dropped
    /// until the Lua collector happens to run.
    #[test]
    fn unloading_a_plugin_closes_the_windows_it_opened() {
        let mut store = WinStore::default();
        let (mine, mine_rx) = flume::unbounded::<WinCommand>();
        let (theirs, theirs_rx) = flume::unbounded::<WinCommand>();
        store.track(Arc::from(WIN_PLUGIN), WinSender::new(mine, None));
        store.track(Arc::from(OTHER_WIN_PLUGIN), WinSender::new(theirs, None));

        store.close_plugin(WIN_PLUGIN);

        assert!(
            matches!(mine_rx.try_recv(), Ok(WinCommand::Close)),
            "{WINDOW_LEFT_OPEN}"
        );
        assert!(theirs_rx.try_recv().is_err(), "{WINDOW_TAKEN_DOWN}");
    }

    /// A popup opened and closed on every keystroke must not grow the list it
    /// is tracked in for the rest of the run.
    #[test]
    fn a_window_already_gone_is_forgotten_on_the_next_open() {
        let mut store = WinStore::default();
        let (gone, gone_rx) = flume::unbounded::<WinCommand>();
        store.track(Arc::from(WIN_PLUGIN), WinSender::new(gone, None));
        drop(gone_rx);

        let (live, _live_rx) = flume::unbounded::<WinCommand>();
        store.track(Arc::from(WIN_PLUGIN), WinSender::new(live, None));

        assert_eq!(store.open[WIN_PLUGIN].len(), 1);
    }

    const STALE_RANGE_ERR: &str = "stop 99 is past the end of the input (5)";
    const READ_SESSION_ID: &str = "11111111-1111-1111-1111-111111111111";
    const INPUT_PLUGIN: &str = "test";

    /// Stands in for the focused session's input box: reads answer with a
    /// snapshot, edits answer with whatever {edit} decides.
    fn ui_with_input(edit: fn(InputEdit) -> UiReply) -> Lua {
        ui_with_input_granted(edit, PluginPermissions::trusted())
    }

    fn ui_with_input_granted(edit: fn(InputEdit) -> UiReply, grants: PluginPermissions) -> Lua {
        let lua = ui_with_input_of_unloaded(edit, grants);
        mark_loaded(&lua, INPUT_PLUGIN);
        lua
    }

    /// [`ui_with_input`] for a plugin whose load is gone.
    fn ui_with_input_of_unloaded(edit: fn(InputEdit) -> UiReply, grants: PluginPermissions) -> Lua {
        let (tx, rx) = flume::unbounded::<UiAction>();
        std::thread::spawn(move || {
            while let Ok(UiAction::Input { req, reply_tx }) = rx.recv() {
                let reply = match req {
                    InputRequest::Read => Ok(serde_json::json!({
                        "session_id": READ_SESSION_ID,
                        "text": "hello",
                        "cursor": 5,
                        "version": 7,
                    })),
                    InputRequest::Edit(edit_req) => edit(edit_req),
                    InputRequest::Decorate(req) => Ok(serde_json::json!({
                        "marks": marks_json(Some(&req.marks)),
                        "version": req.version,
                        "session_id": req.session_id,
                        "plugin": req.plugin,
                    })),
                    InputRequest::Submit(_) => Ok(serde_json::json!(true)),
                };
                let _ = reply_tx.send(reply);
            }
        });
        lua_with_ui_granted(Some(tx), grants)
    }

    fn lua_with_ui(tx: Option<flume::Sender<UiAction>>) -> Lua {
        lua_with_ui_granted(tx, PluginPermissions::trusted())
    }

    fn lua_with_ui_granted(tx: Option<flume::Sender<UiAction>>, grants: PluginPermissions) -> Lua {
        let lua = Lua::new();
        let reach = PluginReach::new(grants).owned_by(Arc::from(INPUT_PLUGIN));
        let t = create_ui_table(&lua, tx, Arc::from(INPUT_PLUGIN), reach).unwrap();
        lua.globals().set("ui", t).unwrap();
        lua
    }

    /// Echoes the request back, so a test can assert on what the UI would have
    /// been asked to do.
    fn echo_edit(edit: InputEdit) -> UiReply {
        let edits: Vec<_> = edit
            .edits
            .iter()
            .map(|e| serde_json::json!({ "start": e.start, "stop": e.stop, "text": e.text }))
            .collect();
        Ok(serde_json::json!({
            "edits": edits,
            "cursor": edit.cursor,
            "version": edit.version,
            "strict": edit.strict,
            "session_id": edit.session_id,
            "plugin": edit.caller.owner(),
            "marks": marks_json(edit.marks.as_ref()),
        }))
    }

    fn marks_json(marks: Option<&InputMarks>) -> serde_json::Value {
        let Some(InputMarks { ns, marks, .. }) = marks else {
            return serde_json::Value::Null;
        };
        let marks: Vec<_> = marks
            .iter()
            .map(|mark| {
                serde_json::json!({
                    "start": mark.start,
                    "stop": mark.stop,
                    "conceal": mark.decoration.conceal,
                    "atomic": mark.decoration.atomic,
                })
            })
            .collect();
        serde_json::json!({ "ns": ns, "marks": marks })
    }

    const PUSHED_MARKS: &str = r#"ns = "chip", marks = { { start = 5 }, { start = 0, stop = 2, conceal = "[h]", atomic = true } }"#;

    fn decorate_script(fields: &str) -> String {
        format!(
            r#"local st = ui.input()
               return ui.input_decorate({{
                 {fields},
                 version = st.version, session_id = st.session_id,
               }})"#
        )
    }

    /// Both guards reach the UI, or a call planned against text that moved on
    /// lands anyway. The host stamps on the plugin name, so a caller cannot
    /// claim another plugin's.
    #[test_case(decorate_script(PUSHED_MARKS) ; "input_decorate")]
    #[test_case(edit_script(&format!(r#"start = 0, stop = 0, text = "x", {PUSHED_MARKS}"#)) ; "input_edit")]
    #[tokio::test]
    async fn marks_reach_the_ui_with_the_guards_and_the_plugin(script: String) {
        let lua = ui_with_input(echo_edit);
        let (val, err) = eval(&lua, &script).await;
        assert_eq!(err, None);
        assert_eq!(
            serde_json::json!({
                "marks": val["marks"],
                "version": val["version"],
                "session_id": val["session_id"],
                "plugin": val["plugin"],
            }),
            serde_json::json!({
                "marks": {
                    "ns": "chip",
                    "marks": [
                        { "start": 5, "atomic": false },
                        { "start": 0, "stop": 2, "conceal": "[h]", "atomic": true },
                    ],
                },
                "version": 7,
                "session_id": READ_SESSION_ID,
                "plugin": INPUT_PLUGIN,
            })
        );
    }

    /// The unload that clears a plugin's marks has already reached the UI, so
    /// marks pushed after it would stay for good.
    #[test_case(decorate_script(PUSHED_MARKS) ; "input_decorate")]
    #[test_case(edit_script(&format!(r#"start = 0, stop = 0, text = "x", {PUSHED_MARKS}"#)) ; "input_edit")]
    #[tokio::test]
    async fn a_plugin_whose_load_is_gone_pushes_no_marks(script: String) {
        let lua = ui_with_input_of_unloaded(echo_edit, PluginPermissions::trusted());
        let (val, err) = eval(&lua, &script).await;
        assert_eq!(val, serde_json::Value::Null);
        assert_eq!(err.as_deref(), Some(UNLOADED_MARKS_ERR));
    }

    /// Each of these would otherwise go quietly wrong. A table read as a list
    /// keeps only its list part, so a single edit in place of `edits` reads
    /// as none, and a single mark in place of `marks` wipes `ns`. A misspelled
    /// option drops what it asks for, like `strict`. A `text` to send reads
    /// as "send this", while what goes out is the text on screen.
    #[test_case(decorate_script(r#"ns = "a", marks = {}, strict = true"#), UNKNOWN_FIELD_ERR ; "decorate_an_option_it_does_not_take")]
    #[test_case(decorate_script(r#"ns = "a", marks = { start = 0, stop = 1 }"#), NOT_A_LIST_ERR ; "decorate_a_single_mark_in_place_of_the_list")]
    #[test_case(decorate_script(r#"ns = "a", marks = { { stop = 1 } }"#), REQUIRED_ERR ; "decorate_a_mark_without_start")]
    #[test_case(decorate_script(r#"ns = "a", marks = { { start = 0, stopp = 1 } }"#), UNKNOWN_FIELD_ERR ; "decorate_a_misspelled_mark_field")]
    #[test_case(decorate_script(r#"ns = "a", marks = { { start = 0, conceal = "a\nb" } }"#), CONTROL_CHAR_ERR ; "decorate_a_control_char_in_drawn_text")]
    #[test_case(edit_script(r#"start = 0, stop = 0, text = "x", marks = {}"#), MARKS_HALVES_ERR ; "edit_marks_without_ns")]
    #[test_case(edit_script(r#"start = 0, stop = 0, text = "x", ns = "a""#), MARKS_HALVES_ERR ; "edit_ns_without_marks")]
    #[test_case(edit_script(r#"edits = { start = 0, stop = 0, text = "x" }, cursor = 1"#), NOT_A_LIST_ERR ; "edit_a_single_edit_in_place_of_the_list")]
    #[test_case(edit_script(r#"edits = { { start = 0, stop = 0, text = "x" }, cursor = 1 }"#), NOT_A_LIST_ERR ; "edit_an_option_inside_the_list")]
    #[test_case(edit_script(r#"start = 0, stop = 0, text = "x", ns = "a", marks = { start = 0, stop = 1 }"#), NOT_A_LIST_ERR ; "edit_a_single_mark_in_place_of_the_marks")]
    #[test_case(edit_script(r#"edits = { { start = 0, stop = 0, text = "a" } }, text = "b""#), MIXED_EDIT_SHAPES_ERR ; "edit_an_edits_list_next_to_a_single_edit")]
    #[test_case(edit_script(r#"start = 0, stop = 0, text = "x", strcit = true"#), UNKNOWN_FIELD_ERR ; "edit_a_misspelled_option")]
    #[test_case(edit_script(r#"edits = { { start = 0, stop = 0, text = "x", cursor = 1 } }"#), UNKNOWN_FIELD_ERR ; "edit_an_option_inside_an_edit")]
    #[test_case(r#"return ui.input_submit({ version = 7, session_id = "s", text = "hi" })"#.to_owned(), UNKNOWN_FIELD_ERR ; "submit_a_text_to_send")]
    #[tokio::test]
    async fn input_calls_throw_on(script: String, expected: &str) {
        let lua = ui_with_input(echo_edit);
        let err = eval_err(&lua, &script).await;
        assert!(err.contains(expected), "{err}");
    }

    #[test_case(r#"ui.input_decorate({ ns = "a", marks = {}, version = 0, session_id = "s" })"# ; "input_decorate")]
    #[test_case(r#"ui.input_submit({ version = 0, session_id = "s" })"# ; "input_submit")]
    #[tokio::test]
    async fn without_a_ui_answers_the_error_slot(call: &str) {
        let lua = lua_with_ui(None);
        mark_loaded(&lua, INPUT_PLUGIN);
        let (val, err) = eval(&lua, &format!("return {call}")).await;
        assert_eq!(val, serde_json::Value::Null);
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    /// Enter sends the text, so changing it costs what sending a message
    /// does, and a plugin without that learns at the call. Moving the cursor
    /// only needs `input`.
    #[test_case(r#"start = 0, stop = 0, text = "x""# => true ; "inserting_text")]
    #[test_case(r#"start = 0, stop = 5, text = """# => true ; "deleting_text")]
    #[test_case(r#"edits = {}, cursor = 2"# => false ; "moving_the_cursor")]
    #[tokio::test]
    async fn changing_the_text_needs_what_a_message_needs(range: &str) -> bool {
        let grants = PluginPermissions::from_approved(["input", "session_read"]);
        let lua = ui_with_input_granted(echo_edit, grants);
        let denied = denied_error(Permission::Agent).to_string();
        lua.load(edit_script(range))
            .eval_async::<mlua::Value>()
            .await
            .is_err_and(|err| err.to_string().contains(&denied))
    }

    /// Both guards are required, so a test only ever varies the range.
    fn edit_script(range: &str) -> String {
        format!(
            r#"local st = ui.input()
               return ui.input_edit({{
                 {range},
                 version = st.version, session_id = st.session_id,
               }})"#
        )
    }

    async fn eval(lua: &Lua, script: &str) -> (serde_json::Value, Option<String>) {
        let (val, err): (mlua::Value, Option<String>) =
            lua.load(script).eval_async().await.unwrap();
        (lua_to_json(lua, &val).unwrap(), err)
    }

    async fn eval_err(lua: &Lua, script: &str) -> String {
        lua.load(script)
            .eval_async::<mlua::Value>()
            .await
            .unwrap_err()
            .to_string()
    }

    #[tokio::test]
    async fn input_reports_text_cursor_and_version() {
        let lua = ui_with_input(echo_edit);
        let (val, err) = eval(&lua, "return ui.input()").await;
        assert_eq!(err, None);
        assert_eq!(val["text"], "hello");
        assert_eq!(val["cursor"], 5);
        assert_eq!(val["version"], 7);
        assert_eq!(val["session_id"], READ_SESSION_ID);
    }

    #[test_case(
        r#"start = 1, stop = 3, text = "xy""#,
        serde_json::json!({ "edits": [{ "start": 1, "stop": 3, "text": "xy" }], "cursor": null, "strict": false })
        ; "single_edit_with_defaults"
    )]
    #[test_case(
        r#"edits = { { start = 0, stop = 0, text = "**" }, { start = 5, stop = 5, text = "**" } }, cursor = 7, strict = true"#,
        serde_json::json!({
            "edits": [{ "start": 0, "stop": 0, "text": "**" }, { "start": 5, "stop": 5, "text": "**" }],
            "cursor": 7,
            "strict": true,
        })
        ; "edits_list_with_options"
    )]
    #[test_case(
        "edits = {}, cursor = 2",
        serde_json::json!({ "edits": [], "cursor": 2, "strict": false })
        ; "no_edits_with_a_cursor"
    )]
    #[tokio::test]
    async fn input_edit_forwards_the_edits_and_options(fields: &str, expected: serde_json::Value) {
        let lua = ui_with_input(echo_edit);
        let (val, err) = eval(&lua, &edit_script(fields)).await;
        assert_eq!(err, None);
        assert_eq!(
            serde_json::json!({ "edits": val["edits"], "cursor": val["cursor"], "strict": val["strict"] }),
            expected
        );
    }

    /// Leaving a required key out has to fail loudly instead of writing
    /// unguarded, and that includes {text}: a misspelled key defaulting to
    /// the empty string would delete the range the call meant to replace.
    #[test_case("version" ; "version")]
    #[test_case("session_id" ; "session_id")]
    #[test_case("text" ; "text")]
    #[tokio::test]
    async fn input_edit_refuses_to_write_without_a_required_key(missing: &str) {
        let lua = ui_with_input(echo_edit);
        let kept = [
            "version = st.version",
            "session_id = st.session_id",
            r#"text = "x""#,
        ]
        .iter()
        .filter(|key| !key.starts_with(missing))
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
        let err = eval_err(
            &lua,
            &format!(
                r#"local st = ui.input()
                   return ui.input_edit({{ start = 0, stop = 5, {kept} }})"#
            ),
        )
        .await;
        assert!(err.contains(missing), "the error has to name it: {err}");
    }

    /// A handler writes after the key that woke it, so a range the user has
    /// typed past comes back as an error instead of landing somewhere else.
    #[tokio::test]
    async fn input_edit_answers_a_stale_range_in_the_error_slot() {
        let lua = ui_with_input(|_| Err(STALE_RANGE_ERR.into()));
        let (val, err) = eval(&lua, &edit_script("start = 0, stop = 99, text = \"x\"")).await;
        assert_eq!(val, serde_json::Value::Null);
        assert_eq!(err.as_deref(), Some(STALE_RANGE_ERR));
    }

    /// An action is priced like the API doing the same thing, so binding a
    /// key to it cannot reach further than calling that API would.
    #[test_case("plan_toggle", None ; "plan_toggle_is_free")]
    #[test_case("pop_queue", Some(Permission::SessionWrite) ; "pop_queue_needs_session_write")]
    #[test_case("file_picker", None ; "file_picker_is_free")]
    #[tokio::test]
    async fn action_charges_what_its_api_would(name: &str, needed: Option<Permission>) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        std::thread::spawn(move || {
            while let Ok(action) = rx.recv() {
                if let UiAction::Builtin { reply_tx, .. } = action {
                    let _ = reply_tx.send(true);
                }
            }
        });
        let run = async |perms: &PluginPermissions| {
            let lua = Lua::new();
            let reach = PluginReach::new(perms.clone());
            let t =
                create_ui_table(&lua, Some(tx.clone()), Arc::from(INPUT_PLUGIN), reach).unwrap();
            lua.globals().set("ui", t).unwrap();
            lua.load(format!("return ui.action('{name}')"))
                .eval_async::<Option<bool>>()
                .await
                .map_err(|e| e.to_string())
        };
        let denied = run(&PluginPermissions::denied()).await;
        let Some(perm) = needed else {
            assert_eq!(denied, Ok(Some(true)));
            return;
        };
        let err = denied.unwrap_err();
        assert!(err.contains(&denied_error(perm).to_string()), "{err}");
        let mut granted = PluginPermissions::denied();
        granted.set(perm, true);
        assert_eq!(run(&granted).await, Ok(Some(true)));
    }
}
