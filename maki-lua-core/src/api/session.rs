//! `maki.session`: host session primitives. Session management round-trips to
//! the UI event loop, which owns live runtimes and storage. `notify` posts
//! directly to the agent mailbox so synchronous callbacks can use it.
//!
//! Transcripts cross into Lua in the serde shape of [`Message`], the same
//! document `maki session export` prints. The one difference is an image
//! block's `source`, which is a `maki.image.Image` sharing the payload.

use std::sync::Arc;

use maki_lua_macro::{lua_fn, lua_table};
use maki_storage::StateDir;
use maki_storage::id::MakiId;
use maki_types::message::{ContentBlock, Message, Role};
use maki_types::session::StoredSession;
use maki_types::transcript::{SessionExport, Transcript};
use mlua::{IntoLua, Lua, LuaSerdeExt, Result as LuaResult, Table, Value};
use serde_json::{Map, Value as JsonValue};
use tokio::task::spawn_blocking;

use crate::api::image::LuaImage;
use crate::api::util::convert::{TableExt, json_to_lua, json_to_lua_exact, lua_to_json};
use crate::api::util::pair::{Pair, err_pair, pair, try_pair};
use crate::facade::{FacadeOps, FacadeRequest, NO_UI_ERR, SessionRequest};
use crate::plugin_permissions::{Permission, PluginPermissions, require};
use crate::session_snapshot::SessionSnapshot;

pub const BLANK_NOTIFY_ERR: &str = "text must not be blank";
pub const SESSION_REQUIRED_ERR: &str = "session is required";
const NOT_LIVE_ERR: &str = "session not live";
pub const NO_FOCUSED_ERR: &str = "no focused session";
pub const EMPTY_ARRAY_ERR: &str =
    "expected an array, got an empty table (copy with maki.deepcopy to keep it a list)";
/// Tables built between two yields, so a long history never holds the
/// shared Lua thread near its budget.
const MESSAGES_PER_YIELD: usize = 256;
pub const MESSAGES_KEY: &str = "messages";
pub const CONTENT_KEY: &str = "content";
const SOURCE_KEY: &str = "source";
const TYPE_KEY: &str = "type";
const BLOCK_IMAGE: &str = "image";
const CWD_KEY: &str = "cwd";
const PROMPT_KEY: &str = "prompt";
const FOCUS_KEY: &str = "focus";
/// What makes the chat input run a line on Enter rather than send it.
const RUN_PREFIXES: [char; 2] = ['/', '!'];
const EXIT_WORD: &str = "exit";
/// What `new` takes from the export document. Anything else is ignored, like
/// an importer ignores unknown envelope fields, so a newer export still goes
/// back in.
const DOCUMENT_KEYS: [&str; 8] = [
    "format",
    "model",
    "mode",
    "thinking",
    "fast",
    "title",
    "facts",
    MESSAGES_KEY,
];

/// Rewind and fork put a user turn back in the chat input, and Enter runs it
/// as if a plugin that could send prompts wrote it. So a turn that runs
/// needs what sending it took, which an import alone does not prove.
fn runs_on_enter(message: &Message) -> bool {
    message.role == Role::User
        && !message.is_from_host()
        && !message.is_context_update()
        && message.user_text().is_some_and(|text| {
            let text = text.trim();
            text.starts_with(RUN_PREFIXES) || text == EXIT_WORD
        })
}

pub fn not_live(session: MakiId) -> String {
    format!("{NOT_LIVE_ERR}: {session}")
}

/// Some calls must name their session. Falling back to the focused tab would
/// make them follow the user from tab to tab.
pub fn required_session(opts: Option<&Table>) -> LuaResult<Result<MakiId, String>> {
    let Some(raw) = opts
        .map(|t| t.opt::<String>("session"))
        .transpose()?
        .flatten()
    else {
        return Ok(Err(SESSION_REQUIRED_ERR.to_owned()));
    };
    Ok(raw.parse::<MakiId>().map_err(|e| e.to_string()))
}

/// The UI answers stored reads from its own state dir. Without one, an
/// embedder may pin it as app data, else it resolves like the CLI's.
fn state_dir(lua: &Lua) -> Result<StateDir, String> {
    match lua.app_data_ref::<StateDir>() {
        Some(dir) => Ok(dir.clone()),
        None => StateDir::resolve().map_err(|e| e.to_string()),
    }
}

