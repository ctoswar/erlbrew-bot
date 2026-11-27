//! FastAPI route handlers for the Erlbrew Café bot.
//! All public functions swallow errors and return appropriate HTTP responses.

use axum::{
    extract::{Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, PlainTextResponse},
    Json,
};
use serde::Deserialize;
use tracing::{error, info, warn};

use crate::{
    config::Settings,
    db::Db,
    faq::FaqStore,
    llm::generate,
    messenger::Messenger,
    pipeline::process_message,
    state::App,
};
use hyper::body::Bytes;

/// Query params for webhook verification
#[derive(Deserialize)]
struct WebhookVerifyParams {
    #[serde(alias = "hub.mode")]
    hub_mode: String,
    #[serde(alias = "hub.verify_token")]
    hub_verify_token: String,
    #[serde(alias = "hub.challenge")]
    hub_challenge: String,
}

/// POST /webhook body (simplified; full payload is a dict parsed from JSON)
type WebhookBody = serde_json::Value;

/// GET /health
pub async fn health(State(app): State<App>) -> Json<serde_json::Value> {
    let db_connected = app.db.connected();
    Json(json!({
        "ok": true,
        "uptime_s": app.uptime_secs(),
        "db": db_connected,
        "model": app.settings.ollama_model,
    }))
}

