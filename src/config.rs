//! Runtime configuration (environment variables) and shared constants.

use regex::Regex;
use std::env;
use std::str::FromStr;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Canned replies
// ---------------------------------------------------------------------------

pub const RATE_LIMITED_TEXT: &str = "Sandali lang po 😊 Medyo marami na po ang messages natin. \
Subukan po ulit after ilang minuto, o hintayin po ang reply ng team namin. ☕";

pub const TOO_LONG_TEXT: &str = "Pasensya po, masyadong mahaba ang message (max 500 characters). \
Pwede po bang paikliin o hatiin sa dalawa? 😊";

pub const NON_TEXT_TEXT: &str = "Salamat po! Sa ngayon po text lang ang naiintindihan ko 😊 \
Paki-type na lang po ang tanong o order ninyo.";

pub const HANDOFF_TEXT: &str = "Sige po, ipapasa ko na po ito sa team namin 🙏 \
Sasagot po sila dito sa chat as soon as possible. Salamat po sa pasensya! ☕";

pub const LLM_FALLBACK_TEXT: &str = "Pasensya po, hindi ko po sigurado ang sagot dito 😅 \
Ipapaalam ko po sa team at sasagot po sila dito sa chat. Pwede rin po kayong mag-type ng 'order' \
para mag-order, o tap ang menu para sa FAQs.";

pub const HELP_TEXT: &str = "Hello po! ☕ Ako ang chat assistant ng Erlbrew Café. \
Pwede po akong sumagot ng tanong, tumanggap ng order (type 'order'), o mag-book ng event.";

// ---------------------------------------------------------------------------
// Intent regexes
// ---------------------------------------------------------------------------

/// Words that hand the conversation over to a human (complaints, refunds, ...).
pub static HANDOFF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(complaints?|complains?|reklamo|refunds?|allerg(?:y|ies|ic)|bulk\s+orders?|human|staff|tao)\b",
    )
    .expect("handoff regex")
});

/// Phrases that start the order intake form (checked AFTER handoff).
pub static ORDER_INTENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(orders?|mag-?order|umorder|um-order|pa-?order|pabili|pa-?deliver)\b")
        .expect("order intent regex")
});

/// Phrases that start the event booking form.
pub static EVENT_INTENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(mag-?book|book(?:ing)?\s+(?:an?\s+|ng\s+)?event|event\s+booking|reserve)\b")
        .expect("event intent regex")
});

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Settings {
    pub port: u16,
    pub log_level: String,
    pub db_path: String,
    pub faq_path: String,
    pub knowledge_path: String,
    pub privacy_path: String,

    pub verify_token: String,
    pub app_secret: String,
    pub admin_token: String,
    pub page_access_token: String,
    pub graph_api_version: String,

    pub ollama_url: String,
    pub ollama_model: String,
    pub llm_timeout_sec: u64,

    pub telegram_bot_token: String,
    pub telegram_chat_id: String,

    /// e.g. mysql://user:pass@host:3306/erlbrew_pos  (empty = no live menu)
    pub pos_mysql_url: String,
    /// Optional override; must return: id, name, category, price, description, popular
    pub pos_menu_query: String,
    pub menu_cache_ttl_sec: u64,

    pub rate_limit_window_sec: i64,
    pub rate_limit_max: i64,
    pub max_text_chars: usize,
    pub event_reply_hours: i64,
}