/// Reads a stored session off the Lua thread without claiming it, so one
/// another maki has open still answers. `answer` runs off the thread too, so
/// a long transcript is never dropped on it.
async fn from_disk<T: Send + 'static>(
    lua: &Lua,
    ops: &FacadeOps,
    id: MakiId,
    answer: impl FnOnce(StoredSession) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let local = Arc::clone(ops.local()?);
    let dir = state_dir(lua)?;
    spawn_blocking(move || answer(local.read_stored(id, &dir)?))
        .await
        .map_err(|e| e.to_string())
        .flatten()
}

async fn roundtrip(lua: Lua, ops: FacadeOps, req: SessionRequest) -> LuaResult<Pair<Value>> {
    ops.ask_json(&lua, FacadeRequest::Session(req)).await
}

/// Lists sessions stored for the current project. Answered from a
/// background scan, so a slow disk never blocks the UI.
///
/// @return (table|nil, string|nil) Array of `{id, title, updated_at}`, or nil and an error.
/// @example
/// local stored, err = maki.session.list()
#[lua_fn]
async fn list(lua: Lua, #[ctx] ops: FacadeOps) -> LuaResult<Pair<Value>> {
    roundtrip(lua, ops, SessionRequest::List).await
}

/// Lists the sessions currently running in this UI. Status is "working",
/// "needs_input", or "idle". A mailbox follow-up stays "working" without an
/// intermediate "idle" status.
///
/// @return (table|nil, string|nil) Array of `{id, title, status, updated_at, focused}`, or nil and an error.
/// @example
/// local live, err = maki.session.live()
#[lua_fn]
async fn live(lua: Lua, #[ctx] ops: FacadeOps) -> LuaResult<Pair<Value>> {
    roundtrip(lua, ops, SessionRequest::Live).await
}

/// A session no host here answers for: one a headless driver runs from what
/// it publishes, any other from disk without claiming it.
async fn unhosted_snapshot(lua: &Lua, ops: &FacadeOps, raw: &str) -> Result<JsonValue, String> {
    let id = raw.parse::<MakiId>().map_err(|e| e.to_string())?;
    let snapshot = match ops.live_header(id) {
        Some(header) => SessionSnapshot::live_headless(id, Arc::unwrap_or_clone(header)),
        None => {
            from_disk(lua, ops, id, |session| {
                Ok(SessionSnapshot::stored(&session))
            })
            .await?
        }
    };
    serde_json::to_value(snapshot).map_err(|e| e.to_string())
}

/// One-call snapshot of a session: queue, usage, context, cost, mode, and
/// status. Reads the focused session, or the one you name in `session`: a
/// background tab, or a stored session nothing has open.
///
/// Metadata only, so it needs no permission. Not to be confused with the
/// `session_read` permission, which guards transcript content
/// (`maki.session.export`, `maki.session.messages`).
///
/// The returned table:
/// ```text
/// {
///   id, cwd, model, mode = "build" | "plan",
///   thinking, fast,    -- nil under headless drivers
///   status = "idle" | "working" | "needs_input" | "stored",
///   focused, updated_at,
///   usage = { input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens },
///   context_size,
///   context_window,    -- nil for a stored session
///   cost,              -- nil for a stored session
///   queue = { count }, -- nil under headless drivers and for a stored session
///   title,             -- nil under headless drivers
/// }
/// ```
///
/// `status` is `"stored"` for a session nothing here runs. Its `model` is the
/// one it was saved with, resolved only when it opens.
///
/// `usage` and `cost` include subagent spend. `context_size` is the main
/// session's own, since a subagent runs its own window. There is no
/// `list_cost` here: the un-subsidised total for a run arrives on the
/// `TurnEnd` autocmd, and `maki.model.info` carries the rates behind it.
///
/// @param opts table? `session` (string?) Session id, live or stored. Defaults to the focused one.
/// @return (table|nil, string|nil) Snapshot table, or nil and an error.
/// @example
/// local s = maki.session.read()
/// if s.context_size > s.context_window * 0.8 then
///   maki.notify("context is nearly full")
/// end
#[lua_fn]
async fn read(lua: Lua, #[ctx] ops: FacadeOps, opts: Option<Table>) -> LuaResult<Pair<Value>> {
    let id = match opts {
        Some(t) => t.opt::<String>("session")?,
        None => None,
    };
    // A headless driver installs a provider for the one session it runs. The
    // UI leaves the slot empty and answers from its event loop, which owns
    // the live runtimes. The slot is asked in its own statement, so its
    // borrow ends before anything below parks the task.
    let headless = ops.headless_snapshot(&lua, id.as_deref());
    // An id no host answers for may still be another driver's, or stored.
    let reply = match (headless, id) {
        (Some(Err(_)), Some(id)) => unhosted_snapshot(&lua, &ops, &id).await,
        (Some(reply), _) => reply,
        (None, id) => {
            let asked = ops
                .ask(FacadeRequest::Session(SessionRequest::Read {
                    id: id.clone(),
                }))
                .await;
            match (asked, id) {
                (Ok(reply), _) => reply,
                (Err(NO_UI_ERR), Some(id)) => unhosted_snapshot(&lua, &ops, &id).await,
                (Err(e), _) => Err(e.to_owned()),
            }
        }
    };
    let snapshot = try_pair!(reply);
    Ok((Some(json_to_lua(&lua, &snapshot)?), None))
}

