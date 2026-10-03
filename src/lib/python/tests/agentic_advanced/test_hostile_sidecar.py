"""ADV-5 — hostile persistence: what the memory store hands back is inert data,
not a wire into the kernel.

LangGraph's persistence layer was itself the attack surface: SQL injection in
the SQLite checkpointer chained to remote code execution (patched June 2026,
CVE chain published by The Hacker News / CSA). The checkpoint STORE executed
attacker-shaped strings.

Here the kernel's execution rows live in its MEMORY STORE, reached only through
the store's own verbs: no SQL, no eval, no interpolation. An attacker who can
hand the store tables (`inject`) gets two possible outcomes, both safe: tables
that break a key or a foreign key → a LOUD refusal, nothing changed; well-formed
hostile rows → they come back as DATA — listable, joinable, never executed.
"""

from __future__ import annotations

import types
from kernel import register_unlisted

_runs: list[str] = []

_mod = types.ModuleType("adv5_probe.tools")


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "adv5 probe", "verbs": {}}
    if verb == "work":
        _runs.append(id)
        return {"run": len(_runs)}
    return {"error": f"unknown verb {verb!r}"}


_mod.handler = _handler
_mod.VERBS = {}
register_unlisted("adv5_probe.tools", _mod)


async def _root_with_store(monkeypatch, tmp_path):
    from _testkit import boot_root

    monkeypatch.chdir(tmp_path)
    # A PREVIOUS RUN left its rows below the bound this run resumes from — the root
    # record's `next_id`, which is what keeps a new run's ids from repeating them.
    (tmp_path / ".fantastic").mkdir()
    (tmp_path / ".fantastic" / "agent.yaml").write_text(
        "id: kernel_state\nhandler_module: kernel_state.tools\nnext_id: 5000\n"
    )
    k = boot_root()
    assert k.ctx.next_id == 5000
    await k.send([{"target": k.id, "payload": {"type": "boot"}}])  # mints the guid
    for payload in (
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": "ram_store.tools",
            "id": "memory",
            "operator": True,
            "admins": ["kernel"],
        },
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": "adv5_probe.tools",
            "id": "adv5_probe",
        },
    ):
        await k.send([{"target": "kernel", "payload": payload}])
    k.ctx.update(k.id, memory_store_id="memory")
    return k


async def _inject(k, rows: list[dict], **extra) -> dict:
    owner = f"{k.record['guid']}|kernel"
    tables = {"rows": [{"owner": owner, "row": r} for r in rows], **extra}
    return (
        await k.send(
            [{"target": "memory", "payload": {"type": "inject", "tables": tables}}]
        )
    )[0]["result"]


async def test_broken_tables_are_refused_loud_and_change_nothing(tmp_path, monkeypatch):
    """A duplicate key or a grant anchored on nothing is refused whole — the store
    never half-loads, never repairs."""
    k = await _root_with_store(monkeypatch, tmp_path)
    ok = await _inject(k, [{"id": 1, "kind": "call"}])
    assert ok == {"rows": 1, "grants": 0}
    dup = await _inject(k, [{"id": 5, "kind": "call"}, {"id": 5, "kind": "settle"}])
    assert dup["reason"] == "exists"
    dangling = await _inject(
        k,
        [{"id": 7, "kind": "call"}],
        grants=[
            {
                "grant": 1,
                "owner": "x|y",
                "reader": "x|z",
                "scope": {"row": 99},
                "mode": "frozen",
                "anchor": 99,
            }
        ],
    )
    assert dangling["reason"] == "bad_arg"
    dump = (await k.send([{"target": "memory", "payload": {"type": "dump"}}]))[0][
        "result"
    ]
    assert [r["row"]["id"] for r in dump["tables"]["rows"]] == [1], "nothing changed"


async def test_hostile_rows_come_back_as_data_and_never_execute(tmp_path, monkeypatch):
    """The checkpointer-RCE shape, replayed against us: attacker-shaped strings in
    every field of a stored call and its settle. They come back as strings."""
    _runs.clear()
    k = await _root_with_store(monkeypatch, tmp_path)
    sqli = "job-1'; DROP TABLE agents;--"
    out = await _inject(
        k,
        [
            {
                "id": 1,
                "kind": "call",
                "agent": "adv5_probe",
                "verb": "work",
                "name": "job-1",
                "named": True,
                "retries": 0,
                "attempts": 1,
                "payload": {"type": "work", "note": sqli},
            },
            {
                "id": 2,
                "kind": "settle",
                "agent": "kernel_state",
                "parent": 1,
                "settled": "adv5_probe",
                "attempts": 1,
                "outcome": {
                    "status": "completed",
                    "result": {
                        "note": sqli,
                        "__proto__": {"polluted": True},
                        "cmd": "__import__('os').system('id')",
                    },
                },
            },
        ],
    )
    assert out == {"rows": 2, "grants": 0}, out
    started = await k.ctx.start(boot_all=False)
    assert started.get("started"), started

    # the row is VISIBLE — as data
    led = (await k.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0][
        "result"
    ]
    rows = {r.get("name"): r for r in led["executions"] if r.get("name")}
    assert "job-1" in rows and rows["job-1"]["status"] == "completed"

    # joining the hostile id returns the recorded strings VERBATIM — the payloads
    # stayed payloads, the handler never ran
    join = (
        await k.send(
            [{"target": "adv5_probe", "payload": {"type": "work"}, "id": "job-1"}]
        )
    )[0]
    assert join["status"] == "completed"
    assert join["result"]["note"] == sqli
    assert join["result"]["cmd"] == "__import__('os').system('id')"
    assert _runs == [], "a stored row EXECUTED — the RCE shape reproduced"
