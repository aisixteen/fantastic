"""The system block's placeholders and its FILE.

Placeholders `{tree}` `{self}` `{store}` `{howto}` are filled per agent and per turn
in whichever prompt is in force; `{tree}\\n\\n{self}\\n\\n{howto}` IS the default block,
byte for byte. `file_bridge_id` + `system_prompt_file` (a path on it) make a file —
read THROUGH that file_bridge, as the agent — the block, re-read on the next turn
after the bridge reports it changed (`watch_path`). Not set: nothing is looked up,
the default block. Set and unusable: the turn FAILS, loud — never another block.
Precedence: per-call > record `system_prompt` > the file > the default; nothing is
ever appended. Real turn path, provider faked."""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest
from _testkit import memory_meta

from ai_core import prompt_file

DONE = '<call name="done"></call>'


@pytest.fixture(autouse=True)
def _forget_watches():
    """The bridge's watch table and the agents' file states are process-wide (one
    kernel per process): drop what a test left, so the next starts clean."""
    yield
    from file_bridge.tools import _WATCH

    for st in _WATCH.values():
        if st["task"] is not None:
            st["task"].cancel()
    _WATCH.clear()
    prompt_file._FILES.clear()


BRIDGE_VERBS = ["read_stream", "watch_path", "unwatch_path", "watch"]


class _Fake:
    def __init__(self):
        self.requests: list[list[dict]] = []

    async def chat(self, messages, max_tokens=None):
        self.requests.append(list(messages))
        yield DONE


def _sys(req: list[dict]) -> str:
    assert req[0]["role"] == "system"
    return req[0]["content"]


async def _op(k, payload) -> dict:
    return (await k.send([{"target": "kernel", "payload": payload}]))[0]["result"]


async def _turn(k, aid: str, fp: _Fake, **extra) -> str:
    n = len(fp.requests)
    out = (
        await k.send(
            [{"target": aid, "payload": {"type": "send", "text": "go", **extra}}]
        )
    )[0]
    assert out["status"] == "completed", out
    return _sys(fp.requests[n])


async def _failed_turn(k, aid: str, fp: _Fake) -> dict:
    """A turn that must fail loud: no request reaches the model."""
    n = len(fp.requests)
    out = (await k.send([{"target": aid, "payload": {"type": "send", "text": "go"}}]))[
        0
    ]
    assert len(fp.requests) == n, "the model was called on another block"
    res = out["result"] or {}
    assert res.get("reason") == "system_prompt_file", out
    return res


def _bridge(k, readers: list[str]) -> str:
    rec = k.create(
        "file_bridge.tools",
        id="prompts",
        allow=["."],
        ingress_rule="allow_all_ingress",
        callable_by={r: BRIDGE_VERBS for r in readers},
    )
    assert rec.get("id") == "prompts", rec
    return "prompts"


def _ai(k, aid: str, fake_providers, *, file: bool = False, **meta) -> _Fake:
    can = {"cli": "*"}
    if file:
        can["prompts"] = BRIDGE_VERBS
        meta["file_bridge_id"] = "prompts"
        meta["system_prompt_file"] = "prompt.md"
    rec = k.create(
        "ollama_backend.tools",
        id=aid,
        endpoint="http://127.0.0.1:9/stub",
        model="stub",
        context_window=32768,
        **meta,
        **memory_meta(k, aid, can),
    )
    assert rec.get("id") == aid, rec
    fp = fake_providers[aid] = _Fake()
    return fp


async def _until_stale(aid: str, timeout: float = 5.0) -> None:
    end = asyncio.get_event_loop().time() + timeout
    while not prompt_file._FILES[aid]["stale"]:
        assert asyncio.get_event_loop().time() < end, "no `changed` reached the agent"
        await asyncio.sleep(0.05)


# ─── placeholders ────────────────────────────────────────────────────