/// The focused tab, or under a headless driver the one session it runs.
async fn focused_session(lua: &Lua, ops: &FacadeOps) -> Result<String, String> {
    // Asked in its own statement, so the borrow of the app data ends before
    // the roundtrip below parks the task.
    let headless = ops.headless_snapshot(lua, None);
    let snapshot = match headless {
        Some(snapshot) => snapshot?,
        None => {
            ops.ask(FacadeRequest::Session(SessionRequest::Current))
                .await??
        }
    };
    // The UI answers with the bare id, a headless driver with its snapshot.
    snapshot
        .get("id")
        .unwrap_or(&snapshot)
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| NO_FOCUSED_ERR.to_owned())
}

/// Unparsed where only a UI answers: a remote tab's id is its agent's, which
/// only the UI resolves. A runtime with a local agent names its sessions by
/// `MakiId`, so a malformed id still fails before anything is asked.
async fn named_or_focused(
    lua: &Lua,
    ops: &FacadeOps,
    id: Option<String>,
) -> Result<String, String> {
    match id {
        Some(id) if matches!(ops, FacadeOps::Agent(_)) => {
            id.parse::<MakiId>().map_err(|e| e.to_string())?;
            Ok(id)
        }
        Some(id) => Ok(id),
        None => focused_session(lua, ops).await,
    }
}

/// A live session from memory, unsaved messages included, any other from
/// disk. The UI answers for its tabs. A headless driver publishes its
/// session's settings next to the transcript, so a session it has not saved
/// yet reads too.
async fn transcript(lua: &Lua, ops: &FacadeOps, raw: &str) -> Result<SessionExport, String> {
    match ops.export(raw).await {
        Ok(reply) => return reply,
        Err(e) if e != NO_UI_ERR => return Err(e.to_owned()),
        Err(_) => {}
    }
    let id = raw.parse::<MakiId>().map_err(|e| e.to_string())?;
    if let Some(snapshot) = ops.live_snapshot(id)
        && let Some(header) = ops.live_header(id)
    {
        return Ok(SessionExport::live(Arc::unwrap_or_clone(header), &snapshot));
    }
    from_disk(lua, ops, id, |stored| Ok(SessionExport::stored(stored))).await
}

/// The serde shape, nulls as `maki.NIL`. An image block is built around a
/// handle on its payload, which is never copied, not even into JSON.
fn message_table(lua: &Lua, message: &Message) -> LuaResult<Value> {
    let envelope = Message {
        role: message.role.clone(),
        content: Vec::new(),
        display_text: message.display_text.clone(),
        kind: message.kind.clone(),
        receipts: message.receipts.clone(),
        settles: message.settles.clone(),
    };
    let json = serde_json::to_value(&envelope).map_err(mlua::Error::external)?;
    let table = lua.unpack::<Table>(json_to_lua_exact(lua, &json)?)?;
    let content = lua.create_table_with_capacity(message.content.len(), 0)?;
    for (index, block) in message.content.iter().enumerate() {
        content.raw_set(index + 1, block_table(lua, block)?)?;
    }
    content.set_metatable(Some(lua.array_metatable()))?;
    table.set(CONTENT_KEY, content)?;
    Ok(Value::Table(table))
}

