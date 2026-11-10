"""FAQ content engine.

`HotFile` re-reads a file whenever its mtime changes, so faq.yaml and
knowledge.md can be edited live without rebuilding the container.

FAQ entries (faq.yaml, Taglish brand voice, fully editable):

```yaml
entries:
  - id: menu
    payloads: [MENU]        # optional: postback / quick-reply payloads
    keywords: [menu, magkano, presyo]   # whole-word matches
    patterns: ["price list"]            # case-insensitive substrings
    regex: ["\\bprices?\\b"]            # raw regexes on the lowercased text
    answer: |
      ...
    quick_replies:                      # optional suggestions under the answer
      - title: Hours & Location
        payload: HOURS
```
"""
from __future__ import annotations

import logging
import re
from pathlib import Path
from typing import Callable, Dict, Generic, Optional, Tuple, TypeVar, Union

import yaml

from dataclasses import dataclass, field

log = logging.getLogger("faq")

T = TypeVar("T")


class HotFile(Generic[T]):
    """Caches a value parsed from a file; reloads automatically on mtime change."""

    def __init__(self, path: str, loader: Callable[[Path], T], name: str):
        self.path = Path(path)
        self.loader = loader
        self.name = name
        self._mtime: Optional[float] = None
        self._value: Optional[T] = None

    def get(self) -> T:
        try:
            mtime = self.path.stat().st_mtime
        except OSError:
            if self._value is not None:
                return self._value
            log.error("hot file missing", extra={"name": self.name, "path": self.path.as_posix()})
            raise  # caller decides what a missing prompt/faq means at first load
        if self._value is None or mtime != self._mtime:
            self._value = self.loader(self.path)
            self._mtime = mtime
            log.info("hot file loaded", extra={"hot_file": self.name, "path": self.path.as_posix()})
        return self._value


@dataclass(frozen=True)
class QRTile:
    title: str
    payload: str

    def as_pair(self) -> Tuple[str, str]:
        return (self.title, self.payload)


@dataclass(frozen=True)
class Entry:
    id: str
    payloads: Tuple[str, ...] = ()
    keywords: Tuple[str, ...] = ()
    patterns: Tuple[str, ...] = ()
    regexes: Tuple[re.Pattern[str], ...] = ()
    answer: str = ""
    quick_replies: Tuple[QRTile, ...] = ()
    payload_index: Dict[str, "Entry"] = field(default_factory=dict, compare=False, repr=False)


def _as_str_list(raw: Union[None, str, list]) -> Tuple[str, ...]:
    if raw is None:
        return ()
    if isinstance(raw, str):
        return (raw,)
    return tuple(str(item) for item in raw)


def _safe_regex(pattern: str) -> Optional[re.Pattern[str]]:
    try:
        return re.compile(pattern, re.IGNORECASE)
    except re.error:
        log.error("bad regex in faq.yaml ignored", extra={"regex": pattern})
        return None


def _load_entry(raw: object) -> Optional[Entry]:
    if not isinstance(raw, dict):
        return None
    eid = str(raw.get("id", "")).strip()
    if not eid:
        return None
    qrs = tuple(
        QRTile(title=str(t.get("title", ""))[:20], payload=str(t.get("payload", "")))
        for t in raw.get("quick_replies", []) or []
        if isinstance(t, dict) and t.get("title") and t.get("payload")
    )
    regexes = tuple(p for p in map(_safe_regex, _as_str_list(raw.get("regex"))) if p)
    return Entry(
        id=eid,
        payloads=tuple(p.strip().upper() for p in _as_str_list(raw.get("payloads"))),
        keywords=tuple(k.strip().lower() for k in _as_str_list(raw.get("keywords"))),
        patterns=tuple(p.lower() for p in _as_str_list(raw.get("patterns"))),
        regexes=regexes,
        answer=str(raw.get("answer", "")).strip(),
        quick_replies=qrs,
    )


def load_faq(path: Path) -> Dict[str, Dict[str, Entry]]:
    """Parse faq.yaml -> {"by_id": {...}, "by_payload": {...}}."""
    try:
        with path.open("r", encoding="utf-8") as fh:
            raw = yaml.safe_load(fh)
    except (OSError, yaml.YAMLError) as exc:
        log.error("cannot read faq.yaml (keeping previous if any)", extra={"error": str(exc)})
        if not path.exists():
            return {"by_id": {}, "by_payload": {}}
        raise
    by_id: Dict[str, Entry] = {}
    by_payload: Dict[str, Entry] = {}
    entries_raw = raw.get("entries", []) if isinstance(raw, dict) else []
    for item in entries_raw:
        entry = _load_entry(item)
        if entry is None:
            continue
        by_id[entry.id] = entry
        for payload in entry.payloads:
            if payload and payload not in by_payload:
                by_payload[payload] = entry
    return {"by_id": by_id, "by_payload": by_payload}


def clear_cache() -> None:
    """Forget cached mtimes so tests / fresh pointers reload from disk."""
    if _faq_hot is not None:
        _faq_hot._mtime = None
        _faq_hot._value = None


_faq_hot: Optional[HotFile[Dict[str, Dict[str, Entry]]]] = None


def _init_hot() -> HotFile[Dict[str, Dict[str, Entry]]]:
    global _faq_hot
    if _faq_hot is None:
        import config

        _faq_hot = HotFile(config.settings.faq_path, load_faq, "faq.yaml")
    return _faq_hot


def _faq() -> Dict[str, Dict[str, Entry]]:
    hot = _init_hot()
    try:
        return hot.get()
    except FileNotFoundError:
        # faq.yaml is missing: run with no static answers, but let the
        # hot-reloader pick it up as soon as the owner creates the file.
        log.error(
            "faq.yaml not found; running with no static answers",
            extra={"path": hot.path.as_posix()},
        )
        if hot._value is None:
            hot._value = {"by_id": {}, "by_payload": {}}
        return hot._value


def payload_entry(payload: str) -> Optional[Entry]:
    """Find an entry by postback / quick-reply payload (case-insensitive)."""
    index = _faq()["by_payload"]
    return index.get((payload or "").strip().upper())


def entry_by_id(entry_id: str) -> Optional[Entry]:
    return _faq()["by_id"].get(entry_id)


def text_entry(text: str) -> Optional[Entry]:
    """Keyword / substring / regex match against faq.yaml (first match wins)."""
    if not text:
        return None
    low = text.lower().strip()
    for entry in _faq()["by_id"].values():
        if any(pat in low for pat in entry.patterns):
            return entry
        if any(rx.search(low) for rx in entry.regexes):
            return entry
        if any(re.search(rf"\b{re.escape(kw)}\b", low) for kw in entry.keywords):
            return entry
    return None


def all_payloads() -> Tuple[str, ...]:
    return tuple(_faq()["by_payload"].keys())
