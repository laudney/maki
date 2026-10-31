//! Self-documenting Lua API. `api` modules define functions and userdata
//! methods with `#[maki_lua_macro::lua_fn]` (Lua name from the fn ident, args
//! from the signature, `@param`/`@return` tags validated against real
//! parameters at compile time) and assemble registration plus the `DOCS`
//! consts with `maki_lua_macro::lua_table!` / `lua_class!`. The few functions
//! that cannot fit (raw `MultiValue`, Lua chunks, conditional registration)
//! and `maki.setup` keep hand-written `FnDoc`s. The core and each domain
//! list theirs as [`DocEntry`]s, and [`compose_docs`] merges a composition's
//! lists into the reference.

use crate::plugin_permissions::Permission;

pub struct ModuleDoc {
    /// Dotted path, e.g. "maki.base64". Classes use a type name, e.g.
    /// "maki.treesitter.Node".
    pub name: &'static str,
    pub kind: DocKind,
    pub desc: &'static str,
    pub fns: &'static [FnDoc],
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    /// A real Lua table; the drift test checks its keys.
    Table,
    /// Methods on userdata handles; not enumerable, skipped by the drift test.
    Class,
}

pub struct FnDoc {
    pub name: &'static str,
    /// Argument list in Neovim notation, e.g. "{path}, {opts?}".
    pub args: &'static str,
    pub desc: &'static str,
    pub params: &'static [ParamDoc],
    /// E.g. "(string) encoded text" or "" when nothing is returned.
    pub returns: &'static str,
    /// Lua snippet rendered as a fenced code block, or "" when absent.
    pub example: &'static str,
    /// The plugin permissions gating this function, all of them required,
    /// from the `guard =` attribute. Rendered into the docs so the reference
    /// can never drift from the actual gate.
    pub guard: Option<&'static [Permission]>,
    /// A plain value such as `maki.NIL`, documented without a call signature.
    pub value: bool,
}

pub struct ParamDoc {
    /// E.g. "{path}".
    pub name: &'static str,
    /// E.g. "string|buffer".
    pub ty: &'static str,
    pub desc: &'static str,
}

/// A module's docs and its place in the reference. A section lists its
/// modules in rank order, and the sections follow their lowest rank, so the
/// reference reads the same whichever parts of the API a composition has.
pub struct DocEntry {
    pub rank: u16,
    pub module: &'static ModuleDoc,
}

impl DocEntry {
    pub const fn new(rank: u16, module: &'static ModuleDoc) -> Self {
        Self { rank, module }
    }
}

pub(crate) const CORE_DOCS: &[DocEntry] = {
    use crate::api;
    &[
        DocEntry::new(0, &api::util::setup::DOCS),
        DocEntry::new(10, &api::pack::DOCS),
        DocEntry::new(23, &api::options::DOCS),
        DocEntry::new(30, &api::autocmd::DOCS),
        DocEntry::new(60, &api::slot::DOCS),
        DocEntry::new(90, &api::r#async::DOCS),
        DocEntry::new(100, &api::r#async::SEMAPHORE_DOCS),
        DocEntry::new(110, &api::r#async::PERMIT_DOCS),
        DocEntry::new(120, &api::r#async::TASK_DOCS),
        DocEntry::new(130, &api::base64::DOCS),
        DocEntry::new(140, &api::env::DOCS),
        DocEntry::new(150, &api::r#fn::DOCS),
        DocEntry::new(160, &api::fs::DOCS),
        DocEntry::new(170, &api::image::DOCS),
        DocEntry::new(180, &api::image::IMAGE_DOCS),
        DocEntry::new(200, &api::json::DOCS),
        DocEntry::new(210, &api::json::VALIDATOR_DOCS),
        DocEntry::new(230, &api::log::DOCS),
        DocEntry::new(250, &api::net::DOCS),
        DocEntry::new(260, &api::net::CONN_DOCS),
        DocEntry::new(300, &api::top::DOCS),
        DocEntry::new(310, &api::top::TIMER_DOCS),
        DocEntry::new(330, &api::text::DOCS),
        DocEntry::new(340, &api::treesitter::DOCS),
        DocEntry::new(350, &api::treesitter::language::DOCS),
        DocEntry::new(360, &api::treesitter::query::DOCS),
        DocEntry::new(370, &api::treesitter::query::QUERY_DOCS),
        DocEntry::new(380, &api::treesitter::tree::DOCS),
        DocEntry::new(390, &api::treesitter::node::DOCS),
        DocEntry::new(400, &api::treesitter::language_tree::DOCS),
        DocEntry::new(440, &api::uv::DOCS),
        DocEntry::new(450, &api::yaml::DOCS),
    ]
};

/// The core's docs and each of {domains}' lists, in rank order.
pub fn compose_docs(
    domains: impl IntoIterator<Item = &'static [DocEntry]>,
) -> Vec<&'static ModuleDoc> {
    let mut entries: Vec<&DocEntry> = CORE_DOCS
        .iter()
        .chain(domains.into_iter().flatten())
        .collect();
    entries.sort_by_key(|entry| entry.rank);
    entries.into_iter().map(|entry| entry.module).collect()
}