fn block_table(lua: &Lua, block: &ContentBlock) -> LuaResult<Value> {
    let ContentBlock::Image { source } = block else {
        let json = serde_json::to_value(block).map_err(mlua::Error::external)?;
        return json_to_lua_exact(lua, &json);
    };
    let table = lua.create_table_with_capacity(0, 2)?;
    table.set(TYPE_KEY, BLOCK_IMAGE)?;
    table.set(
        SOURCE_KEY,
        lua.create_userdata(LuaImage::from_source(source.clone()))?,
    )?;
    Ok(Value::Table(table))
}

/// Tagged as an array, so an empty history still encodes as `[]`.
async fn messages_table(lua: &Lua, messages: &[Message]) -> LuaResult<Table> {
    let out = lua.create_table_with_capacity(messages.len(), 0)?;
    for (index, message) in messages.iter().enumerate() {
        if index > 0 && index % MESSAGES_PER_YIELD == 0 {
            tokio::task::yield_now().await;
        }
        out.raw_set(index + 1, message_table(lua, message)?)?;
    }
    out.set_metatable(Some(lua.array_metatable()))?;
    Ok(out)
}

async fn transcript_table(lua: &Lua, export: SessionExport) -> LuaResult<Table> {
    let envelope = serde_json::to_value(export.envelope()).map_err(mlua::Error::external)?;
    let table = lua.unpack::<Table>(json_to_lua_exact(lua, &envelope)?)?;
    table.set(MESSAGES_KEY, messages_table(lua, &export.messages).await?)?;
    Ok(table)
}

/// Reads a session as the document `maki session export` prints, the same
/// table `maki.session.new` takes back:
///
/// ```text
/// { format, model, mode, thinking, fast, title, facts, messages }
/// ```
///
/// `messages` is what `maki.session.messages` returns. `facts` is the date,
/// working directory, model, instructions and hints the model was last told,
/// nil when the session never ran. A live session is read from memory,
/// unsaved messages included, any other from disk without claiming it, so a
/// session another maki has open can be read too.
///
/// JSON null comes back as `maki.NIL` and arrays are tagged, so
/// `maki.session.export(...)` encodes to the same document as the CLI file.
/// Copy it with `maki.deepcopy` before editing, which keeps both intact.
///
/// @param opts table? `session` (string?) Session id, live or stored. Defaults to the focused one.
/// @return (table|nil, string|nil) The document, or nil and an error.
/// @example
/// local doc = assert(maki.session.export({ session = maki.session.current() }))
/// maki.fs.write("/tmp/session.json", maki.json.encode(doc))
#[lua_fn(guard = SessionRead)]
async fn export(lua: Lua, #[ctx] ops: FacadeOps, opts: Option<Table>) -> LuaResult<Pair<Table>> {
    let id = match &opts {
        Some(opts) => opts.opt::<String>("session")?,
        None => None,
    };
    let session = try_pair!(named_or_focused(&lua, &ops, id).await);
    let transcript = try_pair!(transcript(&lua, &ops, &session).await);
    Ok((Some(transcript_table(&lua, transcript).await?), None))
}

