"""The kernel's stable instance `guid`.

CONTRACT (identical across runtimes): the KERNEL owns a stable instance guid,
minted ONCE on boot into the ROOT agent record's meta and persisted to disk
(`.fantastic/agent.yaml`). It is a PERSISTENCE-scoped identity — sourced from the
persisted meta (unlike run-scoped runtime/env/version, read inline from the env) —
so it round-trips a reboot verbatim and is never regenerated. Root reflect always
carries a top-level `guid`; an explicit/existing guid always wins.

Mirrors `relay_connector`'s guid tests (auto-mint-once + persist-across-reboot),
using the kernel bootstrap helpers (`boot_root` / `persist`).
"""

from __future__ import annotations

import yaml
import json

from _testkit import boot_root, persist

_HEX = set("0123456789abcdef")


def _is_guid(v) -> bool:
    return isinstance(v, str) and len(v) == 32 and all(c in _HEX for c in v)
# twin: the_kernel_guid_is_a_uuid4_hex
# twin: root_reflect_carries_a_32hex_guid


async def test_guid_auto_mints_once_and_persists(tmp_path, monkeypatch):
    """A fresh kernel with no `guid` in the root record auto-mints a 32-hex-char
    lowercase guid on boot, stores it in the root meta, and surfaces it top-level
    in root reflect. `persist` materializes it to `.fantastic/agent.yaml`."""
    monkeypatch.chdir(tmp_path)
    k1 = boot_root()
    assert not k1.record.get("guid")  # nothing minted yet — no boot has run

    # Boot the root → the mint fires once.
    (await k1.send([{"target": "kernel_state", "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    minted = k1.record.get("guid")
    assert _is_guid(minted), minted

    # Root reflect carries the SAME guid top-level (persistence-scoped).
    r = (await k1.send([{"target": "kernel", "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
    assert r["guid"] == minted

    # A second boot is idempotent — the guid is NOT regenerated.
    (await k1.send([{"target": "kernel_state", "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    assert k1.record.get("guid") == minted

    # Materialize to disk and confirm the on-disk root record carries it.
    persist(k1)
    disk = yaml.safe_load(
        (tmp_path / ".fantastic" / "agent.yaml").read_text(encoding="utf-8")
    )
    assert disk["guid"] == minted
# twin: boot_mints_a_32hex_guid_and_persists_it_stably


async def test_guid_persists_across_reboot(tmp_path, monkeypatch):
    """After a reboot in the SAME dir the rehydrated root reflects the SAME guid —
    read back verbatim from disk, never re-minted."""
    monkeypatch.chdir(tmp_path)
    k1 = boot_root()
    (await k1.send([{"target": "kernel_state", "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    minted = k1.record.get("guid")
    assert _is_guid(minted)
    persist(k1)

    # Drop k1, re-bootstrap from the same `.fantastic/` dir.
    del k1
    k2 = boot_root()
    assert k2.record.get("guid") == minted  # hydrated from disk

    # Re-booting the rehydrated root does NOT mint a new one.
    (await k2.send([{"target": "kernel_state", "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    assert k2.record.get("guid") == minted
    r = (await k2.send([{"target": "kernel", "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
    assert r["guid"] == minted


async def test_explicit_guid_wins(tmp_path, monkeypatch):
    """An explicit guid already in the root record is never overwritten by the
    mint — auto-gen only FILLS an absent field."""
    monkeypatch.chdir(tmp_path)
    # Seed a root record carrying an explicit guid.
    fant = tmp_path / ".fantastic"
    fant.mkdir()
    (fant / "agent.yaml").write_text(
        json.dumps(
            {
                "id": "kernel_state",
                "handler_module": "kernel_state.tools",
                "guid": "deadbeef" * 4,
            }
        )
    )
    k = boot_root()
    (await k.send([{"target": "kernel_state", "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    assert k.record.get("guid") == "deadbeef" * 4
    r = (await k.send([{"target": "kernel", "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
    assert r["guid"] == "deadbeef" * 4


async def test_fresh_dir_mints_a_different_guid(tmp_path, monkeypatch):
    """Two independent kernels (different dirs) mint DISTINCT guids — the guid is a
    per-instance identity, not a constant."""
    a, b = tmp_path / "a", tmp_path / "b"
    a.mkdir()
    b.mkdir()

    monkeypatch.chdir(a)
    ka = boot_root()
    (await ka.send([{"target": "kernel_state", "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    guid_a = ka.record.get("guid")

    monkeypatch.chdir(b)
    kb = boot_root()
    (await kb.send([{"target": "kernel_state", "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    guid_b = kb.record.get("guid")

    assert _is_guid(guid_a) and _is_guid(guid_b)
    assert guid_a != guid_b


async def test_in_memory_boot_mints_an_ephemeral_guid(tmp_path, monkeypatch):
    """An in-memory boot with no store wired still mints a guid into the record
    (just not persisted), so root reflect ALWAYS carries one."""
    monkeypatch.chdir(tmp_path)
    k = boot_root()
    (await k.send([{"target": "kernel_state", "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    r = (await k.send([{"target": "kernel", "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
    assert _is_guid(r["guid"])
