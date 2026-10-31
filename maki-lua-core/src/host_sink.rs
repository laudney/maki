//! What the core tells the user: a watchdog kill, a permission gap, a failed
//! task or a plugin gone. The host installs a sink
//! for it, so the core never names the UI it reaches.

#[cfg(test)]
use std::sync::{Arc, Mutex};

use mlua::Lua;

pub trait HostSink: Send {
    /// Shows {text} to the user; false when nobody received it, so the
    /// caller logs it as it does today without a UI.
    fn flash(&self, text: String) -> bool;
    fn plugin_unloaded(&self, plugin: &str);
}

/// The sink of a host with nobody to show anything to: each caller's own log
/// line is all that is left.
pub(crate) struct LogSink;

impl HostSink for LogSink {
    fn flash(&self, _text: String) -> bool {
        false
    }

    fn plugin_unloaded(&self, _plugin: &str) {}
}

struct InstalledSink(Box<dyn HostSink>);

pub fn install_host_sink(lua: &Lua, sink: impl HostSink + 'static) {
    lua.set_app_data(InstalledSink(Box::new(sink)));
}

fn with_sink<R>(lua: &Lua, tell: impl FnOnce(&dyn HostSink) -> R) -> R {
    match lua.app_data_ref::<InstalledSink>() {
        Some(sink) => tell(sink.0.as_ref()),
        None => tell(&LogSink),
    }
}

/// Lets the watchdog tell the user which plugin it stopped. It flashes
/// instead of going through `maki.notify`, because a notify handler is
/// plugin code too. If that handler were the one stuck, every kill would
/// call it again and freeze maki for another few seconds, forever.
pub(crate) fn host_flash(lua: &Lua, text: String) -> bool {
    with_sink(lua, |sink| sink.flash(text))
}

pub(crate) fn host_unloaded(lua: &Lua, plugin: &str) {
    with_sink(lua, |sink| sink.plugin_unloaded(plugin));
}

/// A sink that keeps what the core told it, for core tests to read back.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct RecordingSink {
    flashes: Arc<Mutex<Vec<String>>>,
    unloads: Arc<Mutex<Vec<String>>>,
}

#[cfg(test)]
impl RecordingSink {
    /// Installs a fresh sink on {lua} and returns a handle on what it records.
    pub(crate) fn install(lua: &Lua) -> Self {
        let sink = Self::default();
        install_host_sink(lua, sink.clone());
        sink
    }

    /// The texts flashed so far, oldest first, leaving none recorded.
    pub(crate) fn take_flashes(&self) -> Vec<String> {
        std::mem::take(&mut *self.flashes.lock().unwrap())
    }

    pub(crate) fn take_unloads(&self) -> Vec<String> {
        std::mem::take(&mut *self.unloads.lock().unwrap())
    }
}

#[cfg(test)]
impl HostSink for RecordingSink {
    fn flash(&self, text: String) -> bool {
        self.flashes.lock().unwrap().push(text);
        true
    }

    fn plugin_unloaded(&self, plugin: &str) {
        self.unloads.lock().unwrap().push(plugin.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const TEXT: &str = "plugin stopped";

    #[test_case(false ; "no_sink_installed")]
    #[test_case(true ; "log_sink_installed")]
    fn a_flash_without_a_ui_reaches_nobody(log_sink: bool) {
        let lua = Lua::new();
        if log_sink {
            install_host_sink(&lua, LogSink);
        }
        assert!(!host_flash(&lua, TEXT.into()));
    }

    #[test]
    fn an_installed_sink_hears_every_flash_and_unload() {
        const PLUGIN: &str = "gone";
        let lua = Lua::new();
        let sink = RecordingSink::install(&lua);

        assert!(host_flash(&lua, TEXT.into()));
        host_unloaded(&lua, PLUGIN);

        assert_eq!(sink.take_flashes(), [TEXT]);
        assert_eq!(sink.take_unloads(), [PLUGIN]);
    }
}
