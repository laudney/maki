//! The stages a tool call can be hooked at and the authority hooking it
//! hands over, read by whoever runs the hooks and whoever installs them.

use maki_config::Permission;

/// Fields of the value a [`HookStage::Output`] hook sees and returns.
pub const OUTPUT_TEXT: &str = "text";
pub const OUTPUT_IS_ERROR: &str = "is_error";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum HookStage {
    /// Before the input is parsed and before permission rules resolve, so
    /// rules judge the call as the hook left it. Any later and a hook could
    /// smuggle in an argument the rules never approved.
    Input,
    /// After the call finished, on the text it produced (failures included),
    /// before that text becomes history.
    Output,
}

impl HookStage {
    /// Order matters: stages index array slots via `as usize`.
    pub const ALL: [Self; 2] = [Self::Input, Self::Output];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

/// What hooking a call would hand whoever wrote the hook. Rewriting an input
/// decides what the tool then does, so the host prices that before it lets
/// untrusted code near it.
///
/// There is no "free" variant on purpose. A tool declaring no capability has
/// not told us it exercises none, only that nothing asks: `batch`, `task` and
/// `code_execution` declare nothing and reach every other tool. So a tool
/// landing tomorrow is never silently free to layer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Authority {
    /// Exactly the capability the tool declares; a layer needs that one.
    Capability(Permission),
    /// Reach nobody declared: a tool naming no capability, an MCP server tool,
    /// an ACP client tool, search. A layer needs every capability a tool can
    /// reach, because no narrower answer would be honest. `agent` is not one:
    /// every call already runs inside a turn.
    Unbounded,
    /// Steering what the agent does next, up to turns nobody typed. A layer
    /// needs every permission, `agent` included.
    Total,
}
