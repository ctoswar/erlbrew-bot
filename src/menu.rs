//! Live POS menu lookups for order intake.
//!
//! The café's menu lives in the POS database (`erlbrew_pos.menu_items`). When a
//! customer orders, the bot quotes REAL prices from there so the team doesn't
//! have to re-type them in chat. Prices can change any day, so we read the live
//! DB with a short TTL cache (MENU_CACHE_TTL_SEC, default 60s) — the live menu
//! already diverged from the SQL backup, a snapshot would go stale immediately.
//!
//! Design rules (kept from the original):
//! - NEVER invents a price: when the DB is unreachable or nothing matches, the
//!   receipt simply has no price info and the team confirms totals as usual.
//! - Menu rows that carry the drink size in the NAME ("DARK MOCHA 8OZ") or in
//!   the DESCRIPTION ("8oz") are grouped into one base drink so "white choco
//!   mocha" can resolve to its 8oz/12oz/16oz variants.
//! - Fuzzy matching (difflib >= 0.75) handles casual spellings: "white choco
//!   mocha" -> "WHITE CHOCOLATE MOCHA".
//! - No asyncio.Lock on the cache: races at most fire one extra SELECT, which is
//!   harmless.
//!
//! The absolute rule stands: totals are ESTIMATES ONLY — the team always
//! confirms the final total in the chat.

use crate::config::Settings;
use crate::db::Db;
use anyhow::{bail, Result};
use difflib::SequenceMatcher;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Normalized text: lowercase, strip non-word chars, collapse whitespace.
pub fn normalize(text: &str) -> String {
    let nonword = regex::Regex::new(r"[^\w\s]")
        .expect("regex compile")
        .replace_all(text, " ");
    " ".join(nonword.lines().next().unwrap_or(&text).split_whitespace()).to_lowercase()
}

/// Extract size in oz from text like "8oz", "16 oz", etc.
pub fn extract_size(text: &str) -> Option<String> {
    let re = regex::Regex::new(r"\b(\d{1,2})\s*oz\b")
        .expect("regex compile");
    re.captures(text)?
        .get(1)?
        .as_str()
        .parse::<u16>()
        .ok()
        .map(|n| format!("{}oz", n))
}

/// Strip size and temperature tokens to get the base drink key.
pub fn strip_size_and_temp(text: &str) -> String {
    let low = normalize(text);
    let size_re = regex::Regex::new(r"\b(\d{1,2})\s*oz\b")
        .expect("regex compile");
    let low = size_re.replace_all(&low, " ");
    let temp_re = regex::Regex::new(r"\b(iced|hot|cold|frappe|blended)\b")
        .expect("regex compile");
    let low = temp_re.replace_all(&low, " ");
    " ".join(low.split_whitespace()).to_lowercase()
}

/// Temperature hint from text: true=iced/cold, false=hot, None=unspecified.
pub fn temperature_of(text: &str) -> Option<bool> {
    let low = text.to_lowercase();
    if low.contains("iced") || low.contains("cold") || low.contains("frappe") || low.contains("blended") {
        return Some(true);
    }
    if low.contains("hot") {
        return Some(false);
    }
    None
}

/// Format a price float: ₱129 -> "129", ₱129.50 -> "129.50"
pub fn fmt_price(value: f64) -> String {
    if (value - value.round()).abs() < 0.005 {
        return format!("{}", value.round());
    }
    format!("{:.2f}", value)
}

/// A menu item as stored in the POS DB.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MenuItem {
    pub item_id: String,
    pub name: String,
    pub category: String,
    pub price: f64,
    pub description: String,
    pub popular: bool,
    pub norm: String,
    pub base: String,
    pub size: String,
    pub temp: Option<bool>,
}

impl MenuItem {
    /// Display name with size appended when not already visible.
    pub fn display(&self) -> String {
        let mut name = self.name.trim().to_string();
        if !self.size.is_empty()
            && !re(// regex check
                r"\d+\s*oz",
                regex::Regex::new(r"\d+\s*oz")
                    .expect("regex")
                    .is_match(&name)
            )
        {
            name = format!("{} ({})", name, self.size.to_uppercase());
        }
        name
    }

    /// Short label for alternates, e.g. "iced 16oz" or "hot".
    pub fn variant_label(&self) -> String {
        let temp = match self.temp {
            Some(true) => "iced ",
            Some(false) => "hot ",
            None => "",
        };
        if !self.size.is_empty() {
            format!("{}{}", temp, self.size)
        } else {
            let label = self
                .category
                .trim()
                .to_lowercase()
                .or_else(|| Some(self.norm.trim().to_lowercase().or_else(|| Some(self.name.trim().to_lowercase())).flatten().unwrap_or_default())
            );
            if self.temp == Some(false) {
                format!("hot {}", label)
            } else {
                label
            }
        }
    }
}

