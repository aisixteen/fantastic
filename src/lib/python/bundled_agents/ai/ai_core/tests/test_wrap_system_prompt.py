"""`pre_system_prompt` / `post_system_prompt` (owner, 2026-10-11): record text placed
BEFORE and AFTER the system block on every turn — whichever block is in force (the
default, the record's `system_prompt`, a per-call one), never replacing it; verbatim,
no placeholders. A value the record cannot use is refused AT THE WRITE; null clears."""

from __future__ import annotations

import pytest
from test_record_system_prompt import DONE, LIST, _Fake, _ai, _op, _send, _sys, _until

from ai_core.schema import WRAP_PROMPT_REFUSAL


async def test_pre_and_post_wrap_the_default_block(seeded_kernel, fake_providers):
    k = seeded_kernel
    _ai(k, "wr_plain")
    fp0 = fake_providers["wr_plain"] = _Fake([DONE])
    await _send(k, "wr_plain")
    default = _sys(fp0.requests[0])

    _ai(k, "wr_def", pre_system_prompt="PRE {tree}", post_system_prompt="POST")
    fp = fake_providers["wr_def"] = _Fake([LIST, DONE])
    await _send(k, "wr_def")
    await _until(fp, 2)
    for req in fp.requests:  # the main turn and the reply-woken one alike
        sys = _sys(req)
        assert sys.startswith("PRE {tree}\n\n"), sys[:80]  # verbatim, no fill
        assert sys.endswith("\n\nPOST"), sys[-80:]
        body = sys[len("PRE {tree}\n\n") : -len("\n\nPOST")]
        # the default block stays whole between them (its own id differs)
        assert body.replace("wr_def", "wr_plain") == default


async def test_they_wrap_a_record_and_a_per_call_prompt(seeded_kernel, fake_providers):
    k = seeded_kernel
    _ai(
        k, "wr_rec", system_prompt="ROLE", pre_system_prompt="A", post_system_prompt="Z"
    )
    fp = fake_providers["wr_rec"] = _Fake([DONE, DONE])
    await _send(k, "wr_rec")
    await _send(k, "wr_rec", system_prompt="CALL")
    await _until(fp, 2)
    assert [_sys(r) for r in fp.requests] == ["A\n\nROLE\n\nZ", "A\n\nCALL\n\nZ"]


async def test_one_side_alone_and_update_live(seeded_kernel, fake_providers):
    k = seeded_kernel
    _ai(k, "wr_up", system_prompt="ROLE", post_system_prompt="Z")
    fp = fake_providers["wr_up"] = _Fake([DONE, DONE, DONE])
    await _send(k, "wr_up")
    assert _sys(fp.requests[0]) == "ROLE\n\nZ"
    out = await _op(
        k, {"type": "update_agent", "id": "wr_up", "pre_system_prompt": "A"}
    )
    assert not out.get("error"), out
    await _send(k, "wr_up")
    assert _sys(fp.requests[1]) == "A\n\nROLE\n\nZ"
    out = await _op(
        k,
        {
            "type": "update_agent",
            "id": "wr_up",
            "pre_system_prompt": None,
            "post_system_prompt": None,
        },
    )
    assert not out.get("error"), out
    await _send(k, "wr_up")
    assert _sys(fp.requests[2]) == "ROLE"


@pytest.mark.parametrize("name", ["pre_system_prompt", "post_system_prompt"])
@pytest.mark.parametrize("bad", ["", "   ", 42, ["x"]])
async def test_an_unusable_value_is_refused_at_the_write(seeded_kernel, name, bad):
    k = seeded_kernel
    _ai(k, "wr_ok", **{name: "FINE"})
    out = await _op(k, {"type": "update_agent", "id": "wr_ok", name: bad})
    assert out.get("error") == WRAP_PROMPT_REFUSAL.format(name=name), out
    assert k.get("wr_ok")[name] == "FINE"
