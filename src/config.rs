//! Central configuration, loaded from environment variables.
//!
//! Variable names and defaults intentionally match the original Python bot so
//! an existing `.env` / Docker stack keeps working unchanged.

use std::env;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Settings {
    // Meta / Facebook
    pub app_secret: String,
    pub verify_token: String,
    pub page_access_token: String,
    pub graph_version: String,
    // Admin
    pub admin_token: String,
    // Ollama LLM
    pub ollama_url: String,
    pub ollama_model: String,
    pub ollama_think: bool,
    pub ollama_num_predict: i64,
    pub ollama_num_ctx: i64,
    pub ollama_temperature: f64,
    pub ollama_keep_alive: String,
    pub llm_timeout_sec: f64,
    pub llm_max_history: i64,
    pub llm_history_ttl_sec: i64,
    pub typing_resend_sec: f64,
    // Telegram
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    // POS menu (live prices during order intake)
    pub pos_mysql_url: String,
    pub menu_cache_ttl_sec: i64,
    // Behaviour
    pub event_reply_hours: i64,
    pub db_path: String,
    pub faq_path: String,
    pub knowledge_path: String,
    pub rate_limit_max: i64,
    pub rate_limit_window_sec: i64,
    pub max_text_chars: i64,
    pub event_form_ttl_sec: i64,
    pub cleanup_interval_sec: i64,
    pub port: u16,
    pub log_level: String,
    // Endpoint bases (overridable for tests / private deployments).
    pub graph_base_url: String,
    pub telegram_api_base: String,
}

fn get_str(name: &str, default: &str) -> String {
    match env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => raw.trim().to_string(),
        _ => default.to_string(),
    }
}

fn get_int(name: &str, default: i64) -> i64 {
    match env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => raw.trim().parse().unwrap_or_else(|_| {
            tracing::warn!(variable = name, default, "bad integer value; using default");
            default
        }),
        _ => default,
    }
}

fn get_float(name: &str, default: f64) -> f64 {
    match env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => raw.trim().parse().unwrap_or_else(|_| {
            tracing::warn!(variable = name, default, "bad float value; using default");
            default
        }),
        _ => default,
    }
}

fn get_bool(name: &str, default: bool) -> bool {
    match env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on" | "y"
        ),
        _ => default,
    }
}

/// Prefer an explicit env var, then the first existing candidate path.
///
/// This lets Docker mount `./data` (live-editable files) while still falling
/// back to the bundled copies inside the image.
fn resolve_path(env_name: &str, candidates: &[&str]) -> String {
    let given = get_str(env_name, "");
    if !given.is_empty() {
        return given;
    }
    for cand in candidates {
        let p = Path::new(cand);
        if p.is_absolute() && p.exists() {
            return (*cand).to_string();
        }
    }
    for cand in candidates {
        let p = PathBuf::from(cand);
        if p.exists() {
            return p.to_string_lossy().into_owned();
        }
    }
    candidates[0].to_string()
}

impl Settings {
    pub fn from_env() -> Self {
        let ollama_url = {
            let canonical = get_str("OLLAMA_BASE_URL", "");
            let value = if canonical.is_empty() {
                get_str("OLLAMA_URL", "http://host.docker.internal:11434")
            } else {
                canonical
            };
            value.trim_end_matches('/').to_string()
        };

        Self {
            app_secret: get_str("APP_SECRET", ""),
            verify_token: get_str("VERIFY_TOKEN", ""),
            page_access_token: get_str("PAGE_ACCESS_TOKEN", ""),
            graph_version: get_str("GRAPH_VERSION", "v23.0"),
            admin_token: get_str("ADMIN_TOKEN", ""),
            ollama_url,
            ollama_model: get_str("OLLAMA_MODEL", "qwen3.5:2b"),
            ollama_think: get_bool("OLLAMA_THINK", false),
            ollama_num_predict: get_int("OLLAMA_NUM_PREDICT", 120),
            ollama_num_ctx: get_int("OLLAMA_NUM_CTX", 2048),
            ollama_temperature: get_float("OLLAMA_TEMPERATURE", 0.2),
            ollama_keep_alive: get_str("OLLAMA_KEEP_ALIVE", "30m"),
            llm_timeout_sec: get_float("LLM_TIMEOUT_SEC", 25.0),
            llm_max_history: get_int("LLM_MAX_HISTORY", 6),
            llm_history_ttl_sec: get_int("LLM_HISTORY_TTL_HOURS", 24) * 3600,
            typing_resend_sec: get_float("TYPING_RESEND_SEC", 10.0),
            telegram_bot_token: get_str("TELEGRAM_BOT_TOKEN", ""),
            telegram_chat_id: get_str("TELEGRAM_CHAT_ID", ""),
            pos_mysql_url: get_str("POS_MYSQL_URL", ""),
            menu_cache_ttl_sec: get_int("MENU_CACHE_TTL_SEC", 60),
            event_reply_hours: get_int("EVENT_REPLY_HOURS", 24),
            db_path: get_str("DB_PATH", "data/erlbrew.db"),
            faq_path: resolve_path("FAQ_PATH", &["data/faq.yaml", "faq.yaml"]),
            knowledge_path: resolve_path("KNOWLEDGE_PATH", &["data/knowledge.md", "knowledge.md"]),
            rate_limit_max: get_int("RATE_LIMIT_MAX", 15),
            rate_limit_window_sec: get_int("RATE_LIMIT_WINDOW_SEC", 600),
            max_text_chars: get_int("MAX_TEXT_CHARS", 500),
            event_form_ttl_sec: get_int("EVENT_FORM_TTL_HOURS", 6) * 3600,
            cleanup_interval_sec: get_int("CLEANUP_INTERVAL_SEC", 900),
            port: get_int("PORT", 8000).clamp(1, 65535) as u16,
            log_level: get_str("LOG_LEVEL", "INFO").to_uppercase(),
            graph_base_url: get_str("GRAPH_BASE_URL", "https://graph.facebook.com"),
            telegram_api_base: get_str("TELEGRAM_API_BASE", "https://api.telegram.org"),
        }
    }

