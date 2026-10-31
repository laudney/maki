//! Commands, actions, keymaps, windows, input, hints, forms and tool
//! renderers.

use std::sync::Arc;

use flume::Sender;
use maki_storage::id::MakiId;
use maki_types::snapshot::UiWaker;
use mlua::{Lua, Result as LuaResult, Table};

use crate::api::action::{self, ActionStore, clear_plugin_actions};
use crate::api::command;
use crate::api::keymap::{self, KeymapReader, KeymapStore, KeymapWriter, release_mode};
use crate::api::ui::marks::{InputRuleStore, InputRulesReader};
use crate::api::ui::renderer::{RendererRoutes, RendererStore, clear_plugin_renderers};
use crate::api::ui::{self, HintStore, WinStore, clear_session_hints, view};
use crate::api::util::command::{
    CommandHandlerMap, HintReader, HintWriter, LuaCommandReader, LuaCommandWriter, UiAction,
    UiAttachment, UiSink, install_ui_attachment, publish_command_snapshot, ui_send,
};
use crate::docs::DocEntry;
use crate::domain::{Domain, LoadScope};
use crate::error::PluginError;
use crate::host_sink::install_host_sink;
use crate::key_lint::KeyLint;
use crate::pack::Interaction;
use crate::plugin_permissions::Component;
use crate::question_form::QuestionFormCache;
use crate::runtime::{TaskExts, install_user_source_check};
pub use facade::ui_ops;
use plan_rows::{PlanRowHandlers, clear_plugin_rows, clear_session_rows};
#[cfg(test)]
pub(crate) use warm::WARM_CALL_CAP;
use warm::{CallTurns, WarmCalls, clear_plugin_warm};

mod facade;
mod keybind;
mod plan_form;
mod plan_rows;
pub(crate) mod render;
mod requests;
mod warm;

pub use keybind::FALLBACK_SPENT_ERR;
pub use plan_form::{PLAN_FORM_SLOT_DEADLINE, PLAN_ROW_HANDLER_DEADLINE};
pub(crate) use render::{ClickTool, RenderTool};
pub use render::{RenderCall, RenderCtx, RenderReason, RenderReply};
pub(crate) use requests::{
    OpenPlanForm, OpenQuestionForm, RunCommand, RunKeybindCallback, RunPlanAction,
};

const NAME: &str = "ui";
pub const DOCS: &[DocEntry] = &[
    DocEntry::new(410, &crate::buffer::render::DOCS),
    DocEntry::new(430, &crate::buffer::buf::DOCS),
    DocEntry::new(21, &command::COMMAND_DOCS),
    DocEntry::new(25, &command::RUN_COMMAND_DOCS),
    DocEntry::new(40, &action::DOCS),
    DocEntry::new(155, &view::DOCS),
    DocEntry::new(220, &keymap::DOCS),
    DocEntry::new(411, &ui::UI_DOCS),
    DocEntry::new(420, &ui::win::DOCS),
];

/// The writing halves the host's readers watch, moved into Lua app data on
/// install.
pub(crate) struct UiStores {
    pub command_writer: LuaCommandWriter,
    pub keymap_writer: KeymapWriter,
    pub hint_writer: HintWriter,
    pub input_rule_store: InputRuleStore,
    pub key_lint: KeyLint,
    pub ui_waker: UiWaker,
    pub ui_attachment: UiAttachment,
    pub renderer_routes: Arc<RendererRoutes>,
}

pub(crate) struct UiDomain {
    pub ui_action_tx: Option<Sender<UiAction>>,
    pub stores: Option<UiStores>,
}

/// The reading halves of [`UiStores`] and the UI channels, which the host
/// keeps outside the VM thread.
pub(crate) struct UiHandles {
    pub command_reader: LuaCommandReader,
    pub keymap_reader: KeymapReader,
    pub hint_reader: HintReader,
    pub input_rules_reader: InputRulesReader,
    pub ui_action_rx: flume::Receiver<UiAction>,
    pub ui_wake_rx: flume::Receiver<()>,
    pub ui_attachment: UiAttachment,
    pub renderer_routes: Arc<RendererRoutes>,
    /// What the user source check found on the Lua thread, for the host to
    /// report.
    pub key_lint: KeyLint,
}

