//! Live POS menu lookups for order intake.
//!
//! The café's menu lives in the POS database. When a customer orders, the bot quotes REAL
//! prices from there so the team doesn't have to re-type them in chat. Prices can change any
//! day, so the menu is read live with a short TTL cache (MENU_CACHE_TTL_SEC, default 60s).
//!
//! Rules:
//! - NEVER invents a price: when the DB is unreachable or nothing matches, the receipt simply
//!   has no price info and the team confirms totals as usual.
//! - Rows carrying the drink size in the NAME ("DARK MOCHA 8OZ") or DESCRIPTION ("8oz") are
//!   grouped into one base drink, so "white choco mocha" can resolve to its 8oz/16oz variants.
//! - Fuzzy matching (Ratcliff/Obershelp ratio >= 0.75, same as Python's difflib) handles casual
//!   spellings: "white choco mocha" -> "WHITE CHOCOLATE MOCHA".
//! - Totals are ESTIMATES ONLY — the team always confirms the final total in chat.

use crate::config::Settings;
use anyhow::{Context, Result};
use mysql_async::prelude::*;
use regex::Regex;
use serde::Serialize;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub const FUZZY_THRESHOLD: f64 = 0.75;

static NON_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\w\s]").unwrap());
static SIZE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(\d{1,2})\s*oz\b").unwrap());
static TEMP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(iced|hot|cold|frappe|blended)\b").unwrap());
static SPLIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[,;\n+]|\band\b|\bat\b").unwrap());
static QTY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{1,3})\s*(.*)$").unwrap());
static UNIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:pcs?|pieces?|x|×)\s*").unwrap());

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// Lowercase, strip non-word chars, collapse whitespace.
pub fn normalize(text: &str) -> String {
    let lower = text.to_lowercase();
    let cleaned = NON_WORD.replace_all(&lower, " ");
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// "8oz", "16 oz" -> Some("8oz") / Some("16oz").
pub fn extract_size(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let n: u16 = SIZE_RE.captures(&lower)?.get(1)?.as_str().parse().ok()?;
    Some(format!("{n}oz"))
}

/// Drop size and temperature words to get the base drink key.
pub fn strip_size_and_temp(text: &str) -> String {
    let low = normalize(text);
    let low = SIZE_RE.replace_all(&low, " ");
    let low = TEMP_RE.replace_all(&low, " ");
    low.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Some(true) = iced/cold, Some(false) = hot, None = unspecified.
pub fn temperature_of(text: &str) -> Option<bool> {
    let low = normalize(text);
    let has = |w: &str| low.split_whitespace().any(|t| t == w);
    if has("iced") || has("cold") || has("frappe") || has("blended") {
        Some(true)
    } else if has("hot") {
        Some(false)
    } else {
        None
    }
}

/// 129.0 -> "129", 129.5 -> "129.50".
pub fn fmt_price(value: f64) -> String {
    if (value - value.round()).abs() < 0.005 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.2}")
    }
}

fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ratcliff/Obershelp similarity (what Python's `difflib.SequenceMatcher.ratio()` computes).
pub fn ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0;
    }
    2.0 * matching_chars(&a, &b) as f64 / total as f64
}

fn matching_chars(a: &[char], b: &[char]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    // Longest common substring (earliest in `a`, then earliest in `b`).
    let (mut best, mut ai, mut bi) = (0usize, 0usize, 0usize);
    let mut prev = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        let mut cur = vec![0usize; b.len() + 1];
        for j in 1..=b.len() {
            if a[i - 1] == b[j - 1] {
                cur[j] = prev[j - 1] + 1;
                if cur[j] > best {
                    best = cur[j];
                    ai = i - best;
                    bi = j - best;
                }
            }
        }
        prev = cur;
    }
    if best == 0 {
        return 0;
    }
    best + matching_chars(&a[..ai], &b[..bi]) + matching_chars(&a[ai + best..], &b[bi + best..])
}

// ---------------------------------------------------------------------------
// Menu model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MenuItem {
    pub item_id: String,
    pub name: String,
    pub category: String,
    pub price: f64,
    pub popular: bool,
    pub norm: String,
    pub base: String,
    pub size: String,
    pub temp: Option<bool>,
}