    /// Deterministic defaults for tests and local experiments.
    pub fn for_test() -> Self {
        Self {
            app_secret: "test_app_secret_abc123".into(),
            verify_token: "test_verify_token".into(),
            page_access_token: "test_page_token".into(),
            graph_version: "v23.0".into(),
            admin_token: "test_admin_token".into(),
            ollama_url: "http://127.0.0.1:9".into(),
            ollama_model: "test-model:2b".into(),
            ollama_think: false,
            ollama_num_predict: 120,
            ollama_num_ctx: 2048,
            ollama_temperature: 0.2,
            ollama_keep_alive: "30m".into(),
            llm_timeout_sec: 25.0,
            llm_max_history: 6,
            llm_history_ttl_sec: 86400,
            typing_resend_sec: 10.0,
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
            pos_mysql_url: String::new(),
            menu_cache_ttl_sec: 60,
            event_reply_hours: 24,
            db_path: "data/erlbrew.db".into(),
            faq_path: "data/faq.yaml".into(),
            knowledge_path: "data/knowledge.md".into(),
            rate_limit_max: 15,
            rate_limit_window_sec: 600,
            max_text_chars: 500,
            event_form_ttl_sec: 6 * 3600,
            cleanup_interval_sec: 900,
            port: 8000,
            log_level: "WARNING".into(),
            graph_base_url: "https://graph.facebook.com".into(),
            telegram_api_base: "https://api.telegram.org".into(),
        }
    }

    /// Log a warning for every required value that is missing.
    pub fn warn_missing_config(&self) {
        let mut missing = Vec::new();
        if self.app_secret.is_empty() {
            missing.push("APP_SECRET");
        }
        if self.verify_token.is_empty() {
            missing.push("VERIFY_TOKEN");
        }
        if self.page_access_token.is_empty() {
            missing.push("PAGE_ACCESS_TOKEN");
        }
        if !missing.is_empty() {
            tracing::error!(
                missing = %missing.join(", "),
                "missing required config; webhook POSTs will be rejected"
            );
        }
        if self.admin_token.is_empty() {
            tracing::error!("ADMIN_TOKEN not set; /admin/unpause rejected");
        }
        if self.telegram_bot_token.is_empty() || self.telegram_chat_id.is_empty() {
            tracing::warn!("Telegram notifications disabled (not configured)");
        }
    }
}

// ---------------------------------------------------------------------------
// Canned messages (friendly Taglish brand voice)
// ---------------------------------------------------------------------------
pub const RATE_LIMITED_TEXT: &str = "Pasensya na po, sobrang dami po ng messages ninyo 😅 \
Sandali lang po ulit, salamat po sa pag-intindi! ☕";

pub const NON_TEXT_TEXT: &str = "Salamat po! Pero yung tinatanong ko po, pwede nyo po bang \
i-type as text? I-type nyo lang po yung tanong nyo dito para masagot ko agad. 😊";

pub const HANDOFF_TEXT: &str = "Sige po, ipapasa ko po ito sa aming team para matulungan kayo \
agad. 🙂 I-message po kayo nila dito mismo sa Messenger. Salamat po sa pasensya!";

pub const LLM_FALLBACK_TEXT: &str = "Sandali lang po, ipapasa ko sa team namin.";

pub const RATE_NOTICE_KIND: &str = "rate";
pub const LEN_NOTICE_KIND: &str = "toolong";

pub fn too_long_text(max_chars: i64) -> String {
    format!(
        "Mukhang medyo mahaba po yung message na yan. Paki-shorten naman po \
(max {max_chars} characters) para masagot ko agad. Salamat po! 😊"
    )
}
