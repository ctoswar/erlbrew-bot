//! FAQ content engine with live (mtime-based) reloading.
//!
//! `HotFile<T>` re-reads a file whenever its mtime changes, so `faq.yaml` and
//! `knowledge.md` stay owner-editable without a restart.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use regex::Regex;
use serde_yaml::Value;

#[derive(Debug)]
pub struct QRTile {
    pub title: String,
    pub payload: String,
}

#[derive(Debug)]
pub struct Entry {
    pub id: String,
    pub payloads: Vec<String>,
    pub patterns: Vec<String>,
    pub regexes: Vec<Regex>,
    pub keywords: Vec<Regex>,
    pub answer: String,
    pub quick_replies: Vec<QRTile>,
}

#[derive(Debug, Default)]
pub struct FaqIndex {
    pub by_id: HashMap<String, Arc<Entry>>,
    pub by_payload: HashMap<String, Arc<Entry>>,
    ordered: Vec<Arc<Entry>>,
}

impl FaqIndex {
    pub fn text_entry(&self, text: &str) -> Option<Arc<Entry>> {
        if text.is_empty() {
            return None;
        }
        let low = text.trim().to_lowercase();
        for entry in &self.ordered {
            if entry.patterns.iter().any(|p| low.contains(p)) {
                return Some(entry.clone());
            }
            if entry.regexes.iter().any(|r| r.is_match(&low)) {
                return Some(entry.clone());
            }
            if entry.keywords.iter().any(|r| r.is_match(&low)) {
                return Some(entry.clone());
            }
        }
        None
    }

    pub fn payload_entry(&self, payload: &str) -> Option<Arc<Entry>> {
        self.by_payload
            .get(&payload.trim().to_uppercase())
            .cloned()
    }

    pub fn entry_by_id(&self, id: &str) -> Option<Arc<Entry>> {
        self.by_id.get(id).cloned()
    }

    pub fn all_payloads(&self) -> Vec<String> {
        self.by_payload.keys().cloned().collect()
    }
}

/// Caches a value parsed from a file; reloads automatically on mtime change.
pub struct HotFile<T> {
    path: PathBuf,
    name: String,
    loader: Box<dyn Fn(&Path) -> Result<T, String> + Send + Sync>,
    state: Mutex<Option<(SystemTime, Arc<T>)>>,
}

impl<T: Send + Sync + 'static> HotFile<T> {
    pub fn new<F>(path: impl Into<PathBuf>, name: impl Into<String>, loader: F) -> Self
    where
        F: Fn(&Path) -> Result<T, String> + Send + Sync + 'static,
    {
        Self {
            path: path.into(),
            name: name.into(),
            loader: Box::new(loader),
            state: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the cached value, reloading when the file's mtime changed.
    ///
    /// A missing/unreadable file keeps the last good value; if there is no
    /// previous value the error is returned so the caller can decide.
    pub fn get(&self) -> Result<Arc<T>, String> {
        let mtime = fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .map_err(|e| e.to_string());

        let mut guard = self.state.lock().expect("hot file mutex poisoned");
        match mtime {
            Ok(mtime) => {
                let stale = match guard.as_ref() {
                    Some((cached_mtime, _)) => *cached_mtime != mtime,
                    None => true,
                };
                if stale {
                    match (self.loader)(&self.path) {
                        Ok(value) => {
                            let value = Arc::new(value);
                            *guard = Some((mtime, value.clone()));
                            tracing::info!(hot_file = %self.name, path = %self.path.display(), "hot file loaded");
                            return Ok(value);
                        }
                        Err(err) => {
                            tracing::error!(hot_file = %self.name, error = %err, "hot file reload failed");
                            if let Some((_, value)) = guard.as_ref() {
                                return Ok(value.clone());
                            }
                            return Err(err);
                        }
                    }
                }
                Ok(guard
                    .as_ref()
                    .expect("cached value missing")
                    .1
                    .clone())
            }
            Err(err) => {
                if let Some((_, value)) = guard.as_ref() {
                    return Ok(value.clone());
                }
                Err(format!("{}: {err}", self.path.display()))
            }
        }
    }

    /// Forget the cached mtime/value (used by tests and fresh pointers).
    pub fn clear(&self) {
        *self.state.lock().expect("hot file mutex poisoned") = None;
    }
}

fn as_str_list(value: Option<&Value>) -> Vec<String> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Sequence(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                other => format!("{other:?}"),
            })
            .collect(),
        Some(other) => vec![format!("{other:?}")],
    }
}

fn compile_keyword(keyword: &str) -> Option<Regex> {
    let pattern = format!(r"\b{}\b", regex::escape(keyword));
    match Regex::new(&pattern) {
        Ok(rx) => Some(rx),
        Err(err) => {
            tracing::error!(keyword, error = %err, "bad keyword in faq.yaml ignored");
            None
        }
    }
}

