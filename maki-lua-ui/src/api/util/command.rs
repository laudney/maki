use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use arc_swap::ArcSwap;
pub use maki_lua_core::facade::{
    ModelRequest, NO_UI_ERR, PlanRequest, SessionRequest, TaskRequest, UiReply,
};
pub(crate) use maki_lua_core::facade::{PLUGIN_LOADING_ERR, UI_DROPPED_ERR};
use maki_storage::id::MakiId;
use maki_types::snapshot::SharedBuf;
use maki_types::transcript::SessionExport;
use mlua::{Lua, RegistryKey, Result as LuaResult, Value};
use strum::{EnumString, VariantNames};

use crate::api::ui::marks::Mark;
use crate::api::util::convert::json_to_lua;
use crate::api::util::pair::{Pair, try_pair};
use crate::host_sink::HostSink;
use crate::key::Key;
use crate::plugin_permissions::{Permission, PluginReach};
use crate::runtime::in_plugin_load;

/// Text in the chat input is what the user's Enter sends.
const EDITS_TEXT: &[Permission] = &[
    Permission::Input,
    Permission::Agent,
    Permission::SessionWrite,
];
const ROW_REFINE: &str = "refine";
const ROW_CLEAR_AND_IMPLEMENT: &str = "clear_and_implement";
const ROW_IMPLEMENT: &str = "implement";

#[derive(Clone)]
pub struct LuaCommandInfo {
    pub name: Arc<str>,
    pub description: Arc<str>,
    pub plugin: Arc<str>,
    /// The handler runs with this, so a plugin calling it through
    /// `maki.api.run_command` must reach as far. Otherwise it could borrow
    /// this plugin's reach and pick the arguments.
    pub reach: PluginReach,
    pub max_args: usize,
}

#[derive(Clone, Default)]
pub struct LuaCommandSnapshot {
    pub commands: Vec<LuaCommandInfo>,
    pub generation: u64,
}

#[derive(Clone)]
pub struct LuaCommandReader(Arc<ArcSwap<LuaCommandSnapshot>>);

impl LuaCommandReader {
    pub fn empty() -> Self {
        Self(Arc::new(ArcSwap::from_pointee(
            LuaCommandSnapshot::default(),
        )))
    }

    pub fn from_commands(commands: Vec<LuaCommandInfo>) -> Self {
        Self(Arc::new(ArcSwap::from_pointee(LuaCommandSnapshot {
            commands,
            generation: 1,
        })))
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<LuaCommandSnapshot>> {
        self.0.load()
    }
}

pub struct LuaCommandWriter {
    store: Arc<ArcSwap<LuaCommandSnapshot>>,
    generation: AtomicU64,
}

impl LuaCommandWriter {
    pub fn new() -> (Self, LuaCommandReader) {
        let inner = Arc::new(ArcSwap::from_pointee(LuaCommandSnapshot::default()));
        (
            Self {
                store: Arc::clone(&inner),
                generation: AtomicU64::new(0),
            },
            LuaCommandReader(inner),
        )
    }

    pub fn publish(&self, commands: Vec<LuaCommandInfo>) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.store.store(Arc::new(LuaCommandSnapshot {
            commands,
            generation,
        }));
    }
}

/// An opaque ACP session name or the client's local tab id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TabRef(pub Arc<str>);

impl From<MakiId> for TabRef {
    fn from(id: MakiId) -> Self {
        Self(Arc::from(id.to_string()))
    }
}

impl TabRef {
    /// Resolves the agent's name and the local tab alias the same way.
    pub fn names(&self, local: MakiId, remote: Option<&str>) -> bool {
        remote == Some(&*self.0) || self.0.parse::<MakiId>().is_ok_and(|id| id == local)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HintEntry {
    pub plugin: Arc<str>,
    /// `None` shows in every tab.
    pub session: Option<TabRef>,
    pub spans: Vec<(String, String)>,
}

pub type HintEntries = Vec<HintEntry>;

#[derive(Clone, Default)]
pub struct HintSnapshot {
    pub entries: HintEntries,
    pub generation: u64,
}

impl HintSnapshot {
    /// A plugin's hint for this session takes the place of its every-tab
    /// hint here. A plugin often sets both, one from a keymap and one from a
    /// tool call, and the tab would show it twice otherwise.
    /// The tab is named as in [`TabRef::names`].
    pub fn visible_in<'a>(
        &'a self,
        local: MakiId,
        remote: Option<&'a str>,
    ) -> impl Iterator<Item = &'a HintEntry> {
        let owns = move |entry: &HintEntry| {
            entry
                .session
                .as_ref()
                .is_some_and(|owner| owner.names(local, remote))
        };
        self.entries
            .iter()
            .filter(move |entry| match entry.session {
                Some(_) => owns(entry),
                None => !self
                    .entries
                    .iter()
                    .any(|other| other.plugin == entry.plugin && owns(other)),
            })
    }
}

