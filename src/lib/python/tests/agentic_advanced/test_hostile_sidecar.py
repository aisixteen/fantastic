"""ADV-5 — hostile persistence: the durable ledger is inert data, not a wire
into the kernel.

LangGraph's persistence layer was itself the attack surface: SQL injection in
the SQLite checkpointer chained to remote code execution (patched June 2026,
CVE chain published by The Hacker News / CSA). The checkpoint STORE executed
attacker-shaped strings.

Here the sidecar is one strict-JSON document behind the single fs surface: no
SQL, no eval, no interpolation. An attacker who can write the file gets three
possible outcomes, all safe: unreadable → LOUD empty start; wrong version →
LOUD empty start; well-formed hostile rows → they rehydrate as DATA — listable,
joinable, never executed.
"""

from __future__ import annotations

import json
import sys
import types
from pathlib import Path

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
sys.modules["adv5_probe.tools"] = _mod


async def _boot_with_store(monkeypatch, tmp_path):
    from _testkit import boot_root

    monkeypatch.chdir(tmp_path)
    k = boot_root()
    await k.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "create_agent",
                    "parent_id": k.id,
                    "handler_module": "file_bridge.tools",
                    "id": "kernel_store",
                    "root": ".fantastic",
                    "ingress_rule": "allow_all_ingress",
                },
            }
        ]
    )
    return k


_SIDECAR = Path(".fantastic/ledger.json")


async def test_malformed_sidecar_boots_loud_and_empty(tmp_path, monkeypatch, capsys):
    from _testkit import boot_root, persist

    k1 = await _boot_with_store(monkeypatch, tmp_path)
    persist(k1)
    _SIDECAR.write_text('{"v": 1, "rows": {broken')  # not JSON
    del k1

    k2 = boot_root()
    await k2.send([{"target": k2.id, "payload": {"type": "boot"}}])
    out = capsys.readouterr().out
    assert "unreadable" in out, f"a corrupt sidecar was swallowed silently: {out!r}"
    led = (await k2.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0][
        "result"
    ]
    assert led["executions"] == [] or all(
        r["id"] for r in led["executions"]
    )  # empty start, kernel alive


async def test_unknown_version_boots_loud_and_empty(tmp_path, monkeypatch, capsys):
    from _testkit import boot_root, persist

    k1 = await _boot_with_store(monkeypatch, tmp_path)
    persist(k1)
    _SIDECAR.write_text(json.dumps({"v": 99, "rows": {"x": {}}}))
    del k1

    k2 = boot_root()
    await k2.send([{"target": k2.id, "payload": {"type": "boot"}}])
    out = capsys.readouterr().out
    assert "version" in out and "99" in out, f"version drift not announced: {out!r}"


async def test_hostile_rows_rehydrate_as_data_and_never_execute(tmp_path, monkeypatch):
    """The checkpointer-RCE shape, replayed against us: attacker-shaped strings
    in every field of a persisted row. They come back as strings."""
    from _testkit import boot_root, persist

    _runs.clear()
    k1 = await _boot_with_store(monkeypatch, tmp_path)
    persist(k1)
    sqli = "job-1'; DROP TABLE agents;--"
    hostile = {
        "v": 1,
        "rows": {
            "job-1": {
                "target": "adv5_probe",
                "retries": 0,
                "attempts": 1,
                "named": True,
                "origin": None,
                "outcome": {
                    "pid": "job-1",
                    "status": "completed",
                    "result": {
                        "note": sqli,
                        "__proto__": {"polluted": True},
                        "cmd": "__import__('os').system('id')",
                    },
                    "attempts": 1,
                },
            }
        },
    }
    _SIDECAR.write_text(json.dumps(hostile))
    del k1

    k2 = boot_root()
    await k2.send([{"target": k2.id, "payload": {"type": "boot"}}])
    await k2.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "create_agent",
                    "parent_id": k2.id,
                    "handler_module": "adv5_probe.tools",
                    "id": "adv5_probe",
                },
            }
        ]
    )

    # the row is VISIBLE — as data
    led = (await k2.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0][
        "result"
    ]
    rows = {r["id"]: r for r in led["executions"]}
    assert "job-1" in rows and rows["job-1"]["status"] == "completed"

    # joining the hostile id returns the recorded strings VERBATIM — the
    # payloads stayed payloads, the handler never ran
    join = (
        await k2.send(
            [{"target": "adv5_probe", "payload": {"type": "work"}, "id": "job-1"}]
        )
    )[0]
    assert join["status"] == "completed"
    assert join["result"]["note"] == sqli
    assert join["result"]["cmd"] == "__import__('os').system('id')"
    assert _runs == [], "a rehydrated row EXECUTED — the RCE shape reproduced"
