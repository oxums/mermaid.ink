//! mermaid.ink server: configuration from env and the axum application.

pub mod app;
pub mod config;

pub use app::{App, router, serve, shutdown_signal};
pub use config::Config;
