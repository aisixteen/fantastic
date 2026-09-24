"""ADV-5 — hostile persistence: the durable tape is inert data, not a wire
into the kernel.

LangGraph's persistence layer was itself the attack surface: SQL injection in
the SQLite checkpointer chained to remote code execution (patched June 2026,
CVE chain published by The Hacker News / CSA). The checkpoint STORE executed
attacker-shaped strings.

Here the tape is ONE XML document (`messages.xml`) behind the single fs surface:
no SQL, no eval, no interpolation. An attacker who can write the file gets three
possible outcomes, all safe: unreadable → a LOUD parse error, nothing repaired;
wrong version → a LOUD refusal to guess; well-formed hostile rows → they
rehydrate as DATA — listable, joinable, never executed.
"""

from __future__ import annotations

import sys
import types
import xml.etree.ElementTree as ET
from pathlib import Path

import pytest

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


_TAPE = Path(".fantastic/messages.xml")


def _tape_row(**kw) -> str:
    from kernel_state import messages_xml

    return messages_xml.row_to_xml(kw)


async def test_a_cut_tape_boots_loud_and_repairs_nothing(tmp_path, monkeypatch):
    """A row that opened and never closed is a crash mid-append. Loud, unrepaired
    — the operator truncates; the loader never guesses what the rest said."""
    from _testkit import boot_root

    from kernel_state import messages_xml

    await _boot_with_store(monkeypatch, tmp_path)
    _TAPE.write_text(messages_xml.HEADER + '<row id="1" kind="call" verb="wo')

    with pytest.raises(ET.ParseError) as e:
        boot_root()
    assert "cut" in str(e.value)


async def test_an_unknown_tape_version_boots_loud(tmp_path, monkeypatch):
    from _testkit import boot_root

    await _boot_with_store(monkeypatch, tmp_path)
    _TAPE.write_text('<messages v="99">\n<row id="1" kind="call" />\n')

    with pytest.raises(RuntimeError) as e:
        boot_root()
    assert "unknown log version" in str(e.value) and "99" in str(e.value)


async def test_hostile_rows_rehydrate_as_data_and_never_execute(tmp_path, monkeypatch):
    """The checkpointer-RCE shape, replayed against us: attacker-shaped strings
    in every field of a persisted call and its settle. They come back as strings."""
    from _testkit import boot_root

    from kernel_state import messages_xml

    _runs.clear()
    await _boot_with_store(monkeypatch, tmp_path)
    sqli = "job-1'; DROP TABLE agents;--"
    _TAPE.write_text(
        messages_xml.HEADER
        + _tape_row(
            id=1,
            kind="call",
            agent="adv5_probe",
            verb="work",
            name="job-1",
            named=True,
            retries=0,
            attempts=1,
            payload={"type": "work", "note": sqli},
        )
        + _tape_row(
            id=2,
            kind="settle",
            agent="kernel_state",
            parent=1,
            settled="adv5_probe",
            attempts=1,
            outcome={
                "status": "completed",
                "result": {
                    "note": sqli,
                    "__proto__": {"polluted": True},
                    "cmd": "__import__('os').system('id')",
                },
            },
        )
    )

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
    rows = {r.get("name"): r for r in led["executions"] if r.get("name")}
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