#[derive(Clone)]
pub struct HintReader(Arc<ArcSwap<HintSnapshot>>);

impl HintReader {
    pub fn empty() -> Self {
        Self(Arc::new(ArcSwap::from_pointee(HintSnapshot::default())))
    }

    /// Full `Arc` rather than a `Guard`: the UI keeps the last one alive to
    /// compare identity against the next, which is how it notices a publish.
    pub fn load_full(&self) -> Arc<HintSnapshot> {
        self.0.load_full()
    }
}

/// Publishing end of a plugin's status hints, owned by the Lua thread.
pub struct HintWriter {
    store: Arc<ArcSwap<HintSnapshot>>,
    generation: AtomicU64,
}

impl HintWriter {
    pub fn new() -> (Self, HintReader) {
        let inner = Arc::new(ArcSwap::from_pointee(HintSnapshot::default()));
        (
            Self {
                store: Arc::clone(&inner),
                generation: AtomicU64::new(0),
            },
            HintReader(inner),
        )
    }

    pub fn publish(&self, entries: HintEntries) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.store.store(Arc::new(HintSnapshot {
            entries,
            generation,
        }));
    }
}

pub(crate) struct CommandEntry {
    pub handler: RegistryKey,
    pub description: Arc<str>,
    pub reach: PluginReach,
    pub max_args: usize,
}

pub(crate) type CommandHandlerMap = HashMap<Arc<str>, HashMap<Arc<str>, CommandEntry>>;

pub(crate) fn publish_command_snapshot(map: &CommandHandlerMap, writer: &LuaCommandWriter) {
    let commands = map
        .iter()
        .flat_map(|(plugin, cmds)| {
            cmds.iter().map(move |(name, entry)| LuaCommandInfo {
                name: Arc::clone(name),
                description: Arc::clone(&entry.description),
                plugin: Arc::clone(plugin),
                reach: entry.reach.clone(),
                max_args: entry.max_args,
            })
        })
        .collect();
    writer.publish(commands);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Abs(u16),
    Percent(u16),
}

