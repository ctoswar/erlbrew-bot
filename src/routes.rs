//! HTTP routes (axum) for the Erlbrew Café Messenger bot.

use crate::pipeline::{self, Incoming};
use crate::state::App;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn};

pub fn router(app: App) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/webhook", get(webhook_verify).post(webhook_receive))
        .route("/admin/unpause/:psid", post(admin_unpause))
        .route("/admin/orders", get(admin_orders))
        .route("/privacy", get(privacy))
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any))
        .with_state(app)
}

// ---------------------------------------------------------------------------
// Security helpers
// ---------------------------------------------------------------------------

fn secure_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Verify Meta's `X-Hub-Signature-256: sha256=<hex hmac of the raw body>`. Fails closed.
pub fn signature_ok(body: &[u8], header: &str, app_secret: &str) -> bool {
    if app_secret.is_empty() {
        warn!("APP_SECRET not set: rejecting webhook POST (fail closed)");
        return false;
    }
    let header = header.trim().to_lowercase();
    let Some(sig_hex) = header.strip_prefix("sha256=") else { return false };
    let Ok(sig) = hex::decode(sig_hex) else { return false };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(app_secret.as_bytes()) else { return false };
    mac.update(body);
    mac.verify_slice(&sig).is_ok() // constant-time
}

fn admin_ok(headers: &HeaderMap, expected: &str) -> bool {
    let token = headers.get("X-Admin-Token").and_then(|h| h.to_str().ok()).unwrap_or("");
    !expected.is_empty() && !token.is_empty() && secure_eq(token, expected)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /health
async fn health(State(app): State<App>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "uptime_s": app.uptime_secs(),
        "db": app.db.connected(),
        "model": app.settings.ollama_model,
    }))
}

#[derive(Deserialize)]
struct VerifyParams {
    #[serde(rename = "hub.mode")]
    mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    token: Option<String>,
    #[serde(rename = "hub.challenge")]
    challenge: Option<String>,
}

/// GET /webhook — Meta verification handshake.
async fn webhook_verify(State(app): State<App>, Query(p): Query<VerifyParams>) -> Response {
    let expected = &app.settings.verify_token;
    let token_ok = p.token.as_deref().is_some_and(|t| !expected.is_empty() && secure_eq(t, expected));
    match (p.mode.as_deref(), token_ok, p.challenge) {
        (Some("subscribe"), true, Some(challenge)) => challenge.into_response(),
        _ => StatusCode::FORBIDDEN.into_response(),
    }
}

/// Turn one `entry[].messaging[]` item into an [`Incoming`]; None for delivery/read receipts.
pub fn incoming_from_event(ev: &Value) -> Option<Incoming> {
    let id_of = |key: &str| {
        ev.get(key).and_then(|v| v.get("id")).and_then(Value::as_str).unwrap_or("").trim().to_string()
    };
    let str_at = |ptr: &str| ev.pointer(ptr).and_then(Value::as_str).unwrap_or("").trim().to_string();

    if ev.get("message").is_some() {
        let is_echo = ev.pointer("/message/is_echo").and_then(Value::as_bool).unwrap_or(false);
        // For echoes the page is the sender and the customer is the recipient.
        let psid = if is_echo { id_of("recipient") } else { id_of("sender") };
        if psid.is_empty() {
            return None;
        }
        return Some(Incoming {
            psid,
            mid: str_at("/message/mid"),
            text: str_at("/message/text"),
            payload: str_at("/message/quick_reply/payload"),
            is_echo,
        });
    }
    if ev.get("postback").is_some() {
        let psid = id_of("sender");
        if psid.is_empty() {
            return None;
        }
        return Some(Incoming {
            psid,
            mid: str_at("/postback/mid"),
            text: String::new(),
            payload: str_at("/postback/payload"),
            is_echo: false,
        });
    }
    None
}

/// POST /webhook — receive events from Meta. Always answers 200 quickly; work runs in the
/// background (events of one delivery are processed in order).
async fn webhook_receive(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    let signature = headers.get("X-Hub-Signature-256").and_then(|h| h.to_str().ok()).unwrap_or("");
    if !signature_ok(&body, signature, &app.settings.app_secret) {
        warn!("webhook rejected: bad signature");
        return (StatusCode::FORBIDDEN, "Invalid signature").into_response();
    }

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            warn!("webhook: malformed JSON ignored (200 returned to Meta)");
            return "EVENT_RECEIVED".into_response();
        }
    };
    if payload.get("object").and_then(Value::as_str) != Some("page") {
        return "EVENT_RECEIVED".into_response();
    }

    let incoming: Vec<Incoming> = payload
        .get("entry")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("messaging").and_then(Value::as_array))
        .flatten()
        .filter_map(incoming_from_event)
        .collect();

    if !incoming.is_empty() {
        tokio::spawn(async move {
            for inc in incoming {
                let psid = inc.psid.clone();
                if let Err(err) = pipeline::process(&app, inc).await {
                    error!(psid = %psid, error = %err, "event processing failed");
                }
            }
        });
    }
    "EVENT_RECEIVED".into_response()
}

