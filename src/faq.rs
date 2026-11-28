//! Hot-reloading FAQ (faq.yaml) and knowledge base (knowledge.md).
//!
//! Accepted faq.yaml shapes (top-level list, or a map with `faqs:` / `entries:`):
//!
//! ```yaml
//! - id: hours
//!   keywords: [hours, open, "anong oras"]   # whole-word match (<=3 chars) or word-prefix match
//!   regex: "(?i)\\bwifi\\b"                  # optional, string or list
//!   payload: FAQ_HOURS                       # optional: quick-reply / menu payload
//!   answer: "Bukas po kami 8AM-8PM."
//!   quick_replies:
//!     - { title: "Order 🛍️", payload: ORDER_START }
//! ```
//! An entry with `id: fallback_help` is used for unknown payloads.

use regex::Regex;
use serde::{Deserialize, Deserializer};
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Instant, SystemTime};
use tracing::{info, warn};

#[derive(Clone, Debug, Default, Deserialize)]
pub struct QuickReply {
    pub title: String,
    pub payload: String,
}

fn string_or_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum One {
        One(String),
        Many(Vec<String>),
    }
    Ok(match One::deserialize(d)? {
        One::One(s) => vec![s],
        One::Many(v) => v,
    })
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct FaqEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default, alias = "triggers", deserialize_with = "string_or_vec")]
    pub keywords: Vec<String>,
    #[serde(default, alias = "regex", alias = "regexes", deserialize_with = "string_or_vec")]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub payload: Option<String>,
    #[serde(default, alias = "reply", alias = "response")]
    pub answer: String,
    #[serde(default)]
    pub quick_replies: Vec<QuickReply>,
}

