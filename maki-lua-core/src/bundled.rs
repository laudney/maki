//! The plugins a host builds into its binary. The core reserves their names
//! and serves their directories to `require()`; the host decides which ones
//! exist.

use include_dir::{Dir, File};

use crate::error::PluginError;
use crate::plugin_permissions::{Component, MANIFEST_FILE, PluginPermissions, Requested};
use crate::runtime::LoadChunk;

const LIB_PLUGIN: &str = "lib";
/// A bundled plugin's agent entry.
pub const INIT_ENTRY: &str = "init.lua";
/// A bundled plugin's UI entry.
pub const UI_ENTRY: &str = "ui.lua";

/// {component}'s entry in a bundled plugin.
pub const fn entry(component: Component) -> &'static str {
    match component {
        Component::Agent => INIT_ENTRY,
        Component::Ui => UI_ENTRY,
    }
}

#[macro_export]
macro_rules! bundled_plugins {
    ($($name:expr => $dir:expr),* $(,)?) => {
        &[$($crate::bundled::BundledPlugin { name: $name, dir: $dir }),*]
    };
}

pub struct BundledPlugin {
    pub name: &'static str,
    pub dir: Dir<'static>,
}

impl BundledPlugin {
    /// {component}'s entry as a chunk of this plugin's load, none when the
    /// plugin lacks it. `init.lua` keeps the plugin's name as its chunk name,
    /// any other entry is named `<plugin>/<entry>`.
    pub fn entry_chunk(&self, component: Component) -> Option<LoadChunk> {
        let entry = entry(component);
        let source = self.dir.get_file(entry)?.contents_utf8()?;
        let name = match component {
            Component::Agent => self.name.to_owned(),
            Component::Ui => format!("{}/{entry}", self.name),
        };
        Some(LoadChunk::bundled(name, source))
    }

    /// A bundled plugin declares its permissions the same way an external one
    /// does. Shipping inside the binary buys no implicit grant, so an
    /// unreadable manifest fails the load rather than quietly widening
    /// anything, and a newly guarded function stays denied until the plugin
    /// asks for it.
    ///
    /// Only the component this VM loads receives its grant.
    pub fn permissions(&self, component: Component) -> Result<PluginPermissions, PluginError> {
        let fail = |message: String| PluginError::BundledManifest {
            plugin: self.name.to_owned(),
            message,
        };
        let source = self
            .dir
            .get_file(MANIFEST_FILE)
            .and_then(File::contents_utf8)
            .ok_or_else(|| fail(format!("no {MANIFEST_FILE} next to init.lua")))?;
        let manifest = toml::from_str::<toml::Value>(source).map_err(|e| fail(e.to_string()))?;
        Ok(Requested::from_manifest(&manifest, component).granted())
    }
}

pub struct Bundled {
    plugins: &'static [BundledPlugin],
}

impl Bundled {
    pub const fn new(plugins: &'static [BundledPlugin]) -> Self {
        Self { plugins }
    }

    pub fn plugins(&self) -> &'static [BundledPlugin] {
        self.plugins
    }

    pub fn find(&self, name: &str) -> Result<&'static BundledPlugin, PluginError> {
        self.plugins
            .iter()
            .find(|plugin| plugin.name == name)
            .ok_or_else(|| PluginError::UnknownPlugin {
                plugin: name.to_owned(),
            })
    }

    /// Every bundled name, not just the default-enabled ones. An external
    /// package sharing an owner name with any of them would let one package's
    /// unload tear down the other's registrations.
    pub fn is_bundled(&self, name: &str) -> bool {
        self.plugins.iter().any(|p| p.name == name)
    }

    /// `lib` is not a default builtin; it exists so plugins can `require()`
    /// shared modules across boundaries.
    pub fn lib(&self) -> &'static Dir<'static> {
        &self
            .plugins
            .iter()
            .find(|p| p.name == LIB_PLUGIN)
            .expect("lib plugin bundled")
            .dir
    }

    /// The first bundled directory holding {rel_path}, as UTF-8 source.
    pub(crate) fn source(&self, rel_path: &str) -> Option<&'static str> {
        self.plugins
            .iter()
            .find_map(|p| p.dir.get_file(rel_path).and_then(|f| f.contents_utf8()))
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use include_dir::{Dir, include_dir};

    use super::{Bundled, BundledPlugin, LIB_PLUGIN};

    /// A builtin name that is enabled by default, and the real `lib`, which
    /// is not, for core tests that run without a host.
    pub(crate) static TEST_BUNDLED: Bundled = Bundled::new(&[
        BundledPlugin {
            name: "bash",
            dir: Dir::new("bash", &[]),
        },
        BundledPlugin {
            name: LIB_PLUGIN,
            dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/lib"),
        },
    ]);
}
