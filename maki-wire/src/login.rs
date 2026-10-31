//! The login extension: `/login` on a remote tab runs the agent's own
//! provider login as an ACP terminal auth method, in the client's terminal,
//! so no credential crosses the link, and `_maki/auth/changed` tells the
//! agent to rebuild what it connected under the old credentials.

pub const LOGIN_KEY: &str = "login";
pub const LOGIN_VERSION: u64 = 1;
pub const CHANGED_METHOD: &str = "_maki/auth/changed";
pub const METHOD_ID: &str = "maki-login";
pub const METHOD_NAME: &str = "Log in to a provider";
/// What `maki acp` takes to run the login instead of serving.
pub const LOGIN_FLAG: &str = "--login";
