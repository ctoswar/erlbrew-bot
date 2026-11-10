"""Central configuration for the Erlbrew Café Messenger bot.

All configuration comes from environment variables (see .env.example), with
sane defaults so the bot also runs locally without Compose.  The module also
sets up structured (JSON) logging and owns the canned brand-voice messages.
"""
from __future__ import annotations

import json
import logging
import os
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Set

BASE_DIR = Path(__file__).resolve().parent


def _str(name: str, default: str = "") -> str:
    raw = os.getenv(name)
    if raw is None or raw.strip() == "":
        return default
    return raw.strip()


def _int(name: str, default: int) -> int:
    raw = os.getenv(name)
    if raw is None or raw.strip() == "":
        return default
    try:
        return int(raw.strip())
    except ValueError:
        logging.getLogger("config").warning("bad int for %s, using %d", name, default)
        return default


def _float(name: str, default: float) -> float:
    raw = os.getenv(name)
    if raw is None or raw.strip() == "":
        return default
    try:
        return float(raw.strip())
    except ValueError:
        logging.getLogger("config").warning("bad float for %s, using %s", name, default)
        return default


def _bool(name: str, default: bool) -> bool:
    raw = os.getenv(name)
    if raw is None or raw.strip() == "":
        return default
    return raw.strip().lower() in {"1", "true", "yes", "on", "y"}


def _resolve_path(env_name: str, candidates: list[str]) -> str:
    """Prefer the explicit env var, then the first existing candidate path.

    This lets docker-compose mount ./data (live-editable files) while still
    falling back to the bundled copies inside the image.
    """
    given = _str(env_name)
    if given:
        return given
    for cand in candidates:
        if Path(cand).is_absolute() and Path(cand).exists():
            return cand
    for cand in candidates:
        if (BASE_DIR / cand).exists():
            return str(BASE_DIR / cand)
    return str(BASE_DIR / candidates[0])


@dataclass(frozen=True)
class Settings:
    # Meta / Facebook
    app_secret: str
    verify_token: str
    page_access_token: str
    graph_version: str
    # Admin
    admin_token: str
    # Ollama LLM
    ollama_url: str
    ollama_model: str
    ollama_think: bool
    ollama_num_predict: int
    ollama_num_ctx: int
    ollama_temperature: float
    ollama_keep_alive: str
    llm_timeout_sec: float
    llm_max_history: int
    llm_history_ttl_sec: int
    typing_resend_sec: float
    # Telegram
    telegram_bot_token: str
    telegram_chat_id: str
    # POS menu (live prices during order intake)
    pos_mysql_url: str
    menu_cache_ttl_sec: int
    # Behaviour
    event_reply_hours: int
    db_path: str
    faq_path: str
    knowledge_path: str
    rate_limit_max: int
    rate_limit_window_sec: int
    max_text_chars: int
    event_form_ttl_sec: int
    cleanup_interval_sec: int
    port: int
    log_level: str


def load_settings() -> Settings:
    return Settings(
        app_secret=_str("APP_SECRET"),
        verify_token=_str("VERIFY_TOKEN"),
        page_access_token=_str("PAGE_ACCESS_TOKEN"),
        graph_version=_str("GRAPH_VERSION", "v23.0"),
        admin_token=_str("ADMIN_TOKEN"),
        # OLLAMA_BASE_URL is canonical; OLLAMA_URL kept as an alias.
        ollama_url=(_str("OLLAMA_BASE_URL") or _str("OLLAMA_URL", "http://host.docker.internal:11434")).rstrip("/"),
        ollama_model=_str("OLLAMA_MODEL", "qwen3.5:2b"),
        ollama_think=_bool("OLLAMA_THINK", False),
        ollama_num_predict=_int("OLLAMA_NUM_PREDICT", 120),
        ollama_num_ctx=_int("OLLAMA_NUM_CTX", 2048),
        ollama_temperature=_float("OLLAMA_TEMPERATURE", 0.2),
        ollama_keep_alive=_str("OLLAMA_KEEP_ALIVE", "30m"),
        llm_timeout_sec=_float("LLM_TIMEOUT_SEC", 25.0),
        llm_max_history=_int("LLM_MAX_HISTORY", 6),
        llm_history_ttl_sec=_int("LLM_HISTORY_TTL_HOURS", 24) * 3600,
        typing_resend_sec=_float("TYPING_RESEND_SEC", 10.0),
        telegram_bot_token=_str("TELEGRAM_BOT_TOKEN"),
        telegram_chat_id=_str("TELEGRAM_CHAT_ID"),
        # Read-only POS menu DB; empty simply disables live prices (the order
        # flow degrades gracefully to free-text items with no price lines).
        pos_mysql_url=_str("POS_MYSQL_URL"),
        menu_cache_ttl_sec=_int("MENU_CACHE_TTL_SEC", 60),
        event_reply_hours=_int("EVENT_REPLY_HOURS", 24),
        db_path=_str("DB_PATH", "data/erlbrew.db"),
        faq_path=_resolve_path("FAQ_PATH", ["data/faq.yaml", "faq.yaml"]),
        knowledge_path=_resolve_path("KNOWLEDGE_PATH", ["data/knowledge.md", "knowledge.md"]),
        rate_limit_max=_int("RATE_LIMIT_MAX", 15),
        rate_limit_window_sec=_int("RATE_LIMIT_WINDOW_SEC", 600),
        max_text_chars=_int("MAX_TEXT_CHARS", 500),
        event_form_ttl_sec=_int("EVENT_FORM_TTL_HOURS", 6) * 3600,
        cleanup_interval_sec=_int("CLEANUP_INTERVAL_SEC", 900),
        port=_int("PORT", 8000),
        log_level=_str("LOG_LEVEL", "INFO").upper(),
    )