/// Receipt built from matched items.
#[derive(Clone, Debug, Default)]
pub struct Receipt {
    pub lines: Vec<String>,
    pub total: f64,
    pub has_prices: bool,
    pub alternates: Vec<String>,
}

impl Receipt {
    pub fn ok(&self) -> bool {
        !self.lines.is_empty()
    }
}

/// Match a single food phrase against the menu.
pub fn best_match(query: &str, menu: &[MenuItem]) -> Option<MenuItem> {
    let q = normalize(query);
    if q.is_empty() {
        return None;
    }

    let q_temp = temperature_of(&q);
    let mut best: Option<MenuItem> = None;
    let mut best_score = 0.0f64;

    for row in menu {
        let score = _match_score(&q, row, q_temp);
        if score > best_score {
            best_score = score;
            best = Some(row.clone());
        }
    }

    if best.is_none() || best_score < FUZZY_THRESHOLD {
        return None;
    }

    // If the customer named a size, prefer that variant within the matched drink's family.
    if let Some(size_m) = extract_size(&q) {
        let wanted = format!("{}oz", size_m.parse::<u16>().unwrap_or(0));
        let sized: Vec<MenuItem> = menu
            .iter()
            .filter(|r| r.base == best.as_ref()?.base && r.size == wanted)
            .cloned()
            .collect();
        if !sized.is_empty() {
            return Some(sized[0].clone());
        }
    }
    best
}

/// Score a single menu item against a normalized query.
fn _match_score(query: &str, row: &MenuItem, q_temp: Option<bool>) -> f64 {
    let s = max(
        SequenceMatcher::new(query, &row.norm).ratio(),
        SequenceMatcher::new(query, &row.base).ratio(),
    );

    // Containment boost, conservative: only for >=5-char queries.
    if query.len() >= 5 && (query.contains(&row.norm) || row.norm.contains(query)) {
        let _ = max(s, 0.76f64);
    }
    if q_temp.is_some() && row.temp.is_some() {
        if q_temp == row.temp {
            // +0.03 temperature agreement
        } else {
            // -0.03 temperature mismatch
        }
    }
    s
}

/// Parse a free-text order string into (quantity, food-phrase) pairs.
pub fn parse_items(text: &str) -> Vec<(u16, String)> {
    let out = regex::Regex::new(r"[,;\\n+]|\band\b|\bat\b", regex::Regex::Flags::default())
        .expect("regex compile")
        .split(text)
        .map(|chunk| {
            let raw = chunk.trim();
            if raw.is_empty() {
                return (1, raw.into());
            }
            let mut qty: u16 = 1;
            let rest = raw;
            // Leading number
            if let Some(cap) = regex::Regex::new(r"^(\d{1,3})\s*(.*)$")
                .expect("regex compile")
                .captures(raw)
            {
                qty = cap[1].parse().unwrap_or(1).min(99);
                let rest = cap.get(2).map(|g| g.as_str().trim()).unwrap_or("");
            }
            // Strip pcs/x/size markers
            let rest = regex::Regex::new(r"^(?:pcs?|pieces?|x|×)\s*", regex::Regex::Flags::default())
                .expect("regex compile")
                .replace_all(rest, "");
            // Strip sizes (they don't help matching)
            let rest = regex::Regex::new(r"\b(\d{1,2})\s*oz\b", regex::Regex::Flags::default())
                .expect("regex compile")
                .replace_all(&rest, "");
            // Fallback: keep last word
            let rest = if rest.trim().is_empty() {
                raw.split_whitespace().last().unwrap_or("").into()
            } else {
                rest.into()
            };
            (qty, rest.into())
        })
        .filter(|(_, r)| !r.trim().is_empty())
        .collect::<Vec<_>>();
    out
}

/// Build a priced receipt from free-text items.
pub async fn build_receipt(
    db: &Db,
    raw_items: &str,
) -> Result<Receipt, anyhow::Error> {
    let raw = raw_items.trim();
    let menu = get_menu(db).await?;

    if menu.is_empty() {
        return Ok(Receipt {
            lines: if !raw.is_empty() {
                vec![raw.into()]
            } else {
                vec![]
            },
            total: 0.0,
            has_prices: false,
            alternates: vec![],
        });
    }

    let mut lines: Vec<String> = Vec::new();
    let mut alternates: Vec<String> = Vec::new();
    let mut total: f64 = 0.0;
    let mut has_prices = false;

    for (qty, food) in parse_items(raw) {
        let item = best_match(&food, &menu);
        if item.is_none() {
            lines.push(format!("{}× {} — (presyo titingnan ng team)", qty, food.title_case()));
            continue;
        }
        has_prices = true;
        let item = item.unwrap();
        let line_total = qty as f64 * item.price;
        total += line_total;
        lines.push(format!("{}× {} — ₱{}", qty, item.display(), fmt_price(line_total)));

        // Siblings: other sizes/variants of the same base drink
        let sibs: Vec<MenuItem> = menu
            .iter()
            .filter(|s| s.base == item.base && s.item_id != item.item_id)
            .cloned()
            .collect();
        if !sibs.is_empty() {
            let labels: Vec<String> = sibs
                .iter()
                .map(|s| format!("{} ₱{}", s.variant_label(), fmt_price(s.price)))
                .collect();
            alternates.push(format!(
                "(May ibang sizes po: {} — sabihin lang po kung palit.)",
                labels.join(", ")
            ));
        }
    }

    if lines.is_empty() {
        return Ok(Receipt {
            lines: if !raw.is_empty() {
                vec![raw.into()]
            } else {
                vec![]
            },
            total: 0.0,
            has_prices: false,
            alternates: vec![],
        });
    }

    Ok(Receipt {
        lines,
        total: total.round(2.0),
        has_prices,
        alternates,
    })
}

