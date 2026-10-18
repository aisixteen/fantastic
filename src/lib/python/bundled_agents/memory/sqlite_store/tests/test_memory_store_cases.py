"""The shared memory-store cases (`contract/memory_store/cases.json`), replayed
against BOTH python stores. Each step must answer as the case says, and the two
stores' whole transcripts must be identical — a caller cannot tell them apart.
The rust stores replay the same file (`fantastic-sqlite-store/tests/contract.rs`)."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from _testkit import as_agent

CASES = json.loads(
    (
        Path(__file__).resolve().parents[7] / "contract" / "memory_store" / "cases.json"
    ).read_text(encoding="utf-8")
)
SCENARIOS = {s["name"]: s for s in CASES["scenarios"]}
STORES = {
    "ram_store.tools": {},
    "sqlite_store.tools": {"path": "stores/mem.db"},
}


async def _replay(kernel, handler: str, scenario: dict) -> list:
    """A fresh store under `handler`, the scenario's steps sent in order; returns
    every reply."""
    kernel.ctx.update(kernel.id, guid="g1")
    for aid in ("alice", "bob"):
        if aid not in kernel.ctx.agents:
            kernel.create(None, id=aid)
        kernel.ctx.update(aid, can_call={"mem": "*"})  # a delete drops the edge
    rec = kernel.create(
        handler, id="mem", callable_by={"alice": "*", "bob": "*"}, **STORES[handler]
    )
    assert rec.get("id") == "mem", rec
    replies = []
    for n, step in enumerate(scenario["steps"]):
        batch = [{"target": "mem", "payload": step["send"]}]
        if step["as"] is None:
            out = (await kernel.send(batch))[0]
        else:
            with as_agent(kernel, step["as"]):
                out = (await kernel.send(batch))[0]
        reply = out["result"]
        for key, want in step["expect"].items():
            assert key in reply and reply[key] == want, (
                f"{handler} {scenario['name']} step {n} {step['send']}: "
                f"{key} = {reply.get(key)!r}, want {want!r} — {reply}"
            )
        replies.append(reply)
    await kernel.ctx.agents[kernel.id].delete("mem")
    return replies


@pytest.mark.parametrize("name", list(SCENARIOS))
async def test_every_store_answers_the_case(kernel, name):
    transcripts = {h: await _replay(kernel, h, SCENARIOS[name]) for h in STORES}
    ram, sql = transcripts.values()
    for n, (a, b) in enumerate(zip(ram, sql)):
        a = {
            k: v for k, v in a.items() if k != "error" or a.get("reason") != "unhandled"
        }
        b = {
            k: v for k, v in b.items() if k != "error" or b.get("reason") != "unhandled"
        }
        if "sentence" in a:  # reflect: each says what it is; the counts agree
            a, b = ({k: x.get(k) for k in ("owners", "rows")} for x in (a, b))
        assert a == b, f"step {n}: ram_store {a} != sqlite_store {b}"
