"""FAQ engine: keyword/regex matching, payload routing, hot reloading."""
from __future__ import annotations

import os
import time

import faq
from faq import HotFile


def test_payload_routing_from_faq_yaml():
    assert faq.payload_entry("MENU").id == "menu"
    assert faq.payload_entry("menu").id == "menu"
    assert faq.payload_entry("GET_STARTED").id == "get_started"
    assert faq.payload_entry("PROMO").id == "promo"
    assert faq.payload_entry("EVENTS").id == "events"
    assert faq.payload_entry("HOURS").id == "hours"
    assert faq.payload_entry("UNKNOWN_XYZ") is None


def test_keyword_matching():
    assert faq.text_entry("Anong oras po bukas kayo?").id == "hours"
    assert faq.text_entry("What are your hours?").id == "hours"
    assert faq.text_entry("magkano ang Spanish latte?").id == "menu"
    assert faq.text_entry("magkano po presyo?").id == "menu"
    assert faq.text_entry("May GCash ba kayo?").id == "payment"
    assert faq.text_entry("pwede ba mag-parking?").id == "parking"
    assert faq.text_entry("may wifi po ba?").id == "wifi"
    assert faq.text_entry("delivery ba kayo?").id == "delivery"
    assert faq.text_entry("metre-price").id == "menu"  # 'price' after the dash


def test_word_boundary_prevents_false_positive():
    # 'book' must not match inside 'facebook'
    entry = faq.text_entry("check niyo yung facebook page nyo")
    assert entry is None or entry.id != "events"


def test_no_match_returns_none():
    assert faq.text_entry("zzz qqq beaconstring") is None
    assert faq.text_entry("") is None


def test_quick_replies_roundtrip():
    entry = faq.payload_entry("EVENTS")
    pairs = [(qr.title, qr.payload) for qr in entry.quick_replies]
    assert ("Start booking", "EVT_START") in pairs


def test_hotfile_reloads_on_mtime_change(tmp_path):
    target = tmp_path / "hot.txt"
    target.write_text("one", encoding="utf-8")
    hot = HotFile(str(target), loader=lambda p: p.read_text(encoding="utf-8"), name="hot-test")

    assert hot.get() == "one"
    target.write_text("two", encoding="utf-8")
    os.utime(target, (time.time() + 5, time.time() + 5))
    assert hot.get() == "two"

    target.unlink()
    assert hot.get() == "two"  # missing file keeps last good value


def test_load_faq_tolerates_incomplete_entries(tmp_path):
    bad = tmp_path / "broken.yaml"
    bad.write_text("entries: [ {id: no_answer_body} ]", encoding="utf-8")
    parsed = faq.load_faq(bad)
    assert "no_answer_body" in parsed["by_id"]
    assert parsed["by_id"]["no_answer_body"].answer == ""


def test_load_faq_survives_bad_regex(tmp_path):
    bad = tmp_path / "broken_regex.yaml"
    bad.write_text(
        "entries:\n"
        "  - id: x\n"
        "    keywords: [hello]\n"
        "    regex: ['([unclosed']\n"
        "    answer: hi\n",
        encoding="utf-8",
    )
    parsed = faq.load_faq(bad)  # must not raise
    assert parsed["by_id"]["x"].regexes == ()
    assert faq.text_entry.__module__  # engine still usable