fn env_str_any(keys: &[&str], default: &str) -> String {
    keys.iter()
        .find_map(|key| {
            env::var(key)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| default.to_string())
}

fn env_parse_any<T: FromStr>(keys: &[&str], default: T) -> T {
    keys.iter()
        .find_map(|key| env::var(key).ok().and_then(|v| v.trim().parse::<T>().ok()))
        .unwrap_or(default)
}

fn env_str(key: &str, default: &str) -> String {
    env_str_any(&[key], default)
}

fn env_parse<T: FromStr>(key: &str, default: T) -> T {
    env_parse_any(&[key], default)
}

pub const DEFAULT_MENU_QUERY: &str =
    "SELECT id, name, category, price, description, popular FROM menu_items";

impl Settings {
    pub fn from_env() -> Self {
        Self {
            port: env_parse("PORT", 8000),
            log_level: env_str("LOG_LEVEL", "info"),
            db_path: env_str("DB_PATH", "data/bot.db"),
            faq_path: env_str("FAQ_PATH", "faq.yaml"),
            knowledge_path: env_str("KNOWLEDGE_PATH", "knowledge.md"),
            privacy_path: env_str("PRIVACY_PATH", "data/privacy.html"),

            verify_token: env_str("VERIFY_TOKEN", ""),
            app_secret: env_str("APP_SECRET", ""),
            admin_token: env_str("ADMIN_TOKEN", ""),
            page_access_token: env_str("PAGE_ACCESS_TOKEN", ""),
            graph_api_version: env_str_any(&["GRAPH_API_VERSION", "GRAPH_VERSION"], "v21.0"),

            ollama_url: env_str_any(&["OLLAMA_URL", "OLLAMA_BASE_URL"], "http://127.0.0.1:11434"),
            ollama_model: env_str("OLLAMA_MODEL", "qwen2.5:3b"),
            llm_timeout_sec: env_parse("LLM_TIMEOUT_SEC", 25),

            telegram_bot_token: env_str("TELEGRAM_BOT_TOKEN", ""),
            telegram_chat_id: env_str("TELEGRAM_CHAT_ID", ""),

            pos_mysql_url: env_str("POS_MYSQL_URL", ""),
            pos_menu_query: env_str("POS_MENU_QUERY", DEFAULT_MENU_QUERY),
            menu_cache_ttl_sec: env_parse("MENU_CACHE_TTL_SEC", 60),

            rate_limit_window_sec: env_parse("RATE_LIMIT_WINDOW_SEC", 600),
            rate_limit_max: env_parse("RATE_LIMIT_MAX", 15),
            max_text_chars: env_parse("MAX_TEXT_CHARS", 500),
            event_reply_hours: env_parse("EVENT_REPLY_HOURS", 24),
        }
    }

    /// The LLM is disabled when the URL is empty or the dead-port sentinel used in tests.
    pub fn llm_enabled(&self) -> bool {
        !self.ollama_url.is_empty() && self.ollama_url != "http://127.0.0.1:9"
    }

    pub fn warn_missing_config(&self) {
        let checks = [
            ("PAGE_ACCESS_TOKEN", self.page_access_token.is_empty(), "cannot send Messenger replies"),
            ("VERIFY_TOKEN", self.verify_token.is_empty(), "webhook verification will fail"),
            ("APP_SECRET", self.app_secret.is_empty(), "all webhook POSTs will be rejected"),
            ("ADMIN_TOKEN", self.admin_token.is_empty(), "admin endpoints are disabled"),
            ("TELEGRAM_BOT_TOKEN", self.telegram_bot_token.is_empty(), "team notifications are off"),
            ("TELEGRAM_CHAT_ID", self.telegram_chat_id.is_empty(), "team notifications are off"),
            ("POS_MYSQL_URL", self.pos_mysql_url.is_empty(), "orders get no live prices"),
        ];
        for (name, missing, effect) in checks {
            if missing {
                tracing::warn!("{name} is not set: {effect}");
            }
        }
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        let mut s = Self::from_env();
        s.db_path = ":memory:".into();
        s.faq_path = "does-not-exist.yaml".into();
        s.knowledge_path = "does-not-exist.md".into();
        s.ollama_url = "http://127.0.0.1:9".into();
        s.pos_mysql_url = String::new();
        s.app_secret = "test-secret".into();
        s.admin_token = "test-admin".into();
        s.verify_token = "test-verify".into();
        s
    }
}