/// Reads a session's transcript, oldest first: everything the model has
/// been sent so far, tool calls and results included. Read only.
///
/// Each message has the same shape as in the session file:
///
/// ```text
/// { role = "user" | "assistant", kind, facts?, display_text?, receipts?, settles?, content = { blocks } }
/// ```
///
/// `kind` is `"turn"` for something the user or the model said,
/// `"observation"` for a report sent to the model as a user message, like
/// `maki.session.notify`, and `"context_update"` for a change since the
/// system prompt was built (date, model, plan mode, ...), which carries the
/// changed `facts`. `display_text` is what the UI shows instead of the
/// content: nil shows the content, `""` hides the message (a nudge, a
/// compaction note). `receipts` lists the calls whose results in this
/// message moved to the background, and `settles` = `{ task, end }` marks the
/// notice that answered one, with `end` one of `"finished"`, `"failed"`,
/// `"cancelled"` or `"lost"`. `content` lists blocks:
///
/// ```text
/// { type = "text", text }
/// { type = "thinking", thinking, signature? }
/// { type = "redacted_thinking", data }
/// { type = "tool_use", id, name, input, thought_signature? }
/// { type = "tool_result", tool_use_id, content, is_error?, loaded_tools? }
/// { type = "image", source = maki.image.Image }
/// ```
///
/// `signature`, `thought_signature`, `data` and `loaded_tools` are opaque:
/// pass them through untouched. A block type this build does not know comes
/// as its raw table. JSON null is `maki.NIL`.
///
/// A live session is read from memory, unsaved messages included, any other
/// from disk. Works in the TUI, `maki -p`, sdk mode, and ACP. ACP has no
/// focused session, so pass `session` there. Hook and event payloads carry
/// the `session_id` to pass.
///
/// @param opts table? Options:
///   `session` (string?) Session id, live or stored. Defaults to the focused one.
///   `last` (integer?) only the newest `last` messages.
/// @return (table|nil, string|nil) Array of messages, or nil and an error.
/// @example
/// local msgs = maki.session.messages({ last = 1 })
/// local last = msgs and msgs[1]
/// if last and last.role == "assistant" and last.content[1].type == "text" then
///   print(last.content[1].text)
/// end
#[lua_fn(guard = SessionRead)]
async fn messages(lua: Lua, #[ctx] ops: FacadeOps, opts: Option<Table>) -> LuaResult<Pair<Table>> {
    let (id, last) = match &opts {
        Some(opts) => (opts.opt::<String>("session")?, opts.opt::<usize>("last")?),
        None => (None, None),
    };
    let session = try_pair!(named_or_focused(&lua, &ops, id).await);
    let live = session.parse().ok().and_then(|id| ops.live_history(id));
    let history = match live {
        Some(history) => history,
        None => try_pair!(transcript(&lua, &ops, &session).await).messages,
    };
    let skip = last.map_or(0, |n| history.len().saturating_sub(n));
    Ok((Some(messages_table(&lua, &history[skip..]).await?), None))
}

/// Returns the id of the focused session. Under `maki -p` and the sdk,
/// that is the one session they run.
///
/// @return (string|nil, string|nil) Session id, or nil and an error.
/// @example
/// local id = maki.session.current()
#[lua_fn]
async fn current(lua: Lua, #[ctx] ops: FacadeOps) -> LuaResult<Pair<String>> {
    Ok(pair(focused_session(&lua, &ops).await))
}

/// Switches the UI to the session with {id}.
///
/// @param id string Session id, as returned by `list()` or `live()`.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// local _, err = maki.session.focus(id)
#[lua_fn]
async fn focus(lua: Lua, #[ctx] ops: FacadeOps, id: String) -> LuaResult<Pair<Value>> {
    roundtrip(lua, ops, SessionRequest::Focus { id }).await
}

/// Deletes a session and its stored history, cancelling it first if it
/// is running. The focused session cannot be deleted.
///
/// @param id string Session id to delete.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// local _, err = maki.session.delete(id)
#[lua_fn(guard = SessionWrite)]
async fn delete(lua: Lua, #[ctx] ops: FacadeOps, id: String) -> LuaResult<Pair<Value>> {
    roundtrip(lua, ops, SessionRequest::Delete { id }).await
}

fn is_untagged_empty(lua: &Lua, table: &Table) -> bool {
    table.is_empty() && table.metatable().as_ref() != Some(&lua.array_metatable())
}

/// An untagged `{}` cannot say whether it was a list, and guessing one
/// wrong reaches the model, so it is refused with its path.
async fn messages_json(lua: &Lua, messages: &Table) -> LuaResult<Result<JsonValue, String>> {
    if is_untagged_empty(lua, messages) {
        return Ok(Err(format!("{MESSAGES_KEY}: {EMPTY_ARRAY_ERR}")));
    }
    let len = messages.raw_len();
    // Not a list. The conversion keeps every key, and the transcript parser
    // names what is wrong with it.
    if messages.pairs::<Value, Value>().count() != len {
        return Ok(Ok(lua_to_json(lua, &Value::Table(messages.clone()))?));
    }
    let mut out = Vec::with_capacity(len);
    for index in 0..len {
        if index > 0 && index % MESSAGES_PER_YIELD == 0 {
            tokio::task::yield_now().await;
        }
        let message: Value = messages.raw_get(index + 1)?;
        if let Value::Table(fields) = &message
            && let Value::Table(content) = fields.get::<Value>(CONTENT_KEY)?
            && is_untagged_empty(lua, &content)
        {
            return Ok(Err(format!(
                "{MESSAGES_KEY}[{index}].{CONTENT_KEY}: {EMPTY_ARRAY_ERR}"
            )));
        }
        out.push(lua_to_json(lua, &message)?);
    }
    Ok(Ok(JsonValue::Array(out)))
}

