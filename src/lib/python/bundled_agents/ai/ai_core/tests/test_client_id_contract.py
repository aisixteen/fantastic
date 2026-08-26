"""The `client_id` round-trip contract — pinned, character by character.

Field-measured 2026-08-26 (dataroom session hub): the hub encoded correlation
into client_id ("cid#turn"), ai_core silently rewrote `#` to `_`, and the
demux broke — an hour lost to an undocumented alphabet. This file IS the
documentation: exactly which characters survive `_safe_client` verbatim, what
everything else becomes, and that the sanitized id is what every downstream
surface (token events, status events, persisted history key) actually carries.

The safe alphabet: alphanumerics plus `.` `_` `-`. Anything else → `_`,
silently (a rename, not a refusal — sanitization exists because the id becomes
a filename suffix). A consumer packing structure into client_id must use only
the safe alphabet — `cid__turn` works, `cid#turn` does not.
"""

from __future__ import annotations

from ai_core.core import DEFAULT_CLIENT_ID, _safe_client


def test_the_safe_alphabet_survives_verbatim():
    for cid in (
        "cli",
        "tab1",
        "cid__turn7",
        "a.b-c_d",
        "UPPER.lower-123_",
        "0",
        "...",
        "---",
        "___",
    ):
        assert _safe_client(cid) == cid, cid


def test_everything_else_becomes_underscore_silently():
    cases = {
        "cid#turn": "cid_turn",  # the measured hour-loser
        "a b": "a_b",
        "a/b": "a_b",
        "a\\b": "a_b",
        "a:b": "a_b",
        "a@b!c": "a_b_c",
        "тест": "____",  # non-ASCII alnum? see next test — this is cyrillic
    }
    # NOTE on the last row: `str.isalnum` is UNICODE-aware, so cyrillic/CJK
    # letters actually SURVIVE — pin the truth, not the guess:
    cases.pop("тест")
    for raw, want in cases.items():
        assert _safe_client(raw) == want, raw


def test_unicode_alphanumerics_survive_isalnum_is_unicode_aware():
    """The alphabet is UNICODE alnum, not ASCII — `тест` and `日本語` pass
    through verbatim. A consumer wanting ASCII-only must enforce it itself."""
    assert _safe_client("тест") == "тест"
    assert _safe_client("日本語7") == "日本語7"
    assert _safe_client("naïve") == "naïve"


def test_empty_and_whitespace_fall_back_to_the_default():
    assert _safe_client("") == DEFAULT_CLIENT_ID
    assert _safe_client("   ") == DEFAULT_CLIENT_ID
    assert _safe_client(None) == DEFAULT_CLIENT_ID


def test_two_distinct_raw_ids_can_collide_after_sanitization():
    """The trap behind the trap: `a#b` and `a/b` BOTH become `a_b` — two
    browser tabs with 'different' ids can silently share one history file and
    one event filter. Correlation schemes must stay inside the safe alphabet
    to keep ids injective."""
    assert _safe_client("a#b") == _safe_client("a/b") == "a_b"
