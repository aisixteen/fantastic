"""Every row has an address — an int from the kernel's one counter; parent is
causality, reply_to is delivery. A thread is the parent chain — derived, never stored
(measured unnecessary through 16 threads)."""

from __future__ import annotations

from ai_core.core import _REPLY_TAG, _mint_row_id, _reply_tag, _row_ids_for_incoming
from ai_core.strategies.strip import _PID

from _fake import FakeCtx  # noqa: E402


def test_an_external_message_is_a_root():
    k = FakeCtx()
    rid, parent = _row_ids_for_incoming("ai", "please survey the corpus", k)
    assert isinstance(rid, int) and rid == 1 and parent is None
    assert _row_ids_for_incoming("ai", "again", k)[0] == 2, "the counter, in order"


def test_a_delivered_reply_turn_names_its_parent_from_the_tag():
    text = '[reply to 40 · notes read · "read the spec" · ↑17]\nok'
    rid, parent = _row_ids_for_incoming("ai", text, FakeCtx())
    assert parent == 17 and rid != parent


def test_the_reply_tag_carries_every_segment_in_order_and_still_parses():
    tag = _reply_tag(40, "notes", "read", "read the spec", 17, 2, 3, 17)
    assert tag == (
        '[reply to 40 · notes read · "read the spec" · ↑17 · 2 of 3 in batch 17]'
    )
    m = _REPLY_TAG.match(tag + "\nbody")
    assert m and m.group(1) == "notes" and m.group(2) == "read"
    assert _PID.match(tag).group(1) == "40"


def test_optional_segments_are_omitted_cleanly():
    assert _reply_tag(1, "t", "v", None, None, 1, 1, "x") == "[reply to 1 · t v]"
    assert _reply_tag(1, "t", "v", None, 5, 1, 1, 5) == "[reply to 1 · t v · ↑5]"


def test_the_batch_token_is_the_assistant_rows_id():
    turn = _mint_row_id("ai", FakeCtx())
    tag = _reply_tag(1, "t", "v", None, turn, 1, 2, turn)
    assert f"in batch {turn}" in tag and f"↑{turn}" in tag


def test_a_self_call_inherits_the_conversation_it_was_made_in():
    """`_route_reply` dispatches the model's calls after the turn closed, so a verb that
    resolves its conversation from `_current` would see the default client. The
    conversation rides on the payload instead. Asserted on the source of the seam."""
    import inspect
    from ai_core import core

    src = inspect.getsource(core._route_reply)
    assert 'payload = {**payload, "client_id": client_id}' in src
