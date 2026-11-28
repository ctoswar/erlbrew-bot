//! Erlbrew Café — Facebook Messenger webhook service (native Rust).

mod config;
mod db;
mod events;
mod faq;
mod forms;
mod llm;
mod menu;
mod messenger;
mod notify;
mod orders;
mod pipeline;
mod routes;
mod state;

use anyhow::{Context, Result};
use config::Settings;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

fn init_logging(settings: &Settings) {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_new(&settings.log_level).unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

/// Docker HEALTHCHECK: `bot --health-check` pings the local /health endpoint.
async fn health_check(settings: &Settings) -> Result<()> {
    let url = format!("http://127.0.0.1:{}/health", settings.port);
    let client = reqwest::Client::builder().timeout(Duration::from_secs(4)).build()?;
    client.get(url).send().await?.error_for_status()?;
    Ok(())
}

fn build_app(settings: Settings) -> Result<state::App> {
    let settings = Arc::new(settings);
    let db = Arc::new(db::Db::connect(&settings.db_path)?);
    let faq = Arc::new(faq::FaqStore::new(&settings.faq_path, &settings.knowledge_path));

    let graph_http = reqwest::Client::builder().timeout(Duration::from_secs(12)).build()?;
    let messenger = Arc::new(messenger::Messenger::new(settings.clone(), graph_http.clone()));
    let notifier = Arc::new(notify::Notifier::new(settings.clone(), graph_http));

    let llm = if settings.llm_enabled() {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(settings.llm_timeout_sec))
            .build()?;
        Some(Arc::new(llm::Llm::new(settings.clone(), http)))
    } else {
        None
    };
    let menu = Arc::new(menu::MenuCache::new(settings.clone()));

    // Load the Ollama model in the background so the first customer does not wait for it.
    if let Some(llm) = &llm {
        let llm = llm.clone();
        tokio::spawn(async move { llm.warm_up().await });
    }

    Ok(state::App { settings, db, faq, messenger, notifier, menu, llm, started: Instant::now() })
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}

async fn run(settings: Settings) -> Result<()> {
    let port = settings.port;
    let app = build_app(settings)?;

    // Hourly housekeeping of dedupe / rate-limit tables.
    let db = app.db.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            if let Err(err) = db.prune() {
                error!(error = %err, "db prune failed");
            }
        }
    });

    let listener = TcpListener::bind(("0.0.0.0", port)).await.with_context(|| format!("binding port {port}"))?;
    info!(port, "bot started, listening");
    axum::serve(listener, routes::router(app))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")
}

#[tokio::main]
async fn main() {
    let settings = Settings::from_env();
    init_logging(&settings);

    if std::env::args().any(|a| a == "--health-check") {
        let code = match health_check(&settings).await {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("health check failed: {err}");
                1
            }
        };
        std::process::exit(code);
    }

    settings.warn_missing_config();
    if let Err(err) = run(settings).await {
        error!(error = %err, "bot failed to start");
        std::process::exit(1);
    }
}