/// GET /webhook — Meta verification
pub async fn webhook_verify(
    Query(params): Query<WebhookVerifyParams>,
    State(app): State<App>,
) -> Result<PlainTextResponse, StatusCode> {
    let expected = &app.settings.verify_token;
    if params.hub_mode == "subscribe"
        && !expected.is_empty()
        && hmac::hmac_compare(
            params.hub_verify_token.as_bytes(),
            expected.as_bytes(),
        )
    {
        Ok(PlainTextResponse(params.hub_challenge))
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

/// POST /webhook — receive a webhook event from Meta
pub async fn webhook_receive(
    State(app): State<App>,
    request: Request,
) -> Result<impl IntoResponse, StatusCode> {
    let body = request.into_body().collect().await.map_err(|_| StatusCode::BAD_REQUEST)?;
    let body_bytes = Bytes::from(body.to_bytes().unwrap_or_default());

    // Signature check
    let signature = request
        .headers()
        .get("X-Hub-Signature-256")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    if !crate::routes::_signature_ok(&body_bytes, &signature, &app.settings.app_secret) {
        warn!("webhook rejected: bad signature");
        return Ok((StatusCode::FORBIDDEN, "Invalid signature"));
    }

    // Parse JSON
    let payload: WebhookBody = match serde_json::from_slice(&body_bytes) {
        Ok(p) => p,
        Err(_) => {
            warn!("webhook: malformed JSON ignored (200 returned to Meta)");
            return Ok(PlainTextResponse("EVENT_RECEIVED"));
        }
    };

    if payload.get("object") != Some(&serde_json::Value::String("page".into())) {
        return Ok(PlainTextResponse("EVENT_RECEIVED"));
    }

    if let Some(entries) = payload.get("entry").and_then(|e| e.as_array()) {
        for entry in entries {
            if let Some(messaging) = entry.get("messaging").and_then(|m| m.as_array()) {
                for event in messaging {
                    // Fire-and-forget: spawn a task so the 200 response returns immediately
                    let app_clone = app.clone();
                    tokio::spawn(async move {
                        if let Err(err) = process_event(
                            &app_clone,
                            entry,
                            &app_clone.db,
                            &app_clone.messenger,
                            &app_clone.faq,
                            &mut app_clone.llm_client.clone(),
                        )
                        .await
                        {
                            error!(psid = ?event.get("sender").and_then(|s| s.get("id")).unwrap_or_default(), error = %err, "event processing failed");
                        }
                    });
                }
            }
        }
    }

    Ok(PlainTextResponse("EVENT_RECEIVED"))
}

/// GET /admin/unpause/{psid}
pub async fn admin_unpause(
    State(app): State<App>,
    Path(psid): String,
    Query(query): Query<axum::extract::Path<String>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let token = headers
        .get("X-Admin-Token")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();

    if !crate::routes::_admin_auth(token, &app.settings.admin_token) {
        return Err(StatusCode::FORBIDDEN);
    }

    let ok = app.db.unpause_user(&psid).await.unwrap_or(false);
    info!("admin unpause", extra = { "psid": psid, "updated": ok });
    Ok(Json(json!({ "ok": ok, "psid": psid })))
}

/// GET /admin/orders
pub async fn admin_orders(
    State(app): State<App>,
    headers: HeaderMap,
    Query(limit): Query<axum::extract::Path<usize>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let token = headers
        .get("X-Admin-Token")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();

    if !crate::routes::_admin_auth(token, &app.settings.admin_token) {
        return Err(StatusCode::FORBIDDEN);
    }

    let orders = app.db.list_orders(limit as i64).await.unwrap_or_default();
    Ok(Json(json!({ "ok": true, "orders": orders })))
}

/// GET /privacy
pub async fn privacy() -> Html<String> {
    // Try serving from data/privacy.html
    // For now, serve the static fallback
    Html(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Privacy Policy</title></head>",
    )
}

/// POST /admin/unpause (CLI-style)
pub async fn admin_unpause_cli(
    State(app): State<App>,
    query: Query<crate::state::UnpauseQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let ok = app.db.unpause_user(&query.psid).await.unwrap_or(false);
    info!("cli unpause", extra = { "psid": query.psid, "updated": ok });
    Ok(Json(json!({ "ok": ok, "psid": query.psid })))
}

/// Internal: signature verification
pub fn _signature_ok(body: &Bytes, provided: &str, app_secret: &str) -> bool {
    if app_secret.is_empty() {
        // In dev, if no secret set, still accept but log
        warn!("APP_SECRET not set: rejecting webhook POST (fail closed)");
        return false;
    }
    let expected = format!("sha256={}", hmac::hmac_sha256(app_secret.as_bytes(), body));
    let provided_stripped = provided.trim().to_lowercase();
    if !provided_stripped.starts_with("sha256=") {
        return false;
    }
    // Constant-time compare
    hmac::hmac_compare(expected.as_bytes(), provided_stripped.as_bytes())
}

/// Internal: admin token auth
fn _admin_auth(token: String, expected: &str) -> bool {
    if expected.is_empty() || token.is_empty() {
        return false;
    }
    hmac::hmac_compare(token.as_bytes(), expected.as_bytes())
}

/// Process a single event entry from the webhook payload.
async fn process_event(
    app: &App,
    event: &serde_json::Value,
    db: &Db,
    messenger: &Messenger,
    faq: &FaqStore,
    llm_client: &Option<reqwest::Client>,
) -> anyhow::Result<()> {
    let message = event.get("message").and_then(|m| m.as_object());
    let is_echo = message
        .and_then(|m| m.get("is_echo"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let psid = if is_echo {
        event
            .get("recipient")
            .and_then(|r| r.get("id"))
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    } else {
        event
            .get("sender")
            .and_then(|s| s.get("id"))
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };

    if psid.is_empty() {
        return Ok(());
    }

    // Dedup by mid
    let mid = message
        .and_then(|m| m.get("mid"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !mid.is_empty() && db.mark_message_seen(&mid).await? == false {
        // Duplicate, silently ignore
        return Ok(());
    }

    // Run the pipeline
    if let Some(text) = event
        .get("message")
        .and_then(|m| m.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
    {
        if let Some(reply) = process_message(
            &psid,
            &serde_json::Value::Object(
                serde_json::json!({
                    "message": {
                        "is_echo": is_echo,
                        "text": text,
                        "mid": mid,
                    }
                })
            ),
            db,
            messenger,
            faq,
            llm_client,
        )
        .await
        .ok()
        {
            // Reply sent inside process_message; we just return
        }
    } else if let Some(postback) = event.get("postback").and_then(|p| p.as_object()) {
        let payload = postback
            .get("payload")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        // Route payload through the pipeline
        let _ = route_payload(messenger, db, faq, &psid, &payload).await;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// OLD ENDPOINTS kept for compatibility (admin/orders, etc.)
// ---------------------------------------------------------------------------