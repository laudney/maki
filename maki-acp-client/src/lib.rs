mod cli;
mod connect;

pub use cli::ConnectArgs;
pub use connect::connect;

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
