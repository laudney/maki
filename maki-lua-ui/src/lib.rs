//! The UI binding over `maki-lua-core`: commands, actions, keymaps, windows,
//! input, hints, forms and rendering.

use maki_lua_core::{
    chunks, docs, domain, error, host_sink, language, pack, plugin_permissions, runtime,
};

mod api;
pub(crate) mod buffer;
mod domains;
mod host;
pub mod key;
mod key_lint;
mod question_form;
pub mod test_support;
mod ui_host;

pub use api::action::ActionInfo;
pub use api::keymap::{
    InputMode, InputSnapshot, KeybindTicket, KeymapEntry, KeymapReader, KeymapSnapshot,
};
pub use api::ui::marks::{
    Decoration, InputRule, InputRulesReader, Mark, PatternRule, Reveal, VirtTextPos,
};
pub use api::ui::renderer::{RenderRoute, RendererRoutes};
pub use api::ui::syntax::{SyntaxDoc, SyntaxEdit, SyntaxIndex, SyntaxInput, SyntaxRule};
pub use api::util::command::{
    Anchor, Axis, Border, BuiltinAction, Dimension, Edge, FloatConfig, FloatConfigPatch, HintEntry,
    HintReader, HintSnapshot, InputDecorate, InputEdit, InputMarks, InputRequest, InputSubmit,
    LuaCommandInfo, LuaCommandReader, ModelRequest, NO_UI_ERR, PlanActionOutcome, PlanFormRow,
    PlanMenu, PlanRequest, PlanRowAction, SessionRequest, Split, TabRef, TaskRequest, TextEdit,
    TitlePos, UiAction, UiAttachment, UiReply, WinCommand, WinEvent, WinView,
};
pub use domains::ui::{
    DOCS, PLAN_FORM_SLOT_DEADLINE, PLAN_ROW_HANDLER_DEADLINE, RenderCall, RenderCtx, RenderReason,
    RenderReply,
};
pub use host::UiHandle;
pub use key::{Key, PASTE, RESERVED_KEYS, is_reserved};
pub use key_lint::KEY_WARNING;
pub use question_form::QuestionFormAnswer;
pub use ui_host::{UiDirs, UiFiles, UiHost, UiStart, UiStartError, prepare_pack_command};