fn compile_regex(pattern: &str) -> Option<Regex> {
    match Regex::new(&format!("(?i){pattern}")) {
        Ok(rx) => Some(rx),
        Err(err) => {
            tracing::error!(pattern, error = %err, "bad regex in faq.yaml ignored");
            None
        }
    }
}

fn load_entry(raw: &Value) -> Option<Entry> {
    let map = raw.as_mapping()?;
    let get = |key: &str| map.get(Value::String(key.to_string()));

    let id = get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if id.is_empty() {
        return None;
    }

    let quick_replies = get("quick_replies")
        .and_then(Value::as_sequence)
        .map(|tiles| {
            tiles
                .iter()
                .filter_map(|tile| {
                    let tile = tile.as_mapping()?;
                    let title = tile.get(Value::String("title".into()))?.as_str()?.to_string();
                    let payload = tile
                        .get(Value::String("payload".into()))?
                        .as_str()?
                        .to_string();
                    if title.is_empty() || payload.is_empty() {
                        return None;
                    }
                    Some(QRTile {
                        title: title.chars().take(20).collect(),
                        payload,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let regexes = as_str_list(get("regex"))
        .iter()
        .filter_map(|p| compile_regex(p))
        .collect();

    let keywords = as_str_list(get("keywords"))
        .iter()
        .filter_map(|k| compile_keyword(&k.trim().to_lowercase()))
        .collect();

    Some(Entry {
        id,
        payloads: as_str_list(get("payloads"))
            .iter()
            .map(|p| p.trim().to_uppercase())
            .collect(),
        patterns: as_str_list(get("patterns"))
            .iter()
            .map(|p| p.to_lowercase())
            .collect(),
        regexes,
        keywords,
        answer: get("answer")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        quick_replies,
    })
}

/// Parse `faq.yaml` into an ordered index. A missing file yields an empty
/// index (so the bot still runs); a malformed file is an error so the previous
/// good copy keeps being used.
pub fn load_faq(path: &Path) -> Result<FaqIndex, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FaqIndex::default());
        }
        Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
    };
    let raw: Value = serde_yaml::from_str(&text).map_err(|e| format!("invalid YAML: {e}"))?;
    let entries_raw = raw
        .as_mapping()
        .and_then(|m| m.get(Value::String("entries".into())))
        .and_then(Value::as_sequence)
        .cloned()
        .unwrap_or_default();

    let mut index = FaqIndex::default();
    for item in &entries_raw {
        let Some(entry) = load_entry(item) else {
            continue;
        };
        let entry = Arc::new(entry);
        if index.by_id.contains_key(&entry.id) {
            // Preserve dict semantics: last definition wins.
            if let Some(pos) = index.ordered.iter().position(|e| e.id == entry.id) {
                index.ordered[pos] = entry.clone();
            }
        } else {
            index.ordered.push(entry.clone());
        }
        index.by_id.insert(entry.id.clone(), entry.clone());
        for payload in &entry.payloads {
            if !payload.is_empty() {
                index
                    .by_payload
                    .entry(payload.clone())
                    .or_insert_with(|| entry.clone());
            }
        }
    }
    Ok(index)
}

#[derive(Clone)]
pub struct FaqStore {
    hot: Arc<HotFile<FaqIndex>>,
}

impl FaqStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            hot: Arc::new(HotFile::new(path, "faq.yaml", load_faq)),
        }
    }

    pub fn get(&self) -> Arc<FaqIndex> {
        match self.hot.get() {
            Ok(index) => index,
            Err(err) => {
                tracing::error!(error = %err, "faq.yaml not found; running with no static answers");
                Arc::new(FaqIndex::default())
            }
        }
    }

    pub fn clear_cache(&self) {
        self.hot.clear();
    }
}

// ---------------------------------------------------------------------------
// knowledge.md (the LLM system prompt)
// ---------------------------------------------------------------------------
const MAX_PROMPT_CHARS: usize = 6000;

pub fn load_prompt(path: &Path) -> Result<String, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let text = text.trim();
    if text.is_empty() {
        return Err("knowledge.md is empty".into());
    }
    Ok(text.to_string())
}

pub fn system_prompt(hot: &HotFile<String>) -> String {
    let fallback = "You are Erl, the assistant of Erlbrew Cafe in Kawit, Cavite. \
Answer briefly and friendlily. Never guess prices, hours or availability. \
Decline unrelated questions politely.";
    let text = hot.get().map(|t| (*t).clone()).unwrap_or_else(|err| {
        tracing::error!(error = %err, "knowledge.md unavailable, minimal prompt used");
        fallback.to_string()
    });
    text.chars().take(MAX_PROMPT_CHARS).collect()
}