/// Exactly the export document's fields. The run options stay behind.
async fn document_json(lua: &Lua, opts: &Table) -> LuaResult<Result<JsonValue, String>> {
    let mut doc = Map::new();
    for key in DOCUMENT_KEYS {
        let value: Value = opts.get(key)?;
        let json = match &value {
            Value::Nil => continue,
            Value::Table(messages) if key == MESSAGES_KEY => {
                match messages_json(lua, messages).await? {
                    Ok(json) => json,
                    Err(e) => return Ok(Err(e)),
                }
            }
            _ => lua_to_json(lua, &value)?,
        };
        doc.insert(key.to_owned(), json);
    }
    Ok(Ok(JsonValue::Object(doc)))
}

/// Where a session lands without a UI: the project of the one session a
/// single-session driver (`maki -p`, sdk) runs, so the project list and
/// `maki -r` find it there, else where maki runs. ACP holds many sessions, so
/// it gets the process's directory too, never one of its sessions picked by
/// how many happen to be open.
fn headless_cwd(lua: &Lua, ops: &FacadeOps) -> Result<String, String> {
    let driver = ops
        .headless_snapshot(lua, None)
        .and_then(Result::ok)
        .and_then(|snapshot| snapshot.get(CWD_KEY)?.as_str().map(str::to_owned));
    if let Some(cwd) = driver {
        return Ok(cwd);
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    Ok(cwd.to_string_lossy().into_owned())
}

/// Creates a session from the document `maki.session.export` returns, or
/// from nothing.
///
/// Whatever the document leaves out is what a new tab would get: in the TUI
/// the focused tab's project, model, mode, thinking, fast and workflow, under
/// `maki -p` and sdk mode the project of the session they run, under ACP the
/// directory maki runs in. Outside the TUI no model is stored, so the session
/// opens on the default model. A document with messages gets no permissions
/// (`yolo`, session rules) from either side. Without messages it is a new tab
/// like `Ctrl-N`, which keeps the focused tab's `yolo`.
///
/// Without `prompt` or `focus` the session is only stored, and
/// `maki.session.focus(id)` opens it later. With either it opens in a tab.
/// A session is only stored once it has messages, so storing a document
/// with none left after repair fails. Pass `focus` to open a blank one
/// instead.
/// Headless drivers (`maki -p`, ACP) have nothing to open it in, so there
/// `prompt` and `focus` answer `nil, "no interactive UI attached"` and store
/// nothing.
///
/// History is repaired the way a resume repairs it (orphan tool results go,
/// a dangling final tool call is closed) and anything else wrong is refused
/// with its location, like `messages[12].content[2]: ...` (0-based, as in
/// the JSON document). A block of a known type that does not parse, or one
/// without a `type`, is refused too, while one of a type this build does not
/// know is kept. Thinking blocks and `thought_signature` are dropped: they
/// are bound to a system prompt the new session will not have. `model` is
/// stored as given and resolved when the session opens, falling back with a
/// warning like any resume.
///
/// Image sources may be a `maki.image.Image` or the
/// `{ type = "base64", media_type, data }` table `maki.json` gives. An empty
/// table where a list belongs (`messages`, a message's `content`) is refused,
/// since nothing tells it from an empty object: copy with `maki.deepcopy`,
/// which keeps the tag. Fields `new` does not know are ignored.
///
/// @param opts table? Optional fields:
///   `messages` (table) what `export` or `messages` return. A user turn
///     that starts with `/` or `!`, or is `exit`, needs the `agent`
///     permission too, since a rewind puts it back in the input for Enter
///     to run.
///   `facts` (table) what `export` returns: the date, working directory,
///     model, instructions and hints the model was last told.
///   `model` (string) model spec. Defaults to the focused tab's in the TUI,
///     else the default model when the session opens.
///   `mode` (string) "build" or "plan".
///   `thinking` (string) spelled like the thinking setting.
///   `fast` (boolean) fast output mode, where the model supports it.
///   `title` (string) else generated from the history.
///   `prompt` (string) first user message to submit right away. Needs the
///     `agent` permission.
///   `focus` (boolean) switch the UI to the new session.
/// @return (string|nil, string|nil) New session id, or nil and an error.
/// @example
/// -- copy a session, keeping it in the background
/// local doc = assert(maki.session.export({ session = id }))
/// local copy, err = maki.session.new(doc)
///
/// -- start a fresh one right away
/// local id, err = maki.session.new({ prompt = "fix the tests", focus = true })
#[lua_fn(guard = SessionWrite)]
async fn new(
    lua: Lua,
    #[ctx] ops: FacadeOps,
    #[ctx] grants: PluginPermissions,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let opts = match opts {
        Some(opts) => opts,
        None => lua.create_table()?,
    };
    let prompt: Option<String> = opts.opt(PROMPT_KEY)?;
    let focus = opts.opt_bool(FOCUS_KEY).unwrap_or(false);
    if prompt.is_some() {
        require(&lua, &grants, Permission::Agent)?;
    }
    let opens = prompt.is_some() || focus;
    let has_ui = matches!(ops, FacadeOps::Ui(_));
    if opens && !has_ui {
        return Ok(err_pair(NO_UI_ERR));
    }
    let doc = try_pair!(document_json(&lua, &opts).await?);
    // Off the Lua thread, since parsing a long history is real work.
    let parsed = spawn_blocking(move || serde_json::from_value::<Transcript>(doc))
        .await
        .map_err(mlua::Error::external)?;
    let transcript = try_pair!(parsed.map_err(|e| e.to_string()));
    if transcript.messages.iter().any(runs_on_enter) {
        require(&lua, &grants, Permission::Agent)?;
    }
    if has_ui {
        let req = SessionRequest::New {
            transcript: Box::new(transcript),
            prompt,
            focus,
        };
        return roundtrip(lua, ops, req).await;
    }
    let local = Arc::clone(try_pair!(ops.local()));
    let base = StoredSession::new("", &try_pair!(headless_cwd(&lua, &ops)));
    let dir = try_pair!(state_dir(&lua));
    let id = try_pair!(
        spawn_blocking(move || local.store_new(transcript, base, &dir))
            .await
            .map_err(mlua::Error::external)?
    );
    Ok((Some(id.to_string().into_lua(&lua)?), None))
}