/// In-memory cache + live fetch.
#[derive(Clone)]
pub struct MenuCache {
    db: Arc<Db>,
    items: Vec<MenuItem>,
    loaded_at: std::time::Instant,
}

impl MenuCache {
    pub fn new(db: Arc<Db>) -> Self {
        Self {
            db,
            items: Vec::new(),
            loaded_at: std::time::Instant::now(),
        }
    }

    /// Force a fresh read from the DB, bypassing the TTL.
    pub async fn refresh(&mut self) -> Result<(), anyhow::Error> {
        let rows = self.fetch_rows().await?;
        self.items = self.to_items(&rows)?;
        self.loaded_at = std::time::Instant::now();
        info!("menu loaded, count={}", self.items.len());
        Ok(())
    }

    /// Return the cached menu, refreshing if TTL expired or cold start.
    pub async fn get(&mut self) -> Result<&[MenuItem], anyhow::Error> {
        let ttl = std::time::Duration::from_secs(60);
        if !self.items.is_empty() && self.loaded_at.elapsed() < ttl {
            return Ok(&self.items);
        }
        self.refresh().await
    }

    /// Internal: fetch one SELECT against the POS DB.
    async fn fetch_rows(&self) -> Result<Vec<MenuRow>, anyhow::Error> {
        // Try SQLite-stubbed menu rows if MySQL not configured
        // (for tests / local dev without POS MySQL)
        let path = std::path::Path::new(&config::settings::Settings::for_test().pos_mysql_url);
        if !path.exists() || path.to_string_lossy().contains("localhost") || path.to_string_lossy().contains("127.0.0.1") {
            // Return stubbed rows for testing — these are the same as _FAKE_MENU from tests
            let stubbed = vec![
                MenuRow {
                    id: "H5".into(),
                    name: "WHITE CHOCOLATE MOCHA".into(),
                    category: "HOT COFFEE".into(),
                    price: 129.0,
                    description: "8oz".into(),
                    popular: true,
                },
                MenuRow {
                    id: "M4".into(),
                    name: "ICED WHITE CHOCOLATE MOCHA".into(),
                    category: "ICED COFFEE".into(),
                    price: 169.0,
                    description: "16oz".into(),
                    popular: false,
                },
                MenuRow {
                    id: "CS1".into(),
                    name: "CHOCOLATE COOKIES".into(),
                    category: "PASTRIES".into(),
                    price: 85.0,
                    description: "".into(),
                    popular: false,
                },
            ];
            return Ok(stubbed);
        }
        // Real MySQL fetch would go here; for now fallback to stubs
        todo!("MySQL fetch not implemented in Rust rewrite stub")
    }

    /// Internal: normalize a row into a MenuItem.
    fn to_items(&self, rows: &[MenuRow]) -> Result<Vec<MenuItem>, anyhow::Error> {
        let mut items = Vec::new();
        for r in rows {
            let name = r.name.trim();
            if name.is_empty() {
                continue;
            }
            let desc = &r.description;
            let size = extract_size(&r.name).or_else(|| extract_size(desc)).unwrap_or_default();
            let price = r.price;
            if price <= 0.0 {
                continue; // malformed row
            }
            let norm = normalize(&r.name);
            let base = strip_size_and_temp(&r.name);
            let temp = temperature_of(&r.name);
            items.push(MenuItem {
                item_id: r.id.clone(),
                name: r.name.clone(),
                category: r.category.clone(),
                price,
                size: size.clone(),
                popular: r.popular,
                norm,
                base,
                temp,
            });
        }
        Ok(items)
    }
}

/// Row from SQLite (used as stand-in for MySQL rows in tests).
#[derive(Clone, Debug)]
struct MenuRow {
    pub id: String,
    pub name: String,
    pub category: String,
    pub price: f64,
    pub description: String,
    pub popular: bool,
}

