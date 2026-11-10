"""Live POS menu lookups for order intake (read-only MySQL via aiomysql).

The café's menu lives in the POS database (`erlbrew_pos.menu_items`). When a
customer orders, the bot quotes REAL prices from there so the team doesn't
have to re-type them in chat. Prices can change any day, so we read the live
DB with a short TTL cache (MENU_CACHE_TTL_SEC, default 60s) — the live menu
already diverged from the SQL backup, a snapshot would go stale immediately.

Design rules:
- NEVER invents a price: when the DB is unreachable or nothing matches, the
  receipt simply has no price info and the team confirms totals as usual.
- Menu rows that carry the drink size in the NAME ("DARK MOCHA 8OZ") or in
  the DESCRIPTION ("8oz") are grouped into one base drink so "white choco
  mocha" can resolve to its 8oz/12oz/16oz variants.
- Fuzzy matching (difflib >= 0.75) handles casual spellings: "white choco
  mocha" -> "WHITE CHOCOLATE MOCHA".
- No asyncio.Lock on the cache: races at most fire one extra SELECT, which is
  harmless, and a module-level lock would break across pytest event loops.

The absolute rule stands: totals are ESTIMATES ONLY — the team always
confirms the final total in the chat.
"""
from __future__ import annotations

import difflib
import logging
import re
import time
from dataclasses import dataclass
from typing import Any, Dict, List, Optional, Tuple
from urllib.parse import unquote, urlparse

import aiomysql

import config

log = logging.getLogger("menu")

FUZZY_THRESHOLD = 0.75
_FAIL_COOLDOWN_SEC = 30.0  # when the DB is down, retry at most this often

# ---------------------------------------------------------------------------
# Normalization + data model
# ---------------------------------------------------------------------------
_NONWORD_RE = re.compile(r"[^\w\s]+", re.UNICODE)
_SIZE_RE = re.compile(r"\b(\d{1,2})\s*oz\b", re.IGNORECASE)  # sizes up to 99oz
_TEMP_RE = re.compile(r"\b(iced|hot|cold|frappe|blended)\b", re.IGNORECASE)


def _normalize(text: str) -> str:
    return " ".join(_NONWORD_RE.sub(" ", (text or "").lower()).split())


def _strip_size_and_temperature(text: str) -> str:
    """Group key: drink names minus sizes and minus iced/hot/cold tokens."""
    low = _SIZE_RE.sub(" ", _normalize(text))
    low = _TEMP_RE.sub(" ", low)
    return " ".join(low.split())


def _temperature_of(text: str) -> Optional[bool]:
    """True = iced/cold, False = hot, None = unspecified."""
    if re.search(r"\b(?:iced|cold|frappe|blended)\b", text, re.IGNORECASE):
        return True
    if re.search(r"\bhot\b", text, re.IGNORECASE):
        return False
    return None


def _fmt_price(value: float) -> str:
    if abs(value - round(value)) < 0.005:
        return str(int(round(value)))
    return f"{value:.2f}"


@dataclass(frozen=True)
class MenuItem:
    item_id: str
    name: str        # display name exactly as stored in the POS (uppercase)
    category: str
    price: float
    size: str        # "8oz"/"16oz"/"" — from the name, else the description
    popular: bool
    norm: str        # normalized name (exact/fuzzy target)
    base: str        # norm without size/temp tokens (variant group key)
    temp: Optional[bool]  # True iced/cold · False hot · None unspecified

    @property
    def display(self) -> str:
        """Display name with the size appended when it isn't already visible."""
        name = self.name.strip()
        if self.size and not re.search(r"\d+\s*oz", name, re.IGNORECASE):
            name = f"{name} ({self.size.upper()})"
        return name

    def variant_label(self) -> str:
        """Short label for alternates, e.g. 'iced 16oz' or 'hot'."""
        if self.size:
            temp = "iced " if self.temp is True else ("hot " if self.temp is False else "")
            return f"{temp}{self.size}"
        label = self.category.strip().lower() or self.norm or self.name.strip().lower()
        if self.temp is False:
            label = f"hot {label}"
        return label