impl Dimension {
    pub fn resolve(self, total: u16) -> u16 {
        match self {
            Self::Abs(n) => n,
            Self::Percent(p) => (total as u32 * p as u32 / 100) as u16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    #[default]
    NW,
    NE,
    SW,
    SE,
    /// The cell the chat input caret is drawn in. Named after the widget,
    /// because a window can hold a caret of its own.
    InputCaret,
}

impl Anchor {
    pub fn parse(s: &str) -> Self {
        match s {
            "NE" => Self::NE,
            "SW" => Self::SW,
            "SE" => Self::SE,
            "input_caret" => Self::InputCaret,
            _ => Self::NW,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Border {
    None,
    Single,
    Double,
    #[default]
    Rounded,
}

impl Border {
    pub fn parse(s: &str) -> Self {
        match s {
            "none" => Self::None,
            "single" => Self::Single,
            "double" => Self::Double,
            _ => Self::Rounded,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Split {
    #[default]
    None,
    Above,
    Below,
    Left,
    Right,
    Panel,
}

/// The renderer and layout read `Axis` rather than matching on `Split`, so a
/// new direction never needs a new match arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Reserves rows: a band on the top or bottom edge.
    Vertical,
    /// Reserves columns: a band on the left or right edge.
    Horizontal,
}

/// `at_start` means the top or left edge, otherwise the bottom or right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub axis: Axis,
    pub at_start: bool,
}

impl Split {
    pub fn parse(s: &str) -> Self {
        match s {
            "above" => Self::Above,
            "below" => Self::Below,
            "left" => Self::Left,
            "right" => Self::Right,
            "panel" => Self::Panel,
            _ => Self::None,
        }
    }

    /// The one place a direction turns into geometry, so the mapping lives in a
    /// single spot. `None` means the split is off.
    pub fn edge(self) -> Option<Edge> {
        Some(match self {
            Self::None | Self::Panel => return None,
            Self::Above => Edge {
                axis: Axis::Vertical,
                at_start: true,
            },
            Self::Below => Edge {
                axis: Axis::Vertical,
                at_start: false,
            },
            Self::Left => Edge {
                axis: Axis::Horizontal,
                at_start: true,
            },
            Self::Right => Edge {
                axis: Axis::Horizontal,
                at_start: false,
            },
        })
    }

    /// Every real direction, in carve order: columns before rows. Callers loop
    /// over this so they stay exhaustive without a match. It is hand-written, so
    /// `all_lists_exactly_the_edge_bearing_variants` keeps it honest.
    pub const ALL: [Split; 4] = [Split::Left, Split::Right, Split::Above, Split::Below];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TitlePos {
    #[default]
    Left,
    Center,
    Right,
}

impl TitlePos {
    pub fn parse(s: &str) -> Self {
        match s {
            "center" => Self::Center,
            "right" => Self::Right,
            _ => Self::Left,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloatConfig {
    pub width: Dimension,
    pub height: Dimension,
    pub row: Option<i16>,
    pub col: Option<i16>,
    pub anchor: Anchor,
    pub border: Border,
    pub title: String,
    pub title_pos: TitlePos,
    pub footer: Vec<(String, String)>,
    pub zindex: u16,
    pub cursor_line: bool,
    pub reserved_bottom: usize,
    pub reserved_top: usize,
    pub split: Split,
    pub order: u16,
    pub visible: bool,
    pub needs_input: bool,
    /// Opt in to corner stacking: the UI offsets this window past the other
    /// stacked windows sharing its anchor. Open time only, so no patch field.
    pub stack: bool,
    /// The keys this window takes while it is on screen without being
    /// focused, parsed from the same notation `maki.keymap.set` reads. A
    /// focused window is handed every key and declares none. Open time only,
    /// so no patch field: the list the user sees in the footer is the list
    /// the window opened with, and it dies with the window.
    pub keys: Vec<Key>,
}

impl Default for FloatConfig {
    fn default() -> Self {
        Self {
            width: Dimension::Percent(60),
            height: Dimension::Percent(70),
            row: None,
            col: None,
            anchor: Anchor::default(),
            border: Border::default(),
            title: String::new(),
            title_pos: TitlePos::default(),
            footer: Vec::new(),
            zindex: 50,
            cursor_line: false,
            reserved_bottom: 0,
            reserved_top: 0,
            split: Split::None,
            order: 50,
            visible: true,
            needs_input: false,
            stack: false,
            keys: Vec::new(),
        }
    }
}

macro_rules! apply_opt {
    ($self:ident, $patch:ident, $($field:ident),+ $(,)?) => {
        $(if let Some(v) = $patch.$field { $self.$field = v; })+
    };
}

impl FloatConfig {
    pub fn apply_patch(&mut self, patch: FloatConfigPatch) {
        apply_opt!(
            self,
            patch,
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
            needs_input
        );
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FloatConfigPatch {
    pub width: Option<Dimension>,
    pub height: Option<Dimension>,
    pub row: Option<Option<i16>>,
    pub col: Option<Option<i16>>,
    pub anchor: Option<Anchor>,
    pub border: Option<Border>,
    pub title: Option<String>,
    pub title_pos: Option<TitlePos>,
    pub footer: Option<Vec<(String, String)>>,
    pub zindex: Option<u16>,
    pub cursor_line: Option<bool>,
    pub reserved_bottom: Option<usize>,
    pub reserved_top: Option<usize>,
    pub split: Option<Split>,
    pub order: Option<u16>,
    pub visible: Option<bool>,
    pub needs_input: Option<bool>,
}

pub enum WinEvent {
    Key { key: Key },
    Resize { width: u16, height: u16 },
    Paste { text: String },
    Close,
}

pub enum WinCommand {
    SetConfig(FloatConfigPatch),
    SetCursor(usize),
    SetVisible(bool),
    Close,
}

/// The built-in outcome a plan form row names, which only the host can run.
/// A row may carry one, a plugin handler, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanRowAction {
    Refine,
    ClearAndImplement,
    Implement,
}

impl PlanRowAction {
    /// The `action` tag a row carries in Lua.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Refine => ROW_REFINE,
            Self::ClearAndImplement => ROW_CLEAR_AND_IMPLEMENT,
            Self::Implement => ROW_IMPLEMENT,
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            ROW_REFINE => Some(Self::Refine),
            ROW_CLEAR_AND_IMPLEMENT => Some(Self::ClearAndImplement),
            ROW_IMPLEMENT => Some(Self::Implement),
            _ => None,
        }
    }
}

/// One row of the plan form menu. The host proposes its built-in rows and the
/// `ui.plan_form.actions` chain hands back the list the form draws.
///
/// `id` is what a layer targets a row by, so reordering or relabelling a row
/// never moves its handler. A row with a `plugin` runs that plugin's handler
/// first, and its `action` after, unless the handler said otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFormRow {
    pub id: String,
    pub label: String,
    pub desc: String,
    /// The host outcome the row falls through to, if any.
    pub action: Option<PlanRowAction>,
    /// The plugin whose handler is stashed for this row, `None` on a pure
    /// host row. Picking one names it, so its jobs and its log lines land on
    /// the plugin that wrote it.
    pub plugin: Option<Arc<str>>,
}

/// What became of a pick on a plan form row that carries a plugin handler.
/// A veto says nothing to the user, while a failure is the user pressing a
/// key and getting neither the handler nor the built-in outcome the row
/// promised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanActionOutcome {
    /// The handler ran, and the row's built-in action follows.
    Proceed,
    /// The handler ran and answered `false`, so the action is deliberately
    /// dropped.
    Vetoed,
    /// The handler failed, ran out of its window, or was never reached at
    /// all.
    Failed,
}
/// The menu one draft's chain answered with. The form echoes `generation`
/// back on a pick, so a chain that resumes late and installs its handlers
/// over a menu already on screen cannot have a pick routed to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanMenu {
    pub generation: u64,
    pub rows: Vec<PlanFormRow>,
}

/// Offsets are flat byte counts into the whole input, newlines counted as one
/// byte each, because Lua indexes strings by byte.
#[derive(Debug)]
pub struct TextEdit {
    pub start: usize,
    pub stop: usize,
    pub text: String,
}

impl TextEdit {
    pub fn changes_text(&self) -> bool {
        self.start != self.stop || !self.text.is_empty()
    }
}

pub struct InputEdit {
    /// All planned against the value at {version}, and applied together as
    /// one change.
    pub edits: Vec<TextEdit>,
    /// Where to leave the cursor, in the value the edits produce. The end of
    /// the text of the edit with the highest start when absent, or where the
    /// cursor is now when {edits} is empty.
    pub cursor: Option<usize>,
    /// Selects from here to the cursor, counted like {cursor}. Nothing stays
    /// selected when absent.
    pub anchor: Option<usize>,
    /// The value counter the reader was handed. Required, because an optional
    /// guard makes the shortest call the unguarded one.
    pub version: u64,
    /// Refuses any {version} but the current one instead of mapping the edits
    /// past what changed since.
    pub strict: bool,
    /// The tab the offsets were read from, refused once another is focused.
    /// Required for the same reason as {version}.
    pub session_id: String,
    /// What the edit may change, checked on the UI side, which alone knows
    /// the text the edits land on. Its owner rides along on the
    /// `InputChanged` this edit fires, so one input plugin can tell another's
    /// writes from its own, and owns {marks}.
    pub caller: PluginReach,
    /// Counted in the value the edits produce, like {cursor}, and placed in
    /// the same change as the text, so no frame shows one without the other.
    pub marks: Option<InputMarks>,
}

/// Everything a plugin's namespace {ns} holds from now on.
pub struct InputMarks {
    pub ns: String,
    pub marks: Vec<Mark>,
}

/// Marks planned against the value at {version} of the input of
/// {session_id}, which guard them like they guard an [`InputEdit`].
pub struct InputDecorate {
    pub version: u64,
    pub session_id: String,
    pub plugin: Arc<str>,
    pub marks: InputMarks,
}

/// Sends the input of {session_id} the way Enter does, only while it still
/// holds {version}.
pub struct InputSubmit {
    pub version: u64,
    pub session_id: String,
    /// What the send may reach, checked on the UI side, which alone knows
    /// what Enter would do with the text.
    pub caller: PluginReach,
}

pub enum InputRequest {
    Read,
    Edit(InputEdit),
    Decorate(InputDecorate),
    Submit(InputSubmit),
}

/// Viewport of the focused chat transcript, zero-based like the rest of the
/// UI; `maki.fn.winsaveview` is what puts it in Vim's 1-based shape.
/// `auto_scroll` is true while the transcript follows streaming output.
pub struct WinView {
    pub scroll_top: u32,
    pub line_count: u32,
    pub height: u16,
    pub auto_scroll: bool,
}

/// Lua sees these through `maki.ui.action` as the snake_case variant
/// names, so renaming a variant breaks user configs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, VariantNames)]
#[strum(serialize_all = "snake_case")]
pub enum BuiltinAction {
    FilePicker,
    Search,
    Help,
    PlanToggle,
    PlanEditor,
    EditInput,
    PopQueue,
    PrevChat,
    NextChat,
    ModelPicker,
    InputRaw,
    Undo,
    Redo,
    HistoryPrev,
    HistoryNext,
    Newline,
    /// Moves the main chat's movable running calls to the background.
    Background,
}

impl BuiltinAction {
    /// The built-ins that change the chat input. A plugin runs them only
    /// with `input`, like an edit, and the text they leave is the plugin's.
    /// Switching chats is not one of them: it shows another chat's own draft.
    pub fn changes_input(self) -> bool {
        self.needs().contains(&Permission::Input)
    }

    /// What a plugin needs to run this through `maki.ui.action`, the same
    /// price as the API that does the same thing. Matched without a fallback,
    /// so a new action has to be priced before it builds.
    ///
    /// Most actions open something the user then sees and drives, or move
    /// focus like `maki.task.focus`, so they are open. The plan form only
    /// shows, and the user decides there. Popping the queue throws away a
    /// prompt the user wrote, with nobody asked. An edit of the chat input
    /// needs what `maki.ui.input_edit` needs to change the text.
    pub fn needs(self) -> &'static [Permission] {
        match self {
            Self::FilePicker
            | Self::Search
            | Self::Help
            | Self::PlanToggle
            | Self::PlanEditor
            | Self::EditInput
            | Self::PrevChat
            | Self::NextChat
            | Self::ModelPicker
            | Self::InputRaw => &[],
            Self::PopQueue => &[Permission::SessionWrite],
            Self::Undo | Self::Redo | Self::HistoryPrev | Self::HistoryNext | Self::Newline => {
                EDITS_TEXT
            }
            // Each moved call's result wakes the chat and costs a turn, the
            // price of `notify{ wake = true }`.
            Self::Background => &[Permission::Agent],
        }
    }
}

pub enum UiAction {
    OpenWin {
        /// `None` opens it in the focused tab.
        session: Option<TabRef>,
        buf: Arc<SharedBuf>,
        config: FloatConfig,
        focus: bool,
        event_tx: flume::Sender<WinEvent>,
        cmd_rx: flume::Receiver<WinCommand>,
    },
    Flash(String),
    SetWindowTitle(String),
    OpenEditor {
        path: PathBuf,
        reply_tx: flume::Sender<i32>,
    },
    Session {
        req: SessionRequest,
        reply_tx: flume::Sender<UiReply>,
    },
    /// `maki.session.export` and `messages`: a live tab answers from memory,
    /// unsaved messages included, any other id from disk. Typed rather than
    /// JSON, with the messages shared, so neither the UI thread nor the Lua
    /// thread copies them.
    ExportSession {
        id: String,
        reply_tx: flume::Sender<Result<SessionExport, String>>,
    },
    Model {
        req: ModelRequest,
        reply_tx: flume::Sender<UiReply>,
    },
    Plan {
        req: PlanRequest,
        reply_tx: flume::Sender<UiReply>,
    },
    Input {
        req: InputRequest,
        reply_tx: flume::Sender<UiReply>,
    },
    Task {
        req: TaskRequest,
        reply_tx: flume::Sender<UiReply>,
    },
    WinSaveView {
        reply_tx: flume::Sender<WinView>,
    },
    WinRestView {
        scroll_top: u32,
    },
    /// `reply_tx` answers whether the action did anything, which is what lets
    /// a key handler try one action and fall back to another.
    Builtin {
        action: BuiltinAction,
        caller: PluginReach,
        reply_tx: flume::Sender<bool>,
    },
    /// The handler of the keystroke with this ticket id returned. It comes
    /// after everything the handler asked of the UI, so the keys typed after
    /// that keystroke can go.
    KeybindDone(u64),
    /// The handler of the keystroke with this ticket id called
    /// `ev.fallback()`: run what the host itself does for that key, before
    /// the keys typed after it.
    KeybindFallback(u64),
    /// The plugin of this name was unloaded, or a fresh load of it starts, so
    /// whatever it left in the UI has to go.
    PluginUnloaded(Arc<str>),
    /// `reply_tx` answers whether {cmdline} resolved to a known command, not
    /// how the command itself went: `/compact` streams for a minute, and the
    /// Lua caller must not park for that long.
    RunCommand {
        cmdline: String,
        depth: u8,
        /// Checked on the UI side, since only the UI knows what each command
        /// can reach.
        caller: PluginReach,
        reply_tx: flume::Sender<Result<(), String>>,
    },
}

/// Whether an event loop is draining `UiAction`. The channel cannot answer
/// that, because the runtime keeps a receiver of its own alive for the whole
/// process and `try_send` would go on succeeding long after the loop that
/// sends the replies is gone.
#[derive(Clone)]
pub struct UiAttachment(Arc<AtomicBool>);

impl Default for UiAttachment {
    /// Starts attached, so what a TUI host sends before its loop is up gets
    /// answered once it is. A headless host has no sender at all, so this bit
    /// only ever tells of a loop that went away.
    fn default() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }
}

impl UiAttachment {
    /// Called by every event loop generation, so the UI comes back after a
    /// `/reload` rebuilt the stack around the same handles.
    pub fn attach(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn detach(&self) {
        self.0.store(false, Ordering::Relaxed);
    }

    fn is_attached(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

// Lives on the Lua thread so `ui_send` can consult it without threading a
// handle through every `#[ctx]` sender.
thread_local! {
    static UI_ATTACHMENT: RefCell<Option<UiAttachment>> = const { RefCell::new(None) };
}

pub(crate) fn install_ui_attachment(attachment: UiAttachment) {
    UI_ATTACHMENT.with_borrow_mut(|slot| *slot = Some(attachment));
}

/// A thread with no handle installed (unit tests, embedders driving Lua
/// themselves) counts as attached: only a host that detached knows better.
fn ui_attached() -> bool {
    UI_ATTACHMENT.with_borrow(|slot| slot.as_ref().is_none_or(UiAttachment::is_attached))
}

/// Hand an action to the UI event loop. A detached UI, a full or closed
/// channel and a missing sender all mean the same thing: nobody is there to
/// run it.
pub(crate) fn ui_send(
    tx: Option<&flume::Sender<UiAction>>,
    action: UiAction,
) -> Result<(), &'static str> {
    if !ui_attached() {
        return Err(NO_UI_ERR);
    }
    tx.ok_or(NO_UI_ERR)?.try_send(action).map_err(|_| NO_UI_ERR)
}

/// The UI domain's [`HostSink`]: what the core tells the user becomes the
/// `UiAction` the event loop draws.
pub(crate) struct UiSink(pub(crate) Option<flume::Sender<UiAction>>);

impl HostSink for UiSink {
    fn flash(&self, text: String) -> bool {
        ui_send(self.0.as_ref(), UiAction::Flash(text)).is_ok()
    }

    /// No edit ever drops a point mark, so the ghost text of a plugin that is
    /// gone would stay for good unless the UI hears about it.
    fn plugin_unloaded(&self, plugin: &str) {
        let _ = ui_send(self.0.as_ref(), UiAction::PluginUnloaded(Arc::from(plugin)));
    }
}

/// Send an action carrying a reply channel and await the answer. Only
/// works from a callback: the loop drains `UiAction` between frames, so a
/// call from a plugin's top level fails, see
/// [`as_plugin_load`](crate::runtime::as_plugin_load).
///
/// No deadline on purpose: the loop legitimately stops answering for a long
/// time (`open_editor` holds it for as long as the editor is up). Teardown
/// detaches the UI instead, which fails the send outright.
pub(crate) async fn ui_roundtrip<T>(
    tx: Option<&flume::Sender<UiAction>>,
    action: impl FnOnce(flume::Sender<T>) -> UiAction,
) -> Result<T, &'static str> {
    if in_plugin_load() {
        return Err(PLUGIN_LOADING_ERR);
    }
    let (reply_tx, reply_rx) = flume::bounded(1);
    ui_send(tx, action(reply_tx))?;
    reply_rx.recv_async().await.map_err(|_| UI_DROPPED_ERR)
}

/// `ui_roundtrip` for JSON replies, shaped into the `(value, err)` pair Lua
/// expects. Never reaching the UI and the UI refusing both land in the error
/// slot.
pub(crate) async fn ui_json_roundtrip(
    lua: &Lua,
    tx: Option<&flume::Sender<UiAction>>,
    action: impl FnOnce(flume::Sender<UiReply>) -> UiAction,
) -> LuaResult<Pair<Value>> {
    let reply = try_pair!(ui_roundtrip(tx, action).await);
    let value = try_pair!(reply);
    Ok((Some(json_to_lua(lua, &value)?), None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_permissions::PluginPermissions;
    use crate::runtime::as_plugin_load;
    use futures::FutureExt;
    use mlua::Lua;
    use test_case::test_case;

    #[test]
    fn publish_snapshot_from_multiple_plugins() {
        let lua = Lua::new();
        let mut map = CommandHandlerMap::new();
        let expected = [
            ("plugA", "/cmd1", "desc1"),
            ("plugA", "/cmd2", "desc2"),
            ("plugB", "/cmd3", "desc3"),
        ];
        for (plugin, name, description) in expected {
            let handler = lua.create_function(|_, ()| Ok(())).unwrap();
            map.entry(Arc::from(plugin)).or_default().insert(
                Arc::from(name),
                CommandEntry {
                    handler: lua.create_registry_value(handler).unwrap(),
                    description: Arc::from(description),
                    reach: PluginReach::new(PluginPermissions::trusted()),
                    max_args: 0,
                },
            );
        }
        let (writer, reader) = LuaCommandWriter::new();
        publish_command_snapshot(&map, &writer);
        let snap = reader.load();
        let mut commands: Vec<_> = snap
            .commands
            .iter()
            .map(|c| (&*c.plugin, &*c.name, &*c.description))
            .collect();
        commands.sort_unstable();
        assert_eq!(commands, expected);
    }

    /// The loop's receiver is a clone and the runtime keeps another one for
    /// the whole process, so the channel never disconnects: without the bit a
    /// roundtrip would park on a reply nobody is left to send.
    #[tokio::test]
    async fn ui_roundtrip_fails_once_the_loop_detaches() {
        let (tx, rx) = flume::unbounded();
        let attachment = UiAttachment::default();
        install_ui_attachment(attachment.clone());

        attachment.detach();
        let detached = ui_roundtrip(Some(&tx), |reply_tx| UiAction::WinSaveView { reply_tx }).await;
        assert_eq!(detached.err(), Some(NO_UI_ERR));
        assert!(
            !rx.is_disconnected(),
            "the send has to fail on the bit, not on the channel"
        );

        // `/reload` builds a new loop around the same handles.
        attachment.attach();
        assert!(ui_send(Some(&tx), UiAction::WinRestView { scroll_top: 0 }).is_ok());
    }

    /// The load is refused, while a task that runs as the load waits is not.
    #[tokio::test]
    async fn only_the_plugin_load_itself_is_refused_a_roundtrip() {
        let (tx, rx) = flume::unbounded();
        let (resume_tx, resume_rx) = flume::bounded::<()>(1);

        let (in_load, beside_load) = tokio::join!(
            as_plugin_load(async {
                let refused =
                    ui_roundtrip(Some(&tx), |reply_tx| UiAction::WinSaveView { reply_tx }).await;
                resume_rx.recv_async().await.ok();
                refused
            }),
            async {
                let sent = ui_roundtrip(Some(&tx), |reply_tx| UiAction::WinSaveView { reply_tx })
                    .now_or_never();
                resume_tx.send(()).ok();
                sent
            },
        );

        assert_eq!(in_load.err(), Some(PLUGIN_LOADING_ERR));
        assert!(beside_load.is_none(), "still waiting for the UI's answer");
        assert!(matches!(rx.try_recv(), Ok(UiAction::WinSaveView { .. })));
        assert!(rx.is_empty(), "the refused call never reached the UI");
    }

    #[test_case(Dimension::Abs(42), 200 => 42 ; "abs_ignores_total")]
    #[test_case(Dimension::Percent(50), 200 => 100 ; "percent_half")]
    #[test_case(Dimension::Percent(100), 80 => 80 ; "percent_full")]
    #[test_case(Dimension::Percent(0), 80 => 0 ; "percent_zero")]
    #[test_case(Dimension::Percent(33), 100 => 33 ; "percent_truncates")]
    #[test_case(Dimension::Percent(1), 3 => 0 ; "percent_rounds_down_small")]
    fn dimension_resolve(dim: Dimension, total: u16) -> u16 {
        dim.resolve(total)
    }

    #[test_case("panel", Split::Panel, None ; "panel_has_no_edge")]
    #[test_case("above", Split::Above, Some(Edge { axis: Axis::Vertical, at_start: true }) ; "above")]
    #[test_case("below", Split::Below, Some(Edge { axis: Axis::Vertical, at_start: false }) ; "below")]
    #[test_case("left", Split::Left, Some(Edge { axis: Axis::Horizontal, at_start: true }) ; "left")]
    #[test_case("right", Split::Right, Some(Edge { axis: Axis::Horizontal, at_start: false }) ; "right")]
    #[test_case("none", Split::None, None ; "none_has_no_edge")]
    #[test_case("", Split::None, None ; "empty_defaults_none")]
    #[test_case("Below", Split::None, None ; "exact_match_is_case_sensitive")]
    #[test_case("garbage", Split::None, None ; "unknown_defaults_none")]
    fn split_geometry(s: &str, expected: Split, edge: Option<Edge>) {
        let split = Split::parse(s);
        assert_eq!(split, expected);
        assert_eq!(split.edge(), edge);
    }

    #[test]
    fn a_patch_changes_only_the_supplied_fields() {
        let mut cfg = FloatConfig {
            row: Some(10),
            col: Some(20),
            ..FloatConfig::default()
        };
        cfg.apply_patch(FloatConfigPatch {
            title: Some("hello".into()),
            zindex: Some(99),
            cursor_line: Some(true),
            row: Some(None),
            col: Some(Some(5)),
            ..FloatConfigPatch::default()
        });
        assert_eq!(cfg.title, "hello");
        assert_eq!(cfg.zindex, 99);
        assert!(cfg.cursor_line);
        assert_eq!(cfg.width, Dimension::Percent(60), "untouched fields stay");
        assert_eq!(cfg.border, Border::Rounded, "untouched fields stay");
        assert_eq!(cfg.row, None, "Some(None) clears the value");
        assert_eq!(cfg.col, Some(5), "Some(Some(5)) overwrites it");
    }

    const SHADOWED_PLUGIN: &str = "shadowed";
    const OTHER_PLUGIN: &str = "other";
    const EVERY_TAB_HINT: &str = "every tab";
    const SESSION_HINT: &str = "session";

    fn hint(plugin: &str, session: Option<TabRef>, text: &str) -> HintEntry {
        HintEntry {
            plugin: Arc::from(plugin),
            session,
            spans: vec![(text.into(), String::new())],
        }
    }

    #[test_case(true, &[EVERY_TAB_HINT, SESSION_HINT] ; "own_tab_swaps_in_the_session_hint")]
    #[test_case(false, &[EVERY_TAB_HINT, EVERY_TAB_HINT] ; "other_tab_keeps_the_every_tab_hint")]
    fn session_hint_shadows_the_plugins_every_tab_hint(own_tab: bool, expected: &[&str]) {
        let owner = MakiId::generate();
        let snap = HintSnapshot {
            entries: vec![
                hint(OTHER_PLUGIN, None, EVERY_TAB_HINT),
                hint(SHADOWED_PLUGIN, None, EVERY_TAB_HINT),
                hint(SHADOWED_PLUGIN, Some(TabRef::from(owner)), SESSION_HINT),
            ],
            generation: 0,
        };
        let tab = if own_tab { owner } else { MakiId::generate() };

        let shown: Vec<_> = snap
            .visible_in(tab, None)
            .map(|entry| entry.spans[0].0.as_str())
            .collect();

        assert_eq!(shown, expected);
    }

    const AGENT_SESSION: &str = "agent-session";

    #[test]
    fn a_named_tab_answers_to_its_agents_id_then_to_its_local_id() {
        let local = MakiId::generate();
        let named = |name: &str| TabRef(Arc::from(name));

        let agent_id = MakiId::generate();
        assert!(TabRef::from(agent_id).names(local, Some(&agent_id.to_string())));
        assert!(named(AGENT_SESSION).names(local, Some(AGENT_SESSION)));
        assert!(named(&local.to_string()).names(local, Some(AGENT_SESSION)));
        assert!(!named(AGENT_SESSION).names(local, None));
        assert!(!named(&MakiId::generate().to_string()).names(local, Some(AGENT_SESSION)));
    }
}