/// POST /admin/unpause/:psid
async fn admin_unpause(State(app): State<App>, Path(psid): Path<String>, headers: HeaderMap) -> Response {
    if !admin_ok(&headers, &app.settings.admin_token) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let updated = app.db.unpause_user(&psid).unwrap_or(false);
    info!(psid = %psid, updated, "admin unpause");
    Json(json!({ "ok": updated, "psid": psid })).into_response()
}

#[derive(Deserialize)]
struct LimitParams {
    limit: Option<i64>,
}

/// GET /admin/orders?limit=50
async fn admin_orders(State(app): State<App>, headers: HeaderMap, Query(q): Query<LimitParams>) -> Response {
    if !admin_ok(&headers, &app.settings.admin_token) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let orders = app.db.list_orders(limit).unwrap_or_default();
    Json(json!({ "ok": true, "orders": orders })).into_response()
}

/// GET /privacy — serves PRIVACY_PATH (default data/privacy.html), else a minimal page.
async fn privacy(State(app): State<App>) -> impl IntoResponse {
    let html = tokio::fs::read_to_string(&app.settings.privacy_path).await.unwrap_or_else(|_| {
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Privacy Policy</title></head>\
         <body><h1>Erlbrew Café — Privacy Policy</h1>\
         <p>We only use the messages you send to our Messenger page to answer your questions, \
         take your orders and bookings, and contact you about them. We do not sell your data. \
         To have your data removed, message us on our page.</p></body></html>"
            .to_string()
    });
    Html(html)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn sign(body: &[u8], secret: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn signatures() {
        let body = br#"{"object":"page"}"#;
        assert!(signature_ok(body, &sign(body, "s3cret"), "s3cret"));
        assert!(!signature_ok(body, &sign(body, "other"), "s3cret"));
        assert!(!signature_ok(body, "sha256=zz", "s3cret"));
        assert!(!signature_ok(body, &sign(body, ""), ""), "empty secret fails closed");
    }

    #[test]
    fn event_parsing() {
        let msg = json!({"sender":{"id":"U"},"recipient":{"id":"P"},"message":{"mid":"m1","text":" hi ","quick_reply":{"payload":"X"}}});
        let inc = incoming_from_event(&msg).unwrap();
        assert_eq!((inc.psid.as_str(), inc.text.as_str(), inc.payload.as_str(), inc.is_echo), ("U", "hi", "X", false));

        let echo = json!({"sender":{"id":"P"},"recipient":{"id":"U"},"message":{"mid":"m2","is_echo":true,"text":"yo"}});
        let inc = incoming_from_event(&echo).unwrap();
        assert_eq!((inc.psid.as_str(), inc.is_echo), ("U", true));

        let pb = json!({"sender":{"id":"U"},"postback":{"payload":"GET_STARTED"}});
        assert_eq!(incoming_from_event(&pb).unwrap().payload, "GET_STARTED");

        assert!(incoming_from_event(&json!({"sender":{"id":"U"},"delivery":{}})).is_none());
    }

    #[tokio::test]
    async fn http_endpoints() {
        let app = App::for_test();
        let router = router(app.clone());

        let call = |req: Request<Body>| {
            let router = router.clone();
            async move { router.oneshot(req).await.unwrap() }
        };

        let r = call(Request::get("/health").body(Body::empty()).unwrap()).await;
        assert_eq!(r.status(), StatusCode::OK);

        // Verification handshake
        let r = call(Request::get("/webhook?hub.mode=subscribe&hub.verify_token=test-verify&hub.challenge=123").body(Body::empty()).unwrap()).await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = call(Request::get("/webhook?hub.mode=subscribe&hub.verify_token=nope&hub.challenge=123").body(Body::empty()).unwrap()).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // Signed vs unsigned POST
        let body = r#"{"object":"page","entry":[{"messaging":[{"sender":{"id":"U1"},"message":{"mid":"w1","text":"order"}}]}]}"#;
        let r = call(Request::post("/webhook").body(Body::from(body)).unwrap()).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = call(Request::post("/webhook").header("X-Hub-Signature-256", sign(body.as_bytes(), "test-secret")).body(Body::from(body)).unwrap()).await;
        assert_eq!(r.status(), StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(app.db.get_form("U1").unwrap().unwrap().step, "order_items");

        // Admin auth
        let r = call(Request::get("/admin/orders").body(Body::empty()).unwrap()).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = call(Request::get("/admin/orders?limit=5").header("X-Admin-Token", "test-admin").body(Body::empty()).unwrap()).await;
        assert_eq!(r.status(), StatusCode::OK);
        app.db.set_paused("U1", true).unwrap();
        let r = call(Request::post("/admin/unpause/U1").header("X-Admin-Token", "test-admin").body(Body::empty()).unwrap()).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(!app.db.get_paused("U1").unwrap());

        let r = call(Request::get("/privacy").body(Body::empty()).unwrap()).await;
        assert_eq!(r.status(), StatusCode::OK);
    }
}