@dataclass
class Receipt:
    lines: List[str]   # "2× WHITE CHOCOLATE MOCHA (8OZ) — ₱258"
    total: float       # sum of priced lines (0.0 when nothing was priced)
    has_prices: bool   # False when the menu is unavailable or nothing matched
    alternates: List[str]  # e.g. "iced 16oz ₱169" for other sizes of a hit

    @property
    def ok(self) -> bool:
        return bool(self.lines)


# ---------------------------------------------------------------------------
# Cached live fetch
# ---------------------------------------------------------------------------
_items: List[MenuItem] = []
_loaded_at = 0.0
_last_fail_at = 0.0


async def _fetch_rows() -> List[Dict[str, Any]]:
    """One read-only SELECT against the POS DB. Raises on any failure."""
    url = urlparse(config.settings.pos_mysql_url)
    if not url.hostname or not url.path.strip("/"):
        return []  # POS_MYSQL_URL not configured -> no live prices (by design)
    conn = await aiomysql.connect(
        host=url.hostname,
        port=url.port or 3306,
        user=url.username or "",
        password=unquote(url.password or ""),
        db=url.path.strip("/"),
    )
    try:
        cur = await conn.cursor()
        await cur.execute(
            "SELECT id, name, category, price, description, popular"
            " FROM menu_items ORDER BY id"
        )
        rows = await cur.fetchall()
        return [
            {
                "id": r[0],
                "name": r[1],
                "category": r[2],
                "price": r[3],
                "description": r[4],
                "popular": r[5],
            }
            for r in rows
        ]
    finally:
        conn.close()


def _to_items(rows: List[Dict[str, Any]]) -> List[MenuItem]:
    items: List[MenuItem] = []
    for r in rows:
        name = str(r.get("name") or "").strip()
        if not name:
            continue
        desc = str(r.get("description") or "")
        m = _SIZE_RE.search(name) or _SIZE_RE.search(desc)
        size = f"{int(m.group(1))}oz" if m else ""
        price = float(r.get("price") or 0.0)
        norm = _normalize(name)
        if price <= 0:  # malformed row — never quote a zero/None price
            continue
        items.append(
            MenuItem(
                item_id=str(r.get("id") or ""),
                name=name,
                category=str(r.get("category") or "").strip(),
                price=price,
                size=size or "",
                popular=bool(r.get("popular")),
                norm=norm,
                base=_strip_size_and_temperature(name),
                temp=_temperature_of(name),
            )
        )
    return items


def reset_cache() -> None:
    """Drop the cache — used by tests and safe to call any time."""
    global _items, _loaded_at, _last_fail_at
    _items = []
    _loaded_at = 0.0
    _last_fail_at = 0.0


async def get_menu(force: bool = False) -> List[MenuItem]:
    """Menu rows from the live POS DB with a short TTL cache.

    Returns [] when the DB is unreachable or unconfigured; never raises.
    """
    global _items, _loaded_at, _last_fail_at
    now = time.monotonic()
    ttl = max(5, config.settings.menu_cache_ttl_sec)
    if _items and now - _loaded_at < ttl:
        return _items
    if not force and now - _last_fail_at < _FAIL_COOLDOWN_SEC:
        return _items  # cold + recently failed: don't hammer a down DB
    try:
        rows = await _fetch_rows()
        _items = _to_items(rows)
        _loaded_at = now
        log.info("menu loaded", extra={"count": len(_items)})
    except Exception as exc:  # aiomysql/network/DB errors of any kind
        _last_fail_at = now
        log.warning("menu fetch failed", extra={"error": type(exc).__name__})
    return _items


