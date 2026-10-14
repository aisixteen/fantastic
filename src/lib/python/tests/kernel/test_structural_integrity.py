"""Doc claims pinned against the code: three structural defects and the security
claim the doc rests on.
"""

from __future__ import annotations

import pytest

from cli import Cli
from kernel import Kernel

from _testkit import as_agent


# ─── a1 — save() emitted a document its own load() refused ──────


async def test_save_drops_the_ephemeral_SUBTREE_not_just_the_ephemeral_agent(kernel):
    """`save()` filtered `type(a).ephemeral` per agent, so a CHILD of an
    ephemeral survived carrying `parent_id: cli` — an id no longer in the
    document. `Kernel.load` then refused its own output:
    `SnapshotError: agent 'under_cli' parent 'cli' not in snapshot`.

    A checkpoint that cannot be restored, produced with no error at write time.
    """
    Cli(kernel.ctx, parent=kernel)
    (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "cli",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": "kid",
                    },
                }
            ]
        )
    )[0]["result"]
    # To `kernel`, not to `"kid"`. Kernel verbs are root-only, so addressing the
    # child was refused `not_here` and grandkid was never created — leaving the
    # transitive claim below asserting the absence of something that never existed.
    gk = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kid",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": "grandkid",
                    },
                }
            ]
        )
    )[0]["result"]
    assert gk.get("id") == "grandkid", f"the fixture must actually build it: {gk!r}"
    assert kernel.get("grandkid")["parent_id"] == "kid", "two levels under an ephemeral"

    snap = kernel.ctx.save()
    ids = {r["id"] for r in snap["records"]}
    assert "cli" not in ids
    assert "kid" not in ids, "a child of an ephemeral is not persistable either"
    assert "grandkid" not in ids, "the drop is transitive, not one level"

    Kernel().load(snap)  # the real assertion: its own output must load


async def test_a_saved_document_never_names_a_parent_it_does_not_contain(kernel):
    """The general form. Any dangling `parent_id` is a document that will be
    refused on the way back in."""
    Cli(kernel.ctx, parent=kernel)
    (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "cli",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": "kid",
                    },
                }
            ]
        )
    )[0]["result"]
    records = kernel.ctx.save()["records"]
    ids = {r["id"] for r in records}
    for r in records:
        pid = r.get("parent_id")
        assert pid is None or pid in ids, f"{r['id']} names absent parent {pid!r}"


# ─── a2 — a record could lie about its own parent ───────────────


async def test_a_record_cannot_claim_a_parent_it_does_not_have(kernel):
    """If `Agent.record` built `parent_id` from the tree and THEN merged meta, a
    `parent_id` that reached meta would overwrite the truth: after
    `update_agent id=b parent_id=a`, the record would say `'a'` while the tree says
    `'kernel_state'`.

    Not cosmetic. `$parent` resolves against the TREE on one half of the gate
    and readers take the RECORD on the other (`web/host/src/web/tools.py` reads
    `record.get("parent_id")`), so the two halves disagree about who your parent
    is — and a reboot silently corrects it from disk position, erasing the
    evidence.
    """
    for aid in ("a", "b"):
        (
            await kernel.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": kernel.id,
                            "handler_module": "file_bridge.tools",
                            "allow": ["."],
                            "id": aid,
                        },
                    }
                ]
            )
        )[0]["result"]
    (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {"type": "update_agent", "id": "b", "parent_id": "a"},
                }
            ]
        )
    )[0]["result"]

    rec, node = kernel.get("b"), kernel.ctx.agents["b"]
    assert rec["parent_id"] == node.parent.id == kernel.id


async def test_parent_id_places_the_agent_and_is_required(kernel):
    """`parent_id` NAMES THE OWNER — it is the only way to place an agent.

    The ADDRESSED agent never becomes the parent, and a `parent_id` in the payload
    is never stripped or silently ignored: a field that looks like it expresses
    placement must do so."""
    (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": "a",
                    },
                }
            ]
        )
    )[0]["result"]
    # Named parent wins, whoever is addressed — the whole point.
    (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "a",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": "c",
                    },
                }
            ]
        )
    )[0]["result"]
    assert kernel.get("c")["parent_id"] == kernel.ctx.agents["c"].parent.id == "a"

    # Absent, it is refused rather than guessed. There is no implicit parent:
    # "wherever this happened to be sent" is exactly the ambiguity being removed.
    bad = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    assert bad["reason"] == "missing_arg" and bad["arg"] == "parent_id"
    assert "hint" in bad, "a refusal a caller cannot act on costs the same as none"

    # A parent that names no agent is refused with the live id index, not created.
    ghost = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "nobody",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    assert ghost["reason"] == "unknown_agent" and "agents" in ghost


async def test_the_root_record_has_no_parent_whatever_meta_says(kernel):
    """The root is the one agent whose absent parent is structural — a
    `parent_id` on it would make `validate_records` see zero roots and refuse
    the whole document."""
    (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "update_agent",
                        "id": kernel.id,
                        "parent_id": "nonsense",
                    },
                }
            ]
        )
    )[0]["result"]
    assert "parent_id" not in kernel.ctx.root.record
    Kernel().load(kernel.ctx.save())


# ─── a3 — sidecars over one chunk came back truncated ───────────