/// Sends {text} as a regular user prompt to a live session. The text is
/// never interpreted: slash commands, `exit`, and `!` shell prefixes are
/// all sent to the model verbatim. If the session is currently streaming,
/// the prompt is queued and picked up when the agent reaches it.
///
/// @param text string The prompt to send. Must not be blank.
/// @param opts table? Optional fields: session (string) id of a live
///   session; defaults to the focused one.
/// @return (string|nil, string|nil) "started" or "queued", or nil and an error.
/// @example
/// local state, err = maki.session.prompt("run the tests", { session = id })
#[lua_fn(guard = [Agent, SessionWrite])]
async fn prompt(
    lua: Lua,
    #[ctx] ops: FacadeOps,
    text: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let id = match opts {
        Some(opts) => opts.opt("session")?,
        None => None,
    };
    roundtrip(lua, ops, SessionRequest::Prompt { id, text }).await
}

/// Reports {text} to a live session without creating a user turn. The
/// observation waits for the session's next agent run.
///
/// @param text string What to report. Must not be blank.
/// @param opts table Options:
///   `session` (string) id of a live session.
///   `wake` (boolean) start a TUI turn when it next becomes idle (default
///     false). Needs the `agent` permission.
///   `display` (string?) one-line note for the transcript, shown and saved
///     when the session's agent run accepts the report. Whitespace collapses
///     to single spaces and the note is cut to 256 bytes. Defaults to
///     nothing; the model reads only {text}.
/// @return (boolean|nil, string|nil) true, or nil and an error.
/// @example
/// maki.session.notify("[monitor] deploy failed", { session = id, wake = true })
#[lua_fn(guard = SessionWrite)]
fn notify(
    lua: &Lua,
    #[ctx] ops: FacadeOps,
    #[ctx] grants: PluginPermissions,
    text: String,
    opts: Option<Table>,
) -> LuaResult<Pair<bool>> {
    let wake = opts
        .as_ref()
        .and_then(|t| t.opt_bool("wake"))
        .unwrap_or(false);
    if wake {
        require(lua, &grants, Permission::Agent)?;
    }
    if text.trim().is_empty() {
        return Ok(err_pair(BLANK_NOTIFY_ERR));
    }
    let session_id = try_pair!(required_session(opts.as_ref())?);
    let display = opts
        .as_ref()
        .map(|t| t.opt::<String>("display"))
        .transpose()?
        .flatten();
    let local = try_pair!(ops.local());
    if let Err(error) = local.notify(session_id, text, display.as_deref(), wake) {
        return Ok(err_pair(error));
    }
    Ok((Some(true), None))
}

