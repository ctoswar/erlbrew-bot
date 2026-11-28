//! Erlbrew Café Messenger bot — native Rust implementation.
//!
//! The public surface is small on purpose: [`run`] is what the binary calls,
//! and the modules are exposed so integration tests can drive the pipeline
//! directly without a network.

pub mod config;
pub mod db;
pub mod events;
pub mod faq;
pub mod forms;
pub mod llm;
pub mod menu;
pub mod messenger;
pub mod notify;
pub mod orders;
pub mod pipeline;
pub mod routes;
pub mod state;

pub use config::Settings;
pub use state::App;

/// Initialise structured JSON logging. Safe to call once per process.
pub fn init_logging(settings: &Settings) {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_new(&settings.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .json()
        .with_env_filter(filter)
        .with_target(true)
        .with_current_span(false)
        .with_span_list(false)
        .try_init();
}
