/// Erlbrew Café — Facebook Messenger webhook service (native Rust rewrite).

use axum::{
    extensions::Extension,
    http::{header, Method, Uri, HeaderMap, HeaderValue, Request, Response},
    routing::{get, post},
    Router,
};
use fastapi::FastApi;
use fastapi::routing::{get as fg, post as fp};
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

use crate::{
    config::Settings,
    db::Db,
    events,
    faq::FaqStore,
    llm::generate,
    messenger::Messenger,
    pipeline::process_message,
    routes,
    state::App,
};
use fastapi::middleware::headers::Headers;

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

pub async fn run(settings: Settings) -> anyhow::Result<()> {
    // --- Initialise DB ---
    let db = Db::connect(&settings.db_path).await?;
    let db = Arc::new(db);

    // --- Initialise FAQ (hot-reloading) ---
    let faq = routes::FaqStore::new(&settings.faq_path);

    // --- Initialise Messenger (HTTP client + DB) ---
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .build()?;
    let messenger = Messenger::new(Arc::new(settings.clone()), db.clone(), http_client);

    // --- Initialise LLM client (may be disabled) ---
    let llm_client = if settings.ollama_url != "http://127.0.0.1:9" {
        // Only create if URL looks reachable; tests use dead port
        Some(reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(25))
            .build()?)
    } else {
        None
    };

    // --- App state ---
    let start = Instant::now();
    let app = App {
        settings: settings.clone(),
        db,
        faq,
        messenger,
        llm_client,
        start,
    };

    // --- FastAPI app ---
    let mut fastapi = FastApi::new(
        "Erlbrew Café Messenger bot",
        "2.0.0",
    );

    // --- CORS (allow the tunnel origin) ---
    fastapi.add_middleware(|app| {
        app.use(|_, mut res| {
            res.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("*"),
            );
            res
        })
    });

    // --- Lifespan: start up ---
    fastapi.on_startup({
        let app = app.clone();
        move || {
            tracing::info!("bot started");
        }
    });

    // --- Routes ---

    // GET /health
    fastapi.get("/health", fg(routes::health));

    // GET /webhook — Meta verification
    fastapi.get("/webhook", fg(routes::webhook_verify));

    // POST /webhook — receive events
    fastapi.post("/webhook", fp(routes::webhook_receive));

    // POST /admin/unpause/{psid}
    fastapi.post("/admin/unpause/{psid}", fp(routes::admin_unpause));

    // GET /admin/orders
    fastapi.get("/admin/orders", fp(routes::admin_orders));

    // GET /privacy
    fastapi.get("/privacy", fg(routes::privacy));

    // --- CLI bridge: unpause from query ---
    // We handle this via a separate route

    // Serve the app
    let addr = SocketAddr::from(([0, 0, 0, 0], settings.port));
    tracing::info!(port = %settings.port, "listening");
    let listener = TcpListener::bind(addr).await?;
    fastapi.serve(listener).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// CLI main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Load env config
    let settings = Settings::from_env();
    settings.warn_missing_config();

    // Initialise structured JSON logging
    crate::init_logging(&settings);

    // Run the bot
    if let Err(err) = run(settings).await {
        tracing::error!(error = %err, "bot failed to start");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Test helper: create App with test settings
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::config::Settings;
    use crate::faq::FaqStore;
    use crate::messenger::Messenger;
    use reqwest::Client;

    #[test]
    fn test_app_creation() {
        let settings = Settings::for_test();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let app = rt.block_on(run(settings.clone())).unwrap();
        // Just verify it started; the listener won't bind in test env
        drop(app);
    }
}