@pytest.mark.parametrize("size", [10, 65_536, 65_537, 200_000])
async def test_a_sidecar_round_trips_whatever_its_size(kernel, size):
    """`file_bridge` caps a `read_stream` chunk at 65536 bytes — so a `load_blob`
    that issued ONE `read_stream` would return anything larger as a PREFIX,
    presented as the file, with no error anywhere.

    A memory agent's entire contract is that what it wrote is what it reads
    back, so a truncating read is data loss wearing a successful reply. The
    boundary sizes are parametrized because an off-by-one at exactly one chunk
    is the likeliest way to reintroduce it.
    """
    (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": "store",
                        "root": ".fantastic",
                        "ingress_rule": "allow_all_ingress",
                    },
                }
            ]
        )
    )[0]["result"]
    payload = ("y" * (size - 1) + "\n") if size else ""
    w = (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "persist_blob",
                        "agent_id": "probe",
                        "name": "state.yaml",
                        "content": payload,
                    },
                }
            ]
        )
    )[0]["result"]
    assert "error" not in w, w
    r = (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "load_blob",
                        "agent_id": "probe",
                        "name": "state.yaml",
                    },
                }
            ]
        )
    )[0]["result"]
    assert r["content"] == payload, f"{len(r['content'] or '')} back of {size}"


async def test_a_large_memory_survives_the_agent_that_owns_it(kernel):
    """The same defect at the level it actually bites: a text document that
    outgrew one stream chunk silently lost its tail on the next read."""
    (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": "store",
                        "root": ".fantastic",
                        "ingress_rule": "allow_all_ingress",
                    },
                }
            ]
        )
    )[0]["result"]
    blob = "z" * 90_000
    assert (
        "error"
        not in (
            await kernel.send(
                [
                    {
                        "target": "store",
                        "payload": {
                            "type": "write",
                            "path": "big.txt",
                            "bytes": blob.encode(),
                        },
                    }
                ]
            )
        )[0]["result"]
    )
    got = (
        await kernel.send(
            [{"target": "store", "payload": {"type": "read", "path": "big.txt"}}]
        )
    )[0]["result"]
    assert got["lines"][0]["text"] == blob


# ─── a6 — operator provenance: the escalation windows ───────────


async def test_an_agent_cannot_write_another_agents_namespace(kernel):
    """§10.3 leak site 2. `persist_blob` names its namespace in the PAYLOAD
    (`agent_id`), and the loader writes its own store as bookkeeping — so if an
    agent can reach the loader at all, it can ASK for any sidecar it likes. The
    loader's owner guard (`current_origin`) refuses a namespace that is not the
    caller's; this pins it for the id a caller supplies.
    """
    (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": "store",
                        "root": ".fantastic",
                        "ingress_rule": "allow_all_ingress",
                    },
                }
            ]
        )
    )[0]["result"]
    (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": "victim",
                    },
                }
            ]
        )
    )[0]["result"]
    (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "id": "thief",
                        "can_call": {"kernel_state": ["persist_blob"]},
                    },
                }
            ]
        )
    )[0]["result"]
    with as_agent(kernel, "thief"):
        r = (
            await kernel.ctx.agents["thief"].send(
                [
                    {
                        "target": kernel.id,
                        "payload": {
                            "type": "persist_blob",
                            "agent_id": "victim",
                            "name": "state.yaml",
                            "content": "owned: true\n",
                        },
                    }
                ]
            )
        )[0]["result"]
    # Whether it is refused at the gate or accepted, the SEND must not be a
    # silent success that wrote someone else's namespace.
    if "error" not in r:
        back = (
            await kernel.send(
                [
                    {
                        "target": kernel.id,
                        "payload": {
                            "type": "load_blob",
                            "agent_id": "victim",
                            "name": "state.yaml",
                        },
                    }
                ]
            )
        )[0]["result"]
        assert back.get("content") != "owned: true\n", (
            "an agent wrote another agent's sidecar through persist_blob"
        )


def test_sidecar_persistence_is_a_direct_loader_call():
    """A bundle persisting its OWN sidecar through the loader is bookkeeping: a
    DIRECT call of the loader's `persist_blob`/`load_blob` functions, never a send
    and never an identity switch. The namespace is the
    scheduler's own id — never read from a caller's payload."""
    import inspect

    from scheduler import tools as sch

    for fn, call in (
        (sch._file_read, "load_blob(loader, sid, name)"),
        (sch._file_write, "persist_blob(loader, sid, name, content)"),
    ):
        src = inspect.getsource(fn)
        assert call in src, f"{fn.__name__}: not a direct loader call"
        assert ".send(" not in src, f"{fn.__name__}: persistence must not be a send"
        assert "payload.get" not in src and "payload[" not in src, (
            f"{fn.__name__}: must not read a caller payload"
        )


async def test_a_bare_test_driver_is_an_operator_and_that_is_deliberate(kernel):
    """Stated because it makes half this suite look easier than it is: outside a
    dispatch `_current_sender` is None, so a test driver is an OPERATOR and
    skips the call gate entirely. Every negative authz test therefore stands in
    for an agent's handler explicitly (`as_agent`) — a test that forgets passes
    for the wrong reason."""
    from kernel import _current_sender

    assert _current_sender.get() is None
    with as_agent(kernel, kernel.id):
        assert _current_sender.get() == kernel.id
    assert _current_sender.get() is None
