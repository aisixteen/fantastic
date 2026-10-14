"""The provider is built per turn from the LIVE record, and a deleted id leaves
nothing behind.

A cached provider captures endpoint, model and sampling when it is built: after
`update_agent` moves an agent to another endpoint, every later turn would still post
the conversation (and the key, when its ref still bound the old host) to the old
one, and a deleted id re-created later would inherit it.
These drive the REAL turn path (`send` → `_run` → `_build_provider` →
`make_provider`), with the provider CLASS recorded instead of dialled."""

from __future__ import annotations

from _testkit import memory_meta

from ai_core import core

DONE = '<call name="done"></call>'


class _Recorded:
    """Stands in for `OllamaProvider`: records what each build was given; every
    turn ends at once."""

    built: list[tuple[str, str]] = []

    def __init__(self, *, endpoint, model, **_):
        self._model = model
        _Recorded.built.append((endpoint, model))

    async def chat(self, messages, max_tokens=None):
        yield DONE

    @property
    def model(self):
        return self._model


def _ai(k, aid: str, endpoint: str, model: str):
    rec = k.create(
        "ollama_backend.tools",
        id=aid,
        endpoint=endpoint,
        model=model,
        context_window=32768,
        **memory_meta(k, aid, {"cli": "*"}),
    )
    assert rec.get("id") == aid, rec


async def _turn(k, aid: str):
    out = (await k.send([{"target": aid, "payload": {"type": "send", "text": "hi"}}]))[
        0
    ]
    assert out["status"] == "completed", out


async def _op(k, payload):
    return (await k.send([{"target": "kernel", "payload": payload}]))[0]["result"]


async def test_each_turn_builds_from_the_live_record(seeded_kernel, monkeypatch):
    import ollama_backend.provider as op

    monkeypatch.setattr(op, "OllamaProvider", _Recorded)
    _Recorded.built = []
    k = seeded_kernel
    _ai(k, "pt_ai", "http://first.invalid:1", "model-1")
    await _turn(k, "pt_ai")
    up = await _op(
        k,
        {
            "type": "update_agent",
            "id": "pt_ai",
            "endpoint": "http://second.invalid:2",
            "model": "model-2",
        },
    )
    assert up.get("updated") is True, up
    await _turn(k, "pt_ai")
    assert _Recorded.built[0] == ("http://first.invalid:1", "model-1")
    assert _Recorded.built[-1] == ("http://second.invalid:2", "model-2"), (
        "the turn after update_agent posts where the record now says"
    )
    # a deleted id re-created later builds from ITS record, never the old one's
    assert (await _op(k, {"type": "delete_agent", "id": "pt_ai"})).get("deleted")
    _ai(k, "pt_ai", "http://third.invalid:3", "model-3")
    await _turn(k, "pt_ai")
    assert _Recorded.built[-1] == ("http://third.invalid:3", "model-3")


async def test_delete_drops_every_row_but_the_incarnation(
    seeded_kernel, fake_providers
):
    """`on_delete` (re-exported by every backend) drops each per-id table, so an
    id re-created later starts clean; `_incarnation` stays monotonic, so a reply to
    a call the deleted agent made never becomes a live turn of the new one."""
    from ai_core.strategies.strip import _stripped

    k = seeded_kernel
    fake_providers["pt_del"] = _Recorded(endpoint="e", model="m")
    _ai(k, "pt_del", "http://e.invalid:1", "m")
    await _turn(k, "pt_del")
    core._calib["pt_del"] = 2.0
    core._reported["pt_del"] = 10
    core._projection["pt_del"] = {"fired": False}
    core._compaction_mark["pt_del"] = {}
    core._view_source["pt_del"] = "array"
    core._folds_since["pt_del"] = 1
    core._recall_due.add("pt_del")
    core._inflight["pt_del"] = 1
    core._silent[("pt_del", "c")] = 2
    core._queue["pt_del"] = []
    _stripped["pt_del"] = {"x"}
    mem = core._mem(k)
    for table in mem.values():
        table["pt_del"] = {}
    incarnation = core._incarnation.get("pt_del")
    assert "pt_del" in core._locks
    assert (await _op(k, {"type": "delete_agent", "id": "pt_del"})).get("deleted")
    for table in (
        core._locks,
        core._queue,
        core._current,
        core._projection,
        core._compaction_mark,
        core._view_source,
        core._folds_since,
        core._calib,
        core._reported,
        core._inflight,
        _stripped,
        *mem.values(),
    ):
        assert "pt_del" not in table
    assert "pt_del" not in core._recall_due
    assert not [c for c in core._silent if c[0] == "pt_del"]
    assert core._incarnation.get("pt_del") == incarnation


async def test_vllm_delete_drops_its_last_budget(seeded_kernel):
    from vllm_backend.provider import last_budgets

    k = seeded_kernel
    rec = k.create(
        "vllm_backend.tools",
        id="pt_v",
        endpoint="http://v.invalid:1/v1",
        model="m",
        max_tokens=8000,
        kv_slots=1,
        kv_overflow="evict",
        **memory_meta(k, "pt_v"),
    )
    assert rec.get("id") == "pt_v", rec
    last_budgets["pt_v"] = 4000
    assert (await _op(k, {"type": "delete_agent", "id": "pt_v"})).get("deleted")
    assert "pt_v" not in last_budgets
