"""kernel_state.persist_blob / load_blob — the loader as the ONE byte-store authority.

A dumb whole-file authority: any agent persists its sidecars THROUGH the loader's
discovered store instead of wiring its own file_bridge_id. NO data semantics, NO RAM
fallback, and a filename guard against clobbering kernel-managed files / escaping the
agent's own dir."""

from pathlib import Path

import pytest


async def _mk_target(kernel, mode="mem"):
    """A live child agent to own the sidecars (the scheduler is a real consumer)."""
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "mode": mode,
                    },
                }
            ]
        )
    )[0]["result"]
    assert "id" in rec, rec
    return rec["id"]


async def test_persist_then_load_round_trips(seeded_kernel, store_agent):
    aid = await _mk_target(seeded_kernel)
    w = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "persist_blob",
                        "agent_id": aid,
                        "name": "state.yaml",
                        "content": "user.name: Ada\n",
                    },
                }
            ]
        )
    )[0]["result"]
    assert w.get("ok") is True, w
    # Lands on disk store-relative, next to the agent's record dir.
    on_disk = Path(f".fantastic/agents/{aid}/state.yaml")
    assert on_disk.exists(), "blob must land under .fantastic/agents/<id>/"
    assert "Ada" in on_disk.read_text()
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "load_blob",
                        "agent_id": aid,
                        "name": "state.yaml",
                    },
                }
            ]
        )
    )[0]["result"]
    assert r.get("content") == "user.name: Ada\n"


async def test_load_absent_is_null_not_error(seeded_kernel, store_agent):
    aid = await _mk_target(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "load_blob",
                        "agent_id": aid,
                        "name": "missing.yaml",
                    },
                }
            ]
        )
    )[0]["result"]
    assert r.get("content") is None  # never-written sidecar reads empty
    assert "error" not in r


@pytest.mark.parametrize(
    "bad", ["agent.yaml", "readme.md", "../escape", "sub/dir.yaml", ".hidden"]
)
async def test_guard_rejects_dangerous_names(seeded_kernel, store_agent, bad):
    aid = await _mk_target(seeded_kernel)
    w = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "persist_blob",
                        "agent_id": aid,
                        "name": bad,
                        "content": "x",
                    },
                }
            ]
        )
    )[0]["result"]
    assert "error" in w, f"guard must reject {bad!r}: {w!r}"


async def test_no_store_failfasts_no_ram(seeded_kernel):
    # No file_bridge rooted at .fantastic ⇒ no store discovered ⇒ error (no RAM).
    aid = await _mk_target(seeded_kernel)
    w = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "persist_blob",
                        "agent_id": aid,
                        "name": "state.yaml",
                        "content": "x",
                    },
                }
            ]
        )
    )[0]["result"]
    assert "error" in w and "no store" in w["error"], w


async def test_sealed_store_surfaces_denied_write(seeded_kernel):
    # A store rooted at .fantastic but SEALED (no ingress_rule) is still discovered;
    # the denied write is SURFACED, not silently dropped (no fallback).
    sealed = (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "root": ".fantastic",
                    },
                }
            ]
        )
    )[0]["result"]
    assert "id" in sealed, sealed
    aid = await _mk_target(seeded_kernel)
    w = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "persist_blob",
                        "agent_id": aid,
                        "name": "state.yaml",
                        "content": "x",
                    },
                }
            ]
        )
    )[0]["result"]
    assert "error" in w, w
    assert w.get("reason") == "unauthorized", w


# ─── the loader's own guards (A-5) ──────────────────────────────


async def test_a_non_utf8_sidecar_is_refused_not_mangled(seeded_kernel, store_agent):
    """`errors="replace"` turned a binary sidecar into U+FFFD mojibake and
    returned it as `content`, so a caller could not tell a corrupt read from a
    real one — and writing that back would have destroyed the file."""
    from kernel import operator_context

    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "id": "blob_bin_owner",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    assert rec.get("id") == "blob_bin_owner", rec
    with operator_context():
        w = (
            await seeded_kernel.send(
                [
                    {
                        "target": store_agent,
                        "payload": {
                            "type": "write_stream",
                            "path": "agents/blob_bin_owner/binary.bin",
                            "bytes": b"\xff\xfe\x00\x01",
                            "truncate": True,
                        },
                    }
                ]
            )
        )[0]["result"]
    assert w.get("written") == 4, w
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {
                        "type": "load_blob",
                        "agent_id": "blob_bin_owner",
                        "name": "binary.bin",
                    },
                }
            ]
        )
    )[0]["result"]
    assert r.get("reason") == "bad_encoding", r
    assert r.get("content") is None, r
    assert "�" not in str(r.get("error", "")), "the refusal must not carry mojibake"


async def test_persist_record_refuses_a_siblings_namespace(seeded_kernel):
    """The ROOT loader's tree is hydrated at boot, so no agent writes it — not a
    sibling's record and not its OWN (audit 2026-09-28, a15: a record an agent
    persisted there came back at the next boot with whatever grants it named).
    An agent's record changes through `update_agent`, where conferral runs."""
    from kernel import sender_context

    async def _mk(agent_id):
        rec = (
            await seeded_kernel.send(
                [
                    {
                        "target": seeded_kernel.id,
                        "payload": {
                            "type": "create_agent",
                            "parent_id": seeded_kernel.id,
                            "id": agent_id,
                            "handler_module": "file_bridge.tools",
                            "allow": ["."],
                            # `persist_record` is privileged: the baseline never
                            # reaches it, so the grant is named — and the LOADER's
                            # owner guard is what this test is about.
                            "can_call": {seeded_kernel.id: ["persist_record"]},
                        },
                    }
                ]
            )
        )[0]["result"]
        assert rec.get("id") == agent_id, rec
        return agent_id

    attacker = await _mk("ro_attacker")
    victim = await _mk("ro_victim")
    with sender_context(attacker):
        r = (
            await seeded_kernel.send(
                [
                    {
                        "target": seeded_kernel.id,
                        "payload": {
                            "type": "persist_record",
                            "record": {"id": victim, "parent_id": seeded_kernel.id},
                        },
                    }
                ]
            )
        )[0]["result"]
        assert r.get("reason") == "unauthorized", r
        # …and not ITSELF either.
        own = (
            await seeded_kernel.send(
                [
                    {
                        "target": seeded_kernel.id,
                        "payload": {
                            "type": "persist_record",
                            "record": {"id": attacker, "parent_id": seeded_kernel.id},
                        },
                    }
                ]
            )
        )[0]["result"]
    assert own.get("reason") == "unauthorized", own
    assert "only by the operator" in own["error"], own


async def test_forget_record_refuses_a_siblings_namespace(seeded_kernel):
    """`forget_record` ends in an rmtree, so the same guard has to hold there."""
    from kernel import sender_context

    for aid in ("ro_f_attacker", "ro_f_victim"):
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "id": aid,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        # named, so the LOADER's owner guard answers, not the gate
                        "can_call": {seeded_kernel.id: ["forget_record"]},
                    },
                }
            ]
        )
    with sender_context("ro_f_attacker"):
        r = (
            await seeded_kernel.send(
                [
                    {
                        "target": seeded_kernel.id,
                        "payload": {"type": "forget_record", "id": "ro_f_victim"},
                    }
                ]
            )
        )[0]["result"]
    assert r.get("reason") == "unauthorized", r