impl MenuItem {
    /// Display name, with the size appended when it isn't already visible in the name.
    pub fn display(&self) -> String {
        let name = self.name.trim().to_string();
        if !self.size.is_empty() && !SIZE_RE.is_match(&name.to_lowercase()) {
            format!("{} ({})", name, self.size.to_uppercase())
        } else {
            name
        }
    }

    /// Short label for alternates, e.g. "iced 16oz" or "hot pastries".
    pub fn variant_label(&self) -> String {
        let temp = match self.temp {
            Some(true) => "iced ",
            Some(false) => "hot ",
            None => "",
        };
        if !self.size.is_empty() {
            return format!("{temp}{}", self.size);
        }
        let label = [self.category.trim(), self.norm.trim(), self.name.trim()]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_lowercase();
        if self.temp == Some(false) {
            format!("hot {label}")
        } else {
            label
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Receipt {
    pub lines: Vec<String>,
    pub total: f64,
    pub has_prices: bool,
    pub alternates: Vec<String>,
}

/// One row from the POS `menu_items` query.
struct MenuRow {
    id: String,
    name: String,
    category: String,
    price: f64,
    description: String,
    popular: bool,
}

fn rows_to_items(rows: Vec<MenuRow>) -> Vec<MenuItem> {
    rows.into_iter()
        .filter(|r| !r.name.trim().is_empty() && r.price > 0.0)
        .map(|r| MenuItem {
            size: extract_size(&r.name).or_else(|| extract_size(&r.description)).unwrap_or_default(),
            norm: normalize(&r.name),
            base: strip_size_and_temp(&r.name),
            temp: temperature_of(&r.name),
            item_id: r.id,
            name: r.name,
            category: r.category,
            price: r.price,
            popular: r.popular,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

fn match_score(query: &str, row: &MenuItem, q_temp: Option<bool>) -> f64 {
    let mut s = ratio(query, &row.norm).max(ratio(query, &row.base));
    // Containment boost, conservative: only for >= 5-char queries.
    if query.chars().count() >= 5 && (row.norm.contains(query) || query.contains(&row.norm)) {
        s = s.max(0.76);
    }
    if let (Some(q), Some(r)) = (q_temp, row.temp) {
        s += if q == r { 0.03 } else { -0.03 };
    }
    s
}

/// Match one food phrase against the menu.
pub fn best_match(query: &str, menu: &[MenuItem]) -> Option<MenuItem> {
    let q = normalize(query);
    if q.is_empty() {
        return None;
    }
    let q_temp = temperature_of(&q);

    let mut best: Option<(&MenuItem, f64)> = None;
    for row in menu {
        let score = match_score(&q, row, q_temp);
        if best.is_none_or(|(_, b)| score > b) {
            best = Some((row, score));
        }
    }
    let (best, score) = best?;
    if score < FUZZY_THRESHOLD {
        return None;
    }

    // If the customer named a size, prefer that variant of the same base drink.
    if let Some(wanted) = extract_size(&q) {
        if let Some(sized) = menu.iter().find(|r| r.base == best.base && r.size == wanted) {
            return Some(sized.clone());
        }
    }
    Some(best.clone())
}

/// "2 iced latte + 1 cookie" -> [(2, "iced latte"), (1, "cookie")].
pub fn parse_items(text: &str) -> Vec<(u16, String)> {
    SPLIT_RE
        .split(text)
        .filter_map(|chunk| {
            let raw = chunk.trim();
            if raw.is_empty() {
                return None;
            }
            let (qty, rest) = match QTY_RE.captures(raw) {
                Some(cap) => (
                    cap[1].parse::<u16>().unwrap_or(1).clamp(1, 99),
                    cap[2].trim().to_string(),
                ),
                None => (1, raw.to_string()),
            };
            let rest = UNIT_RE.replace(&rest, "").to_string();
            // Sizes don't help matching.
            let rest = SIZE_RE.replace_all(&rest, "").trim().to_string();
            let rest = if rest.is_empty() {
                raw.split_whitespace().last().unwrap_or("").to_string()
            } else {
                rest
            };
            (!rest.is_empty()).then_some((qty, rest))
        })
        .collect()
}

/// Build a priced receipt from free-text items. Pure: no I/O.
pub fn build_receipt(menu: &[MenuItem], raw_items: &str) -> Receipt {
    let raw = raw_items.trim();
    let unpriced = || Receipt {
        lines: if raw.is_empty() { vec![] } else { vec![raw.to_string()] },
        ..Receipt::default()
    };
    if menu.is_empty() {
        return unpriced();
    }

    let mut receipt = Receipt::default();
    for (qty, food) in parse_items(raw) {
        let Some(item) = best_match(&food, menu) else {
            receipt
                .lines
                .push(format!("{qty}× {} — (presyo titingnan ng team)", title_case(&food)));
            continue;
        };
        receipt.has_prices = true;
        let line_total = f64::from(qty) * item.price;
        receipt.total += line_total;
        receipt.lines.push(format!("{qty}× {} — ₱{}", item.display(), fmt_price(line_total)));

        let labels: Vec<String> = menu
            .iter()
            .filter(|s| s.base == item.base && s.item_id != item.item_id)
            .map(|s| format!("{} ₱{}", s.variant_label(), fmt_price(s.price)))
            .collect();
        if !labels.is_empty() {
            receipt.alternates.push(format!(
                "(May ibang sizes po: {} — sabihin lang po kung palit.)",
                labels.join(", ")
            ));
        }
    }

    if receipt.lines.is_empty() {
        return unpriced();
    }
    receipt.total = (receipt.total * 100.0).round() / 100.0;
    receipt
}

// ---------------------------------------------------------------------------
// Live cache
// ---------------------------------------------------------------------------

/// TTL cache over the POS menu. Races at most fire one extra SELECT, which is harmless.
pub struct MenuCache {
    settings: Arc<Settings>,
    pool: Option<mysql_async::Pool>,
    cached: Mutex<Option<(Instant, Arc<Vec<MenuItem>>)>>,
}

impl MenuCache {
    pub fn new(settings: Arc<Settings>) -> Self {
        let pool = if settings.pos_mysql_url.is_empty() {
            None
        } else {
            match mysql_async::Opts::from_url(&settings.pos_mysql_url) {
                Ok(opts) => Some(mysql_async::Pool::new(opts)),
                Err(err) => {
                    warn!(error = %err, "POS_MYSQL_URL is invalid; orders will have no live prices");
                    None
                }
            }
        };
        Self { settings, pool, cached: Mutex::new(None) }
    }

    /// Cached or freshly fetched menu. Never fails: falls back to the stale copy, then to empty.
    pub async fn get(&self) -> Arc<Vec<MenuItem>> {
        let ttl = Duration::from_secs(self.settings.menu_cache_ttl_sec);
        let stale = {
            let guard = self.cached.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some((at, items)) if !items.is_empty() && at.elapsed() < ttl => {
                    return Arc::clone(items);
                }
                Some((_, items)) => Some(Arc::clone(items)),
                None => None,
            }
        };

        match tokio::time::timeout(Duration::from_secs(5), self.fetch()).await {
            Ok(Ok(items)) => {
                info!(count = items.len(), "menu loaded from POS");
                let items = Arc::new(items);
                *self.cached.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some((Instant::now(), Arc::clone(&items)));
                items
            }
            Ok(Err(err)) => {
                warn!(error = %err, "POS menu fetch failed");
                stale.unwrap_or_default()
            }
            Err(_) => {
                warn!("POS menu fetch timed out");
                stale.unwrap_or_default()
            }
        }
    }

    async fn fetch(&self) -> Result<Vec<MenuItem>> {
        let pool = self.pool.as_ref().context("POS_MYSQL_URL not configured")?;
        let mut conn = pool.get_conn().await.context("connecting to POS database")?;
        // Text protocol returns every column as bytes, which mysql_async converts for us.
        let rows: Vec<(String, String, Option<String>, f64, Option<String>, Option<i64>)> =
            conn.query(self.settings.pos_menu_query.as_str()).await.context("running menu query")?;
        Ok(rows_to_items(
            rows.into_iter()
                .map(|(id, name, category, price, description, popular)| MenuRow {
                    id,
                    name,
                    category: category.unwrap_or_default(),
                    price,
                    description: description.unwrap_or_default(),
                    popular: popular.unwrap_or(0) != 0,
                })
                .collect(),
        ))
    }

    pub async fn build_receipt(&self, raw_items: &str) -> Receipt {
        build_receipt(&self.get().await, raw_items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_menu() -> Vec<MenuItem> {
        rows_to_items(vec![
            MenuRow { id: "H5".into(), name: "WHITE CHOCOLATE MOCHA".into(), category: "HOT COFFEE".into(), price: 129.0, description: "8oz".into(), popular: true },
            MenuRow { id: "M4".into(), name: "ICED WHITE CHOCOLATE MOCHA".into(), category: "ICED COFFEE".into(), price: 169.0, description: "16oz".into(), popular: false },
            MenuRow { id: "CS1".into(), name: "CHOCOLATE COOKIES".into(), category: "PASTRIES".into(), price: 85.0, description: "".into(), popular: false },
        ])
    }

    #[test]
    fn helpers() {
        assert_eq!(normalize("  WHITE   CHOCOLATE, MOCHA 8oz "), "white chocolate mocha 8oz");
        assert_eq!(extract_size("ICED LATTE 16 oz"), Some("16oz".into()));
        assert_eq!(extract_size("CAPPUCCINO"), None);
        assert_eq!(temperature_of("ICED LATTE"), Some(true));
        assert_eq!(temperature_of("HOT CHOCOLATE"), Some(false));
        assert_eq!(temperature_of("WHITE MOCHA"), None);
        assert_eq!(fmt_price(129.0), "129");
        assert_eq!(fmt_price(169.5), "169.50");
        assert_eq!(strip_size_and_temp("ICED White Mocha 16oz"), "white mocha");
    }

    #[test]
    fn ratio_matches_difflib() {
        assert!((ratio("abcd", "bcde") - 0.75).abs() < 1e-9);
        assert_eq!(ratio("same", "same"), 1.0);
        assert_eq!(ratio("abc", "xyz"), 0.0);
    }

    #[test]
    fn parsing() {
        assert_eq!(
            parse_items("2 iced white choco mocha + 1 banana bread"),
            vec![(2, "iced white choco mocha".to_string()), (1, "banana bread".to_string())]
        );
        assert_eq!(
            parse_items("3 pcs cookies at 1 latte 16oz"),
            vec![(3, "cookies".to_string()), (1, "latte".to_string())]
        );
    }

    #[test]
    fn matching_and_size_preference() {
        let menu = fake_menu();
        assert_eq!(best_match("white chocolate mocha", &menu).unwrap().item_id, "H5");
        assert_eq!(best_match("1 iced white choco mocha 16oz", &menu).unwrap().item_id, "M4");
        assert!(best_match("pizza margherita", &menu).is_none());
    }

    #[test]
    fn receipts() {
        let menu = fake_menu();
        let r = build_receipt(&menu, "2 white choco mocha + cookies");
        assert!(r.has_prices);
        assert!(r.lines.contains(&"2× WHITE CHOCOLATE MOCHA (8OZ) — ₱258".to_string()), "{:?}", r.lines);
        assert!(r.lines.contains(&"1× CHOCOLATE COOKIES — ₱85".to_string()), "{:?}", r.lines);
        assert_eq!(r.total, 343.0);
        assert_eq!(r.alternates.len(), 1);

        let none = build_receipt(&[], "2 iced latte");
        assert!(!none.has_prices);
        assert_eq!(none.lines, vec!["2 iced latte".to_string()]);

        let unknown = build_receipt(&menu, "2 pizza");
        assert!(!unknown.has_prices);
        assert_eq!(unknown.lines, vec!["2× Pizza — (presyo titingnan ng team)".to_string()]);
    }
}