impl FaqEntry {
    /// Quick replies as (title, payload) pairs.
    pub fn quick_reply_pairs(&self) -> Vec<(String, String)> {
        self.quick_replies.iter().map(|q| (q.title.clone(), q.payload.clone())).collect()
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FaqFile {
    List(Vec<FaqEntry>),
    Map {
        #[serde(default, alias = "faqs", alias = "faq")]
        entries: Vec<FaqEntry>,
    },
}

struct Compiled {
    entry: FaqEntry,
    keywords: Vec<String>,
    regexes: Vec<Regex>,
}

impl Compiled {
    fn new(entry: FaqEntry) -> Self {
        let keywords = entry
            .keywords
            .iter()
            .map(|k| k.trim().to_lowercase())
            .filter(|k| !k.is_empty())
            .collect();
        let regexes = entry
            .patterns
            .iter()
            .filter_map(|p| match Regex::new(p) {
                Ok(r) => Some(r),
                Err(err) => {
                    warn!(id = %entry.id, error = %err, "invalid FAQ regex skipped");
                    None
                }
            })
            .collect();
        Self { entry, keywords, regexes }
    }

    fn matches(&self, low: &str, raw: &str) -> bool {
        self.keywords.iter().any(|k| contains_keyword(low, k))
            || self.regexes.iter().any(|r| r.is_match(raw))
    }
}

/// Short keywords (<= 3 chars, e.g. "hi") must match a whole word; longer ones match a word
/// prefix so "price" also catches "prices".
fn contains_keyword(low: &str, kw: &str) -> bool {
    let whole = kw.chars().count() <= 3;
    low.match_indices(kw).any(|(i, m)| {
        let before_ok = low[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let after_ok = !whole || low[i + m.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        before_ok && after_ok
    })
}

#[derive(Default)]
struct Inner {
    entries: Vec<Compiled>,
    knowledge: String,
    faq_mtime: Option<SystemTime>,
    knowledge_mtime: Option<SystemTime>,
    last_check: Option<Instant>,
}

pub struct FaqStore {
    faq_path: PathBuf,
    knowledge_path: PathBuf,
    inner: RwLock<Inner>,
}

fn mtime(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl FaqStore {
    pub fn new(faq_path: &str, knowledge_path: &str) -> Self {
        let store = Self {
            faq_path: faq_path.into(),
            knowledge_path: knowledge_path.into(),
            inner: RwLock::new(Inner::default()),
        };
        store.reload(true);
        store
    }

    /// Re-read files whose mtime changed (checked at most every 2 seconds).
    fn reload(&self, force: bool) {
        if !force {
            let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
            if guard.last_check.is_some_and(|t| t.elapsed().as_secs() < 2) {
                return;
            }
        }
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.last_check = Some(Instant::now());

        let faq_m = mtime(&self.faq_path);
        if force || faq_m != inner.faq_mtime {
            inner.faq_mtime = faq_m;
            match std::fs::read_to_string(&self.faq_path) {
                Ok(text) => match serde_yaml::from_str::<FaqFile>(&text) {
                    Ok(file) => {
                        let list = match file {
                            FaqFile::List(v) => v,
                            FaqFile::Map { entries } => entries,
                        };
                        info!(count = list.len(), path = %self.faq_path.display(), "FAQ loaded");
                        inner.entries = list.into_iter().map(Compiled::new).collect();
                    }
                    Err(err) => warn!(error = %err, "faq.yaml is invalid; keeping previous FAQ"),
                },
                Err(err) => warn!(error = %err, path = %self.faq_path.display(), "cannot read FAQ file"),
            }
        }

        let know_m = mtime(&self.knowledge_path);
        if force || know_m != inner.knowledge_mtime {
            inner.knowledge_mtime = know_m;
            if let Ok(text) = std::fs::read_to_string(&self.knowledge_path) {
                inner.knowledge = text;
            }
        }
    }

    /// First entry (file order) whose keywords/regex match the customer's text.
    pub fn match_text(&self, text: &str) -> Option<FaqEntry> {
        self.reload(false);
        let low = text.to_lowercase();
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner.entries.iter().find(|c| c.matches(&low, text)).map(|c| c.entry.clone())
    }

    pub fn match_payload(&self, payload: &str) -> Option<FaqEntry> {
        self.reload(false);
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner
            .entries
            .iter()
            .find(|c| c.entry.payload.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(payload)))
            .map(|c| c.entry.clone())
    }

    pub fn by_id(&self, id: &str) -> Option<FaqEntry> {
        self.reload(false);
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner.entries.iter().find(|c| c.entry.id == id).map(|c| c.entry.clone())
    }

    pub fn knowledge(&self) -> String {
        self.reload(false);
        self.inner.read().unwrap_or_else(|e| e.into_inner()).knowledge.clone()
    }

    #[cfg(test)]
    pub fn from_yaml(yaml: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("erlbrew-faq-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("faq.yaml");
        std::fs::write(&path, yaml).unwrap();
        Self::new(path.to_str().unwrap(), "does-not-exist.md")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = r#"
faqs:
  - id: hours
    keywords: [hours, "anong oras"]
    answer: "Bukas po kami 8AM-8PM."
    quick_replies:
      - { title: "Order", payload: ORDER_START }
  - id: greet
    keywords: hi
    answer: "Hello po!"
    payload: GET_STARTED
  - id: wifi
    regex: '(?i)\bwi-?fi\b'
    answer: "Password po: ask staff"
"#;

    #[test]
    fn matching() {
        let faq = FaqStore::from_yaml(YAML);
        assert_eq!(faq.match_text("What are your opening HOURS?").unwrap().id, "hours");
        assert_eq!(faq.match_text("hi po").unwrap().id, "greet");
        assert!(faq.match_text("hindi ko alam").is_none(), "'hi' must not match 'hindi'");
        assert_eq!(faq.match_text("may wifi ba?").unwrap().id, "wifi");
        assert_eq!(faq.match_payload("get_started").unwrap().id, "greet");
        assert_eq!(faq.by_id("hours").unwrap().quick_reply_pairs()[0].1, "ORDER_START");
    }
}
