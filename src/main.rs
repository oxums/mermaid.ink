use std::process::ExitCode;
use std::sync::Arc;

use mermaid_ink::{App, Config};
use tracing_subscriber::EnvFilter;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> ExitCode {
    let filter = match std::env::var("RUST_LOG") {
        Ok(v) if !v.is_empty() => match EnvFilter::try_new(&v) {
            Ok(f) => f,
            Err(e) => return fail(format!("invalid RUST_LOG: {e}")),
        },
        // usvg/resvg/krilla warnings quote attribute values from the diagram, at no rate limit.
        _ => EnvFilter::new("info,usvg=off,resvg=off,krilla=off,merman=warn"),
    };
    // rediss:// (redis-rs) builds rustls configs from the process-wide provider; install it
    // explicitly so no feature combination can leave it ambiguous. Err: already installed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let addr = (config.host.clone(), config.port);
    let app = match App::new(config) {
        Ok(a) => Arc::new(a),
        Err(e) => return fail(e),
    };
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => return fail(format!("bind: {e}")),
    };
    if let Ok(addr) = listener.local_addr() {
        tracing::info!(%addr, "listening");
    }
    match mermaid_ink::serve(listener, app, mermaid_ink::shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(format!("server: {e}")),
    }
}

fn fail(msg: String) -> ExitCode {
    tracing::error!("{msg}");
    eprintln!("mermaid-ink: {msg}");
    ExitCode::FAILURE
}