settings = load_settings()


# --------------------------------------------------------------------------
# Structured (JSON) logging.  Never log tokens or secrets: no module should
# pass them as fields; URLs with tokens are never constructed into logs.
# --------------------------------------------------------------------------
_STD_LOG_ATTRS: Set[str] = set(vars(logging.LogRecord("", 0, "", 0, "", (), None))) | {
    "message",
    "asctime",
}


class JSONFormatter(logging.Formatter):
    def format(self, record: logging.LogRecord) -> str:
        payload: Dict[str, Any] = {
            "ts": self.formatTime(record, "%Y-%m-%dT%H:%M:%S%z"),
            "level": record.levelname,
            "logger": record.name,
            "msg": record.getMessage(),
        }
        if record.exc_info and record.exc_info[0] is not None:
            payload["exc"] = self.formatException(record.exc_info).splitlines()[-1]
        for key, value in record.__dict__.items():
            if key in _STD_LOG_ATTRS or key in ("exc_info", "exc_text", "stack_info"):
                continue
            if key not in payload:
                payload[key] = value
        return json.dumps(payload, ensure_ascii=False, default=str)


def setup_logging() -> None:
    level = getattr(logging, settings.log_level, logging.INFO)
    handler = logging.StreamHandler(sys.stdout)
    handler.setFormatter(JSONFormatter())
    root = logging.getLogger()
    root.handlers[:] = [handler]
    root.setLevel(level)
    # Quieten third-party noise.
    logging.getLogger("httpx").setLevel(logging.WARNING)
    logging.getLogger("httpcore").setLevel(logging.WARNING)
    logging.getLogger("aiosqlite").setLevel(logging.WARNING)


def warn_missing_config() -> None:
    problems = [
        name
        for name, value in (
            ("APP_SECRET", settings.app_secret),
            ("VERIFY_TOKEN", settings.verify_token),
            ("PAGE_ACCESS_TOKEN", settings.page_access_token),
        )
        if not value
    ]
    if problems:
        logging.getLogger("config").error(
            "missing required config; webhook POSTs will be rejected: %s", ", ".join(problems)
        )
    if not settings.admin_token:
        logging.getLogger("config").error("ADMIN_TOKEN not set; /admin/unpause rejected")
    if not settings.telegram_bot_token or not settings.telegram_chat_id:
        logging.getLogger("config").warning("Telegram notifications disabled (not configured)")


# --------------------------------------------------------------------------
# Canned messages (friendly Taglish brand voice)
# --------------------------------------------------------------------------
RATE_LIMITED_TEXT = (
    "Pasensya na po, sobrang dami po ng messages ninyo 😅 "
    "Sandali lang po ulit, salamat po sa pag-intindi! ☕"
)
TOO_LONG_TEXT = (
    "Mukhang medyo mahaba po yung message na yan. Paki-shorten naman po "
    f"(max {settings.max_text_chars} characters) para masagot ko agad. Salamat po! 😊"
)
NON_TEXT_TEXT = (
    "Salamat po! Pero yung tinatanong ko po, pwede nyo po bang i-type as text? "
    "I-type nyo lang po yung tanong nyo dito para masagot ko agad. 😊"
)
HANDOFF_TEXT = (
    "Sige po, ipapasa ko po ito sa aming team para matulungan kayo agad. 🙂 "
    "I-message po kayo nila dito mismo sa Messenger. Salamat po sa pasensya!"
)
LLM_FALLBACK_TEXT = "Sandali lang po, ipapasa ko sa team namin."
RATE_NOTICE_KIND = "rate"
LEN_NOTICE_KIND = "toolong"