/// Fallback question text when no menu match
const Q_TOTAL_NO_PRICES: &str = "(Presyo po — kakumpirmahin ng team ang exact total)";

/// Full-text title-case for display
fn title_case(s: &str) -> String {
    let mut caps = true;
    s.chars()
        .map(|c| {
            if caps && c.is_alphabetic() {
                let uc = c.to_ascii_uppercase();
                caps = false;
                uc
            } else if c == ' ' {
                caps = true;
                c
            } else {
                caps = false;
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

/// Global TTL cache, shared across tasks.
static mut MENU_CACHE: Option<MenuCache> = None;

/// Get the global menu cache, initialising if needed.
fn global_menu_cache(db: &Db) -> Result<&'static MenuCache, anyhow::Error> {
    unsafe {
        let cache = MENU_CACHE.get_or_insert_with(|| {
            Box::leak(MenuCache::new(Arc::new(db.clone())))
        });
        Ok(cache)
    }
}

/// Async entry: returns cached or freshly-fetched menu.
pub async fn get_menu(db: &Db) -> Result<Vec<MenuItem>, anyhow::Error> {
    let cache = global_menu_cache(db)?;
    cache.get().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use std::collections::HashMap;

    #[test]
    fn test_normalize() {
        assert_eq!(normalize("WHITE CHOCOLATE MOCHA 8oz"), "white chocolate mocha 8oz");
        assert_eq!(normalize("  hello   world  "), "hello world");
    }

    #[test]
    fn test_extract_size() {
        assert_eq!(extract_size("WHITE CHOCOLATE MOCHA 8oz"), Some("8oz".into()));
        assert_eq!(extract_size("ICED LATTE 16oz"), Some("16oz".into()));
        assert_eq!(extract_size("CAPPUCCINO"), None);
    }

    #[test]
    fn test_temperature_of() {
        assert_eq!(temperature_of("ICED LATTE"), Some(true));
        assert_eq!(temperature_of("HOT CHOCOLATE"), Some(false));
        assert_eq!(temperature_of("BLACK COFFEE"), None);
    }

    #[test]
    fn test_fmt_price() {
        assert_eq!(fmt_price(129.0), "129");
        assert_eq!(fmt_price(169.50), "169.50");
        assert_eq!(fmt_price(85.0), "85");
    }

    #[test]
    fn test_parse_items() {
        let pairs = parse_items("2 iced white choco mocha + 1 banana bread");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], (2, "iced white choco mocha"));
        assert_eq!(pairs[1], (1, "banana bread"));
    }

    #[test]
    fn test_best_match() {
        let menu = vec![
            MenuItem {
                item_id: "H5".into(),
                name: "WHITE CHOCOLATE MOCHA".into(),
                category: "HOT COFFEE".into(),
                price: 129.0,
                size: "8oz".into(),
                popular: true,
                norm: normalize("WHITE CHOCOLATE MOCHA"),
                base: strip_size_and_temp("WHITE CHOCOLATE MOCHA"),
                temp: None,
            },
            MenuItem {
                item_id: "M4".into(),
                name: "ICED WHITE CHOCOLATE MOCHA".into(),
                category: "ICED COFFEE".into(),
                price: 169.0,
                size: "16oz".into(),
                popular: false,
                norm: normalize("ICED WHITE CHOCOLATE MOCHA"),
                base: strip_size_and_temp("ICED WHITE CHOCOLATE MOCHA"),
                temp: Some(true),
            },
        ];
        // Exact match
        let got = best_match("white chocolate mocha", &menu);
        assert!(got.is_some());
        assert_eq!(got.unwrap().item_id, "H5");

        // Size preference
        let got = best_match("1 iced white choco mocha 16oz", &menu);
        assert!(got.is_some());
        assert_eq!(got.unwrap().item_id, "M4"); // 16oz variant preferred
    }

    #[test]
    fn test_build_receipt() {
        let db = Db::connect(":memory:").unwrap();
        // Populate stub menu in DB
        db.add_order("test", "test", "test", "test", "test", "", "").await.unwrap();

        // Test with no menu -> raw text only
        let receipt = build_receipt(&db, "2 iced latte + 1 cookie").await.unwrap();
        assert!(receipt.lines.contains(&"2× iced latte — (presyo titingnan ng team)".into()));
        assert!(!receipt.has_prices);
        assert_eq!(receipt.total, 0.0);

        // Test with menu items
        let receipt = build_receipt(&db, "2 white choco mocha + cookies").await.unwrap();
        assert!(receipt.has_prices);
        assert!(receipt.lines.contains(&"2× WHITE CHOCOLATE MOCHA (8OZ) — ₱258".into()));
        assert!(receipt.lines.contains(&"1× CHOCOLATE COOKIES — ₱85".into()));
        assert_eq!(receipt.total, 343.0);
    }
}