impl UiDomain {
    /// The domain with its stores, and the handles a host keeps of them. Only
    /// a TTY host sends the UI anything.
    pub(crate) fn new(interaction: Interaction) -> (Self, UiHandles) {
        let (ui_action_tx, ui_action_rx) = flume::unbounded::<UiAction>();
        let (ui_waker, ui_wake_rx) = UiWaker::new();
        let ui_attachment = UiAttachment::default();
        let (command_writer, command_reader) = LuaCommandWriter::new();
        let (keymap_writer, keymap_reader) = KeymapWriter::new();
        let (hint_writer, hint_reader) = HintWriter::new();
        let (input_rule_store, input_rules_reader) = InputRuleStore::new();
        let key_lint = KeyLint::default();
        let renderer_routes = Arc::<RendererRoutes>::default();
        let domain = Self {
            ui_action_tx: (interaction == Interaction::Tty).then_some(ui_action_tx),
            stores: Some(UiStores {
                command_writer,
                keymap_writer,
                hint_writer,
                input_rule_store,
                key_lint: key_lint.clone(),
                ui_waker,
                ui_attachment: ui_attachment.clone(),
                renderer_routes: Arc::clone(&renderer_routes),
            }),
        };
        let handles = UiHandles {
            command_reader,
            keymap_reader,
            hint_reader,
            input_rules_reader,
            ui_action_rx,
            ui_wake_rx,
            ui_attachment,
            renderer_routes,
            key_lint,
        };
        (domain, handles)
    }
}

