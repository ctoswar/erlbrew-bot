//! Shared application state handed to every request handler.

use crate::config::Settings;
use crate::db::Db;
use crate::faq::FaqStore;
use crate::forms::Reply;
use crate::llm::Llm;
use crate::menu::MenuCache;
use crate::messenger::Messenger;
use crate::notify::Notifier;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Instant;
use tracing::error;

#[derive(Clone)]
pub struct App {
    pub settings: Arc<Settings>,
    pub db: Arc<Db>,
    pub faq: Arc<FaqStore>,
    pub messenger: Arc<Messenger>,
    pub notifier: Arc<Notifier>,
    pub menu: Arc<MenuCache>,
    pub llm: Option<Arc<Llm>>,
    pub started: Instant,
    /// Rotates the closing lines so the same acknowledgement is not answered identically.
    pub ack_counter: Arc<AtomicUsize>,
}

impl App {
    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Send a reply; delivery problems are logged but never abort the pipeline.
    pub async fn send(&self, psid: &str, reply: &Reply) {
        if reply.text.is_empty() {
            return;
        }
        if let Err(err) = self.messenger.send_reply(psid, reply).await {
            error!(error = %err, "failed to send reply");
        }
    }
}

#[cfg(test)]
impl App {
    /// Fully in-memory app for tests: no network config, no LLM, no POS menu.
    pub fn for_test() -> Self {
        Self::for_test_with(Settings::for_test())
    }

    /// Same, but with custom settings.
    pub fn for_test_with(settings: Settings) -> Self {
        let settings = Arc::new(settings);
        let db = Arc::new(Db::connect(":memory:").unwrap());
        let http = reqwest::Client::new();
        App {
            faq: Arc::new(FaqStore::new(&settings.faq_path, &settings.knowledge_path)),
            messenger: Arc::new(Messenger::new(settings.clone(), http.clone())),
            notifier: Arc::new(Notifier::new(settings.clone(), http)),
            menu: Arc::new(MenuCache::new(settings.clone())),
            llm: None,
            started: Instant::now(),
            ack_counter: Arc::new(AtomicUsize::new(0)),
            settings,
            db,
        }
    }
}
