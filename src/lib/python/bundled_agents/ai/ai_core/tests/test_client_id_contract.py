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

from _testkit import memory_meta
import asyncio

from ai_core import core
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


# ── the queue row a cancelled WAIT used to leave behind ──────────────────────
# A send is enqueued BEFORE it waits on the per-backend FIFO lock, and was
# dequeued only inside the lock body. An item `timeout` (or an operator
# `cancel`) landing during that wait raised straight out of `acquire()`, so the
# row was never popped: `status` reported a phantom pending caller forever and
# every later caller was told `ahead: 1` for work that would never run.


class _Blocked:
    """A provider that never finishes — it holds the FIFO lock until released."""

    def __init__(self):
        self.release = asyncio.Event()

    async def chat(self, messages, max_tokens=None):
        await self.release.wait()
        yield "done"


async def test_a_send_cancelled_while_queued_leaves_no_phantom_queue_entry(
    seeded_kernel,
):
    aid = "cid_queue"
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    blocked = _Blocked()
    core._providers[aid] = blocked
    holder = asyncio.create_task(
        seeded_kernel.send(
            [{"target": aid, "payload": {"type": "send", "text": "first"}}]
        )
    )
    try:
        # wait until the first send actually HOLDS the lock
        for _ in range(200):
            if core._current.get(aid) is not None:
                break
            await asyncio.sleep(0.02)
        assert core._current.get(aid) is not None, "the holder never took the lock"

        # a second caller queues behind it and is killed by its OWN item deadline
        out = (
            await seeded_kernel.send(
                [
                    {
                        "target": aid,
                        "payload": {
                            "type": "send",
                            "text": "second",
                            "client_id": "tab2",
                        },
                        "timeout": 0.3,
                    }
                ]
            )
        )[0]
        assert out["status"] == "timed_out", out

        # the row is GONE — not just absent from this caller's view.
        assert core._queue.get(aid, []) == []
        snap = (
            await seeded_kernel.send([{"target": aid, "payload": {"type": "status"}}])
        )[0]["result"]
        assert snap["others_pending"] == 0, snap
        assert snap["generating"] is True, "the holder is still the in-flight turn"
        # …and a THIRD caller is told the truth about the line ahead of it.
        assert len(core._queue.get(aid, [])) == 0
    finally:
        blocked.release.set()
        try:
            await asyncio.wait_for(holder, 5)
        except (TimeoutError, asyncio.CancelledError):
            holder.cancel()
        core._providers.pop(aid, None)
        await asyncio.sleep(0.1)