/// Switches a live session between plan and build mode. Entering plan mode
/// allocates the session's plan file if it has none.
///
/// A session that is mid-plan answers the next prompt with another draft of
/// the plan. Set `"build"` first and that prompt implements it.
///
/// @param mode string "build" or "plan".
/// @param opts table? Options:
///   session (string) id of a live session, defaults to the focused one.
/// @return (boolean|nil, string|nil) true, or nil and an error.
/// @example
/// maki.session.set_mode("build", { session = opts.session })
/// maki.session.prompt("Implement the plan at `" .. opts.path .. "`.", { session = opts.session })
#[lua_fn(guard = SessionSettings)]
async fn set_mode(
    lua: Lua,
    #[ctx] ops: FacadeOps,
    mode: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let id = match opts {
        Some(opts) => opts.opt("session")?,
        None => None,
    };
    roundtrip(lua, ops, SessionRequest::SetMode { id, mode }).await
}

/// Renames a session, live or stored.
///
/// @param opts table Required fields: id (string) session to rename;
///   title (string) the new title.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// local _, err = maki.session.set_title({ id = id, title = "refactor" })
#[lua_fn(guard = SessionSettings)]
async fn set_title(lua: Lua, #[ctx] ops: FacadeOps, opts: Table) -> LuaResult<Pair<Value>> {
    let req = SessionRequest::SetTitle {
        id: opts.get::<String>("id")?,
        title: opts.get::<String>("title")?,
    };
    roundtrip(lua, ops, req).await
}

lua_table! {
    /// Host session primitives. The interactive UI can run several sessions
    /// at once. These functions list, create, focus, rename, and delete them.
    ///
    /// Without a UI, most functions return `nil, "no interactive UI attached"`.
    /// `current`, `read`, `export`, `messages`, `notify` and `new` without
    /// `prompt` or `focus` still work. ACP has no focused session, so there
    /// `current` fails and `read`, `export` and `messages` need `session`.
    ///
    /// Under `maki connect` the connected agent answers. `list` returns its
    /// sessions for the workspace, `live` and `current` the connected one,
    /// and `read`, `prompt`, `set_mode` and `focus` take only that session.
    /// The other functions return an error.
    "maki.session" => pub fn create_session_table(
        ops: FacadeOps,
        perms: &PluginPermissions,
        grants: PluginPermissions,
    ),
    DOCS [
        list(ops), live(ops), current(ops), read(ops), export(perms, ops), messages(perms, ops),
        focus(ops), delete(perms, ops), new(perms, ops, grants), prompt(perms, ops),
        notify(perms, ops, grants), set_mode(perms, ops), set_title(perms, ops),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_types::message::{TaskEnd, TaskSettled};

    #[test]
    fn lua_message_export_preserves_background_receipts_and_settlements() {
        let lua = Lua::new();
        let message = Message {
            receipts: vec!["background-call".into()],
            settles: Some(TaskSettled {
                task: "background-call".into(),
                end: TaskEnd::Lost,
            }),
            ..Message::default()
        };
        let table = message_table(&lua, &message).unwrap();
        assert_eq!(
            lua_to_json(&lua, &table).unwrap(),
            serde_json::to_value(message).unwrap()
        );
    }
}