impl Domain for UiDomain {
    fn name(&self) -> &'static str {
        NAME
    }

    fn component(&self) -> Component {
        Component::Ui
    }

    fn begin_task(&self, _lua: &Lua, exts: &mut TaskExts) {
        crate::buffer::begin_task(exts);
    }

    fn docs(&self) -> &'static [DocEntry] {
        DOCS
    }

    fn install(&mut self, lua: &Lua) -> LuaResult<()> {
        lua.set_app_data(HostUi(self.ui_action_tx.clone()));
        install_host_sink(lua, UiSink(self.ui_action_tx.clone()));
        let Some(stores) = self.stores.take() else {
            return Ok(());
        };
        install_ui_attachment(stores.ui_attachment);
        lua.set_app_data(CommandHandlerMap::new());
        lua.set_app_data(stores.command_writer);
        lua.set_app_data(KeymapStore::new());
        lua.set_app_data(ActionStore::default());
        lua.set_app_data(stores.keymap_writer);
        lua.set_app_data(HintStore::new());
        lua.set_app_data(WinStore::default());
        lua.set_app_data(stores.hint_writer);
        lua.set_app_data(PlanRowHandlers::default());
        let key_lint = stores.key_lint.clone();
        install_user_source_check(lua, move |chunk, source| key_lint.check(chunk, source));
        lua.set_app_data(stores.key_lint);
        lua.set_app_data(stores.input_rule_store);
        lua.set_app_data(stores.ui_waker);
        lua.set_app_data(QuestionFormCache::default());
        lua.set_app_data(RendererStore::new(stores.renderer_routes));
        lua.set_app_data(WarmCalls::default());
        lua.set_app_data(CallTurns::default());
        Ok(())
    }

    fn extend(&self, lua: &Lua, maki: &Table, load: &LoadScope<'_>) -> LuaResult<()> {
        crate::buffer::extend(lua, maki)?;
        let plugin = load.plugin;
        let reach = load.reach;
        let api: Table = maki.get("api")?;
        command::add_ui_fns(
            &api,
            lua,
            self.ui_action_tx.clone(),
            reach.clone(),
            Arc::clone(plugin),
        )?;
        action::add_action_methods(&api, lua, Arc::clone(plugin), reach.clone())?;
        ui::add_ui_fns(
            &maki.get::<Table>("ui")?,
            lua,
            self.ui_action_tx.clone(),
            Arc::clone(plugin),
            reach.clone(),
        )?;
        view::add_fn_ui(&maki.get::<Table>("fn")?, lua, self.ui_action_tx.clone())?;
        maki.set(
            "keymap",
            keymap::create_keymap_table(lua, Arc::clone(plugin), reach.permissions().clone())?,
        )?;
        maki_lua_core::facade::extend(
            lua,
            maki,
            load,
            &maki_lua_core::facade::FacadeOps::Ui(ui_ops(self.ui_action_tx.clone())),
        )
    }

    fn reload(&self, lua: &Lua, plugin: &str) {
        drop_plugin_keys(lua, plugin);
        // The one place a name `clear_plugin` tombstoned comes back live, so
        // a handler of the load being replaced cannot publish bindings that
        // fire for a load that is gone.
        if let Some(mut store) = lua.app_data_mut::<KeymapStore>() {
            store.revive(plugin);
        }
    }

    fn commit_load(&self, lua: &Lua, load: &LoadScope<'_>) -> Result<(), PluginError> {
        let name = load.plugin;
        if let Some(key) = lua
            .app_data_ref::<KeymapStore>()
            .and_then(|store| store.normal_only_key(name))
            && let Some(key_lint) = lua.app_data_ref::<KeyLint>()
        {
            key_lint.check_normal_only(name, key);
        }
        Ok(())
    }

    fn clear_plugin(&self, lua: &Lua, plugin: &str) {
        drop_plugin_keys(lua, plugin);
        if let Some(mut store) = lua.app_data_mut::<KeymapStore>() {
            store.clear_plugin(plugin);
            let entries = store.snapshot_entries();
            drop(store);
            if let Some(writer) = lua.app_data_ref::<KeymapWriter>() {
                writer.publish(entries);
            }
        }
        if let Some(mut store) = lua.app_data_mut::<HintStore>() {
            store.clear_plugin(plugin);
            let entries = store.snapshot_entries();
            drop(store);
            if let Some(writer) = lua.app_data_ref::<HintWriter>() {
                writer.publish(entries);
            }
        }
        if let Some(mut store) = lua.app_data_mut::<InputRuleStore>() {
            store.clear_plugin(plugin);
        }
        // Last of the revocations, and the only one the user can see: a float
        // left up by an unloaded plugin still holds the keys it claimed, with
        // the loop that answered them cancelled along with the load.
        if let Some(mut store) = lua.app_data_mut::<WinStore>() {
            store.close_plugin(plugin);
        }
    }

    fn end_session(&self, lua: &Lua, session: MakiId) {
        // Nothing can pick from this session's plan form again.
        clear_session_rows(lua, &session.to_string());
        clear_session_hints(lua, session);
    }
}

/// The UI sender for host code that answers the UI itself, such as a
/// keybind handler.
struct HostUi(Option<flume::Sender<UiAction>>);

fn host_ui_send(lua: &Lua, action: UiAction) {
    let tx = lua.app_data_ref::<HostUi>().and_then(|ui| ui.0.clone());
    let _ = ui_send(tx.as_ref(), action);
}

fn drop_plugin_keys(lua: &Lua, name: &str) {
    // A row stays on screen with the layer that drew it gone, but its
    // handler captured the permissions the user just revoked.
    clear_plugin_rows(lua, name);
    clear_plugin_actions(lua, name);
    clear_plugin_renderers(lua, name);
    clear_plugin_warm(lua, name);
    release_mode(lua, name);
    if let Some(mut cmd_map) = lua.app_data_mut::<CommandHandlerMap>()
        && let Some(cmds) = cmd_map.remove(name)
    {
        for (_, entry) in cmds {
            if let Err(e) = lua.remove_registry_value(entry.handler) {
                tracing::warn!(plugin = name, error = %e, "failed to drop command handler key");
            }
        }
        drop(cmd_map);
        if let (Some(map), Some(writer)) = (
            lua.app_data_ref::<CommandHandlerMap>(),
            lua.app_data_ref::<LuaCommandWriter>(),
        ) {
            publish_command_snapshot(&map, &writer);
        }
    }
}