async def test_the_default_layout_in_placeholders_is_the_default_block(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    fp = _ai(k, "ph_ai", fake_providers)
    default = await _turn(k, "ph_ai", fp)
    up = await _op(
        k,
        {
            "type": "update_agent",
            "id": "ph_ai",
            "system_prompt": "{tree}\n\n{self}\n\n{howto}",
        },
    )
    assert up.get("updated") is True, up
    assert await _turn(k, "ph_ai", fp) == default


async def test_placeholders_fill_and_unknown_ones_stay(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    fp = _ai(k, "ph2", fake_providers, system_prompt="store={store} {nope} {tree}")
    # `{tree}` is the kernel's reflect, read as the agent: grant it both ends
    cb = dict(k.get(k.id).get("callable_by") or {})
    k.ctx.update(k.id, callable_by={**cb, "ph2": ["reflect"]})
    can = dict(k.get("ph2").get("can_call") or {})
    k.ctx.update("ph2", can_call={**can, k.id: ["reflect"]})
    first = await _turn(k, "ph2", fp)
    assert first.startswith("store=memory {nope} "), first[:80]
    assert "fresh_one" not in first
    k.create(None, id="fresh_one")
    assert "fresh_one" in await _turn(k, "ph2", fp), "{tree} is the live tree"
    # a per-call prompt is filled too
    per_call = await _turn(k, "ph2", fp, system_prompt="me={self}")
    assert per_call.startswith("me=You are `ph2`."), per_call[:80]


# ─── the file ────────────────────────────────────────────────────────


async def test_the_file_is_the_block_and_an_edit_is_live_next_turn(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    _bridge(k, ["pf_ai"])
    Path("prompt.md").write_bytes(b"FILE-ROLE for {store}\n")
    fp = _ai(k, "pf_ai", fake_providers, file=True)
    assert await _turn(k, "pf_ai", fp) == "FILE-ROLE for memory\n"
    # held under the bridge's watcher, as the agent
    from file_bridge.tools import _WATCH

    assert _WATCH["prompts"]["paths"] == {"prompt.md": {"pf_ai"}}
    await asyncio.sleep(0.3)
    Path("prompt.md").write_bytes(b"EDITED {self}\n")  # an editor's save
    await _until_stale("pf_ai")
    assert (await _turn(k, "pf_ai", fp)).startswith("EDITED You are `pf_ai`.")
    # no change: no read (the bridge saw one read per change)
    reads = [r for r in fp.requests]
    assert len(reads) == 2


async def test_a_record_prompt_beats_the_file_and_clearing_the_field_drops_it(
    seeded_kernel, fake_providers
):
    from file_bridge.tools import _WATCH

    k = seeded_kernel
    _bridge(k, ["pf2"])
    Path("prompt.md").write_bytes(b"FILE\n")
    fp = _ai(k, "pf2", fake_providers, file=True, system_prompt="RECORD")
    assert await _turn(k, "pf2", fp) == "RECORD"
    await _op(k, {"type": "update_agent", "id": "pf2", "system_prompt": None})
    assert await _turn(k, "pf2", fp) == "FILE\n"
    await _op(k, {"type": "update_agent", "id": "pf2", "system_prompt_file": None})
    default = await _turn(k, "pf2", fp)
    assert "You are `pf2`." in default, "not set: nothing looked up, the default"
    assert "pf2" not in prompt_file._FILES
    assert _WATCH["prompts"]["paths"] == {}, "the agent's hold is released"


async def test_a_path_without_file_bridge_id_fails_and_a_bridge_alone_is_the_default(seeded_kernel, fake_providers):
    k = seeded_kernel
    fp = _ai(k, "pf6", fake_providers, system_prompt_file="prompt.md")
    res = await _failed_turn(k, "pf6", fp)
    assert "no file_bridge_id" in res["error"], res
    # file_bridge_id alone names nothing to read: the default block, silently
    await _op(k, {"type": "update_agent", "id": "pf6", "system_prompt_file": None})
    await _op(k, {"type": "update_agent", "id": "pf6", "file_bridge_id": "prompts"})
    assert "You are `pf6`." in await _turn(k, "pf6", fp)


async def test_a_missing_file_fails_the_turn_until_it_appears(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    _bridge(k, ["pf3"])
    fp = _ai(k, "pf3", fake_providers, file=True)
    res = await _failed_turn(k, "pf3", fp)
    assert "prompt.md" in res["error"], res
    rf = (await k.send([{"target": "pf3", "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
    assert "prompt.md" in rf.get("system_prompt_file_error", ""), rf
    # the file appears: the watcher reports it, the next turn uses it
    await asyncio.sleep(0.3)
    Path("prompt.md").write_bytes(b"NOW\n")
    await _until_stale("pf3")
    assert await _turn(k, "pf3", fp) == "NOW\n"
    rf = (await k.send([{"target": "pf3", "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
    assert "system_prompt_file_error" not in rf, rf


async def test_without_the_grants_the_turn_fails_and_nothing_is_read(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    _bridge(k, [])  # the bridge does not accept the agent
    Path("prompt.md").write_bytes(b"SECRET\n")
    fp = _ai(k, "pf4", fake_providers, file=True)
    res = await _failed_turn(k, "pf4", fp)
    assert "watch_path" in res["error"] and "SECRET" not in res["error"], res


async def test_an_empty_file_fails_the_turn(seeded_kernel, fake_providers):
    k = seeded_kernel
    _bridge(k, ["pf7"])
    Path("prompt.md").write_bytes(b" \n")
    fp = _ai(k, "pf7", fake_providers, file=True)
    res = await _failed_turn(k, "pf7", fp)
    assert "empty" in res["error"], res


async def test_deleting_the_agent_releases_the_file(seeded_kernel, fake_providers):
    from file_bridge.tools import _WATCH

    k = seeded_kernel
    _bridge(k, ["pf5"])
    Path("prompt.md").write_bytes(b"X\n")
    fp = _ai(k, "pf5", fake_providers, file=True)
    await _turn(k, "pf5", fp)
    await _op(k, {"type": "delete_agent", "id": "pf5"})
    assert "pf5" not in prompt_file._FILES
    assert _WATCH["prompts"]["paths"] == {}


@pytest.mark.parametrize(
    "field,bad,refusal",
    [
        ("system_prompt_file", "", prompt_file.SYSTEM_PROMPT_FILE_REFUSAL),
        ("system_prompt_file", "  ", prompt_file.SYSTEM_PROMPT_FILE_REFUSAL),
        ("system_prompt_file", 5, prompt_file.SYSTEM_PROMPT_FILE_REFUSAL),
        (
            "system_prompt_file",
            {"bridge": "prompts", "path": "p.md"},
            prompt_file.SYSTEM_PROMPT_FILE_REFUSAL,
        ),
        ("file_bridge_id", "", prompt_file.FILE_BRIDGE_ID_REFUSAL),
        ("file_bridge_id", 5, prompt_file.FILE_BRIDGE_ID_REFUSAL),
        ("file_bridge_id", ["prompts"], prompt_file.FILE_BRIDGE_ID_REFUSAL),
    ],
)
async def test_a_bad_value_is_refused_at_the_write(seeded_kernel, field, bad, refusal):
    k = seeded_kernel
    meta = memory_meta(k, "pf_bad", {"cli": "*"})
    out = await _op(
        k,
        {
            "type": "create_agent",
            "parent_id": "kernel_state",
            "handler_module": "ollama_backend.tools",
            "id": "pf_bad",
            "endpoint": "http://127.0.0.1:9/stub",
            "model": "stub",
            field: bad,
            **meta,
        },
    )
    assert out.get("error") == refusal, out


async def test_the_schema_publishes_the_field(seeded_kernel):
    cat = (
        await _op(seeded_kernel, {"type": "reflect", "tree": "none", "bundles": "all"})
    )["bundles"]
    row = next(b for b in cat if b["name"] == "vllm_backend")
    names = [f["name"] for f in row["schema"]["fields"]]
    assert names[-5:] == [
        "system_prompt",
        "file_bridge_id",
        "system_prompt_file",
        "pre_system_prompt",
        "post_system_prompt",
    ]
