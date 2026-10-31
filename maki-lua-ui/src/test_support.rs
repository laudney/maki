//! Stand-ins for the Lua thread's writers and request decoders for a UI that
//! probes its handles.

use std::sync::Arc;

use maki_lua_core::runtime::Request;
use maki_types::snapshot::SharedBuf;

pub use crate::api::keymap::TAKEN_ERR;
use crate::api::keymap::{
    InputMode, KeybindTicket, KeymapReader, KeymapStore, KeymapWriter, SetOpts,
};
use crate::api::util::command::{HintReader, HintWriter, LuaCommandReader, LuaCommandWriter};
pub use crate::domains::ui::{FALLBACK_SPENT_ERR, ui_ops};
use crate::domains::ui::{RunCommand, RunKeybindCallback};
use crate::key::Key;
pub use crate::key_lint::{MODE_WARNING, UPGRADE_DOCS, lint};
use crate::plugin_permissions::PluginPermissions;

pub use crate::api::util::command::{
    HintWriter as HintWriterHandle, LuaCommandWriter as LuaCommandWriterHandle,
};

pub fn lua_command_writer_pair() -> (LuaCommandWriterHandle, LuaCommandReader) {
    LuaCommandWriter::new()
}

pub fn hint_writer_pair() -> (HintWriterHandle, HintReader) {
    HintWriter::new()
}

pub fn dispatched_command(req: Request) -> Option<(String, String, u8)> {
    let RunCommand {
        command,
        args,
        depth,
        ..
    } = req.into_domain()?;
    Some((command.to_string(), args, depth))
}

pub fn keybind_ticket(req: Request) -> Option<KeybindTicket> {
    req.into_domain::<RunKeybindCallback>()
        .map(|RunKeybindCallback { ticket }| ticket)
}

pub fn set_mode(reader: &KeymapReader, mode: InputMode) {
    reader.mode.set(mode);
}

/// One binding for [`keymap_reader_from`], global when `buffer` is `None`.
pub struct TestBinding<'a> {
    pub plugin: &'a str,
    pub modes: &'a [InputMode],
    pub key: Key,
    pub buffer: Option<&'a Arc<SharedBuf>>,
}

/// Publishes {binds}, set in order, so a later one shadows an earlier one
/// in its scope the way a later `maki.keymap.set` would.
pub fn keymap_reader_from(binds: &[TestBinding]) -> KeymapReader {
    let lua = mlua::Lua::new();
    let mut store = KeymapStore::new();
    for bind in binds {
        let callback = lua
            .create_registry_value(lua.create_function(|_, ()| Ok(())).unwrap())
            .unwrap();
        store.set(
            bind.key,
            bind.modes,
            callback,
            Arc::from(bind.plugin),
            PluginPermissions::trusted(),
            SetOpts {
                buffer: bind.buffer.map(Arc::downgrade),
                ..Default::default()
            },
        );
    }
    let (writer, reader) = KeymapWriter::new();
    writer.publish(store.snapshot_entries());
    reader
}
