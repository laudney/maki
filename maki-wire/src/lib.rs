//! The ACP client, the Maki extensions' wire types, the display mapping
//! and the client links, with no edge to the agent. `maki-acp`'s host
//! depends on it for the wire types both sides share.

pub mod btw;
pub mod client;
pub mod commands;
pub mod configuration;
pub mod display;
pub mod elicitation;
pub mod extensions;
pub mod format;
pub mod login;
pub mod methods;
pub mod plan;
pub mod plugins;
pub mod sanitize;
pub mod session;
pub mod settings;
pub mod tool_data;
pub mod usage;
pub mod websocket;

#[cfg(test)]
fn test_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::LazyLock<tokio::runtime::Runtime> = std::sync::LazyLock::new(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime")
    });
    &RUNTIME
}
#[cfg(test)]
fn test_block_on<F: std::future::Future>(future: F) -> F::Output {
    test_runtime().block_on(future)
}
