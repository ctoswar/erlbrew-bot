//! Application state shared across axum routes.

use std::sync::Arc;
use std::time::Instant;

use crate::{config::Settings, db::Db, faq::FaqStore, messenger::Messenger};

/// Shared app state.
#[derive(Clone)]
pub struct App {
    pub settings: Settings,
    pub db: Db,
    pub faq: FaqStore,
    pub messenger: Messenger,
    /// LLM client may be None in test/config-off mode.
    pub llm_client: Option<reqwest::Client>,
    /// Start time for uptime calculations.
    pub start: Instant,
}

impl App {
    /// How many seconds since the bot started.
    pub fn uptime_secs(&self) -> u64 {
        self.start.elapsed().as_secs()
    }
}

// ---------------------------------------------------------------------------
// CLI helpers (unpause from command line / admin)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
pub struct UnpauseQuery {
    pub psid: String,
}