# ---------------------------------------------------------------------------
# Matching + receipt building
# ---------------------------------------------------------------------------
def _match_score(query: str, row: MenuItem, q_temp: Optional[bool]) -> float:
    q = _normalize(query)
    s = max(
        difflib.SequenceMatcher(None, q, row.norm).ratio(),
        difflib.SequenceMatcher(None, q, row.base).ratio(),
    )
    # Containment boost, conservative: only for >=5-char queries. Lets
    # "cookies" hit "CHOCOLATE COOKIES" without letting short words like
    # "tea" match half the menu.
    if len(q) >= 5 and (q in row.norm or row.norm in q):
        s = max(s, 0.76)
    if q_temp is not None and row.temp is not None:
        if q_temp == row.temp:
            s += 0.03  # temperature agreement is a strong human hint
        else:
            s -= 0.03
    return s


def best_match(query: str, menu: List[MenuItem]) -> Optional[MenuItem]:
    """Best fuzzy match for one food phrase, or None below the threshold."""
    q = _normalize(query)
    if not q:
        return None
    q_temp = _temperature_of(q)
    best: Optional[MenuItem] = None
    best_score = 0.0
    for row in menu:
        score = _match_score(q, row, q_temp)
        if score > best_score:
            best, best_score = row, score
    if best is None or best_score < FUZZY_THRESHOLD:
        return None
    # If the customer named a size ("...16oz"), prefer that variant within
    # the matched drink's family.
    size_m = _SIZE_RE.search(q)
    if size_m:
        wanted = f"{int(size_m.group(1))}oz"
        sized = [r for r in menu if r.base == best.base and r.size == wanted]
        if sized:
            return sized[0]
    return best


def parse_items(text: str) -> List[Tuple[int, str]]:
    """Split a free-text order into (quantity, food-phrase) pairs.

    Delimiters: '+', ',', ';' and the Filipino/English conjunctions
    ' at ' / ' and '. Quantity = leading number (capped at 99), defaulting
    to 1. "Pcs"/"x" after the number are ignored.
    """
    out: List[Tuple[int, str]] = []
    for chunk in re.split(r"[,;\n+]|\band\b|\bat\b", text, flags=re.IGNORECASE):
        raw = " ".join(chunk.split())
        if not raw:
            continue
        qty, rest = 1, raw
        m = re.match(r"^(\d{1,3})\s*(.*)$", raw)
        if m:
            qty = min(99, int(m.group(1)))
            rest = m.group(2).strip()
        rest = re.sub(r"^(?:pcs?|pieces?|x|×)\s*", "", rest, flags=re.IGNORECASE)
        rest = _SIZE_RE.sub("", rest)  # sizes don't help matching
        rest = " ".join(rest.split())
        if not rest:
            rest = raw.split()[-1]  # bare number message: keep something to match
        out.append((qty, rest))
    return out


async def build_receipt(raw_items: str) -> Receipt:
    """Turn a free-text items string into priced receipt lines.

    Never raises, never invents prices: when the POS menu is unavailable or
    nothing matches, `has_prices` stays False and callers keep the free-text
    description only (the team confirms totals, as always).
    """
    raw = " ".join((raw_items or "").split())
    menu = await get_menu()
    if not menu:
        return Receipt(lines=[raw] if raw else [], total=0.0, has_prices=False, alternates=[])
    try:
        lines: List[str] = []
        alternates: List[str] = []
        total = 0.0
        has_prices = False
        for qty, food in parse_items(raw):
            item = best_match(food, menu)
            if item is None:
                lines.append(f"{qty}× {food.title()} — (presyo titingnan ng team)")
                continue
            has_prices = True
            line_total = qty * item.price
            total += line_total
            lines.append(f"{qty}× {item.display} — ₱{_fmt_price(line_total)}")
            sibs = [s for s in menu if s.base == item.base and s.item_id != item.item_id]
            if sibs:
                labels = ", ".join(f"{s.variant_label()} ₱{_fmt_price(s.price)}" for s in sibs)
                alternates.append(f"(May ibang sizes po: {labels} — sabihin lang po kung palit.)")
        if not lines:
            return Receipt(lines=[raw] if raw else [], total=0.0, has_prices=False, alternates=[])
        return Receipt(lines=lines, total=round(total, 2), has_prices=has_prices, alternates=alternates)
    except Exception:
        log.exception("receipt build failed")
        return Receipt(lines=[raw] if raw else [], total=0.0, has_prices=False, alternates=[])
