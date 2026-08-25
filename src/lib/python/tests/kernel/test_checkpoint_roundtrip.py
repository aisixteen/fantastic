"""C25 / C2 / C22 — the kernel document: one shape for init AND checkpoint.

Per md §7.2h. `Kernel.load` reads only `version` and `records` and ignores every
other top-level key, so a document the loader dumped goes back to `load`
VERBATIM. "A checkpoint is loadable as an init" is therefore not a property to
hope for — the checkpoint IS the argument. These tests pin that, and pin what
deliberately does NOT survive (§7.2c: records hydrate, live edges re-establish,
only in-flight work at an edge is lost).

`Kernel.save()` is the medium-agnostic half: it returns a dict and names no
path, which is the seam that keeps the kernel ignorant of disk. Nothing in
production calls it — these tests are where that contract is held.
"""

from __future__ import annotations

import yaml

from kernel import Kernel


def _doc(kernel) -> dict:
    """The kernel document, exactly as a loader would serialise it."""
    return yaml.safe_load(yaml.safe_dump(kernel.ctx.save(), sort_keys=False))


async def _seed(kernel) -> None:
    (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
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
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "yaml_state.tools",
                        "id": "memory",
                        "file_bridge_id": "store",
                        "callable_by": {"store": ["read"]},
                    },
                }
            ]
        )
    )[0]["result"]
    # A GRANDCHILD: nesting is structure, and a flat record list carries it in
    # `parent_id`. A document that could not express depth would quietly
    # flatten the tree it claims to restore.
    (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "memory",
                        "handler_module": "file_bridge.tools",
                        "id": "leaf",
                    },
                }
            ]
        )
    )[0]["result"]


# ─── C25 — round-trip fidelity ──────────────────────────────────


async def test_save_load_save_is_byte_identical(kernel):
    """The strongest form: not "equivalent", IDENTICAL. Anything that drifts
    between two saves is a field the document cannot carry, and it would rot
    silently across a restart."""
    await _seed(kernel)
    first = kernel.ctx.save()

    fresh = Kernel()
    fresh.load(first)
    second = fresh.save()

    assert second == first


async def test_the_document_survives_yaml_and_is_still_a_valid_load_argument(kernel):
    """Through the MEDIUM, not just through memory. yaml is where a `code`
    literal block, a None, or a nested dict would get mangled."""
    await _seed(kernel)
    before = kernel.ctx.save()

    fresh = Kernel()
    fresh.load(_doc(kernel))  # the yaml round-trip IS the argument
    assert fresh.save() == before


async def test_a_checkpoint_carries_structure_grants_and_postures(kernel):
    """Field-by-field, the things that would be silently lost: depth,
    both halves of the two-sided gate, and io posture. A document that dropped
    `callable_by` would restore a tree that looks right and refuses calls."""
    await _seed(kernel)
    recs = {r["id"]: r for r in kernel.ctx.save()["records"]}

    assert recs["leaf"]["parent_id"] == "memory"  # depth, not flattened
    assert recs["store"]["ingress_rule"] == "allow_all_ingress"  # io posture
    assert recs["memory"]["file_bridge_id"] == "store"  # weak binding by id
    assert "read" in recs["memory"]["callable_by"]["store"]  # ingress half
    assert recs["memory"]["can_call"]["$self"] == "*"  # the stamped baseline


async def test_awkward_values_survive_the_document(kernel):
    """None, empty containers, nested dicts, unicode, and a multi-line program.
    Each is a real record field somewhere, and each is a classic serialiser
    casualty — `code` especially, which rides as a literal block."""
    program = "import kernel\nkernel.call('x', {'type': 'ping'})\n"
    (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "id": "awkward",
                        "nothing": None,
                        "empty_map": {},
                        "empty_list": [],
                        "nested": {"a": {"b": [1, 2, {"c": None}]}},
                        "unicode": "kernel — ✓ · ⇒",
                        "code": program,
                    },
                }
            ]
        )
    )[0]["result"]
    rec = {r["id"]: r for r in _doc(kernel)["records"]}["awkward"]
    assert rec["nothing"] is None
    assert rec["empty_map"] == {} and rec["empty_list"] == []
    assert rec["nested"]["a"]["b"][2]["c"] is None
    assert rec["unicode"] == "kernel — ✓ · ⇒"
    assert rec["code"] == program


async def test_ephemeral_agents_are_never_in_the_document(kernel):
    """Composition is per-process. An ephemeral that round-tripped would be
    resurrected as a persisted agent on the next load — the opposite of what
    the flag means."""
    from cli import Cli

    Cli(kernel.ctx, parent=kernel)
    assert "cli" in kernel.ctx.agents
    assert "cli" not in {r["id"] for r in kernel.ctx.save()["records"]}


# ─── C2 — resume in a DIFFERENT kernel ──────────────────────────


async def test_a_checkpoint_loads_into_a_fresh_kernel_and_the_tree_behaves(kernel):
    """The cross-process case, minus the process: a document written by one
    kernel, loaded by another that shares nothing with it. Asserted by
    REACHABILITY — attempt the call, do not read a field."""
    await _seed(kernel)
    doc = _doc(kernel)

    fresh = Kernel()
    fresh.load(doc)
    assert set(fresh.agents) == set(kernel.ctx.agents)

    # The restored gate actually gates: a declared edge answers...
    reply = (
        await fresh.agents["store"].send(
            [{"target": "store", "payload": {"type": "reflect"}}]
        )
    )[0]["result"]
    assert reply.get("id") == "store"
    # ...and the io posture came back with it.
    assert reply.get("sealed") is False


async def test_process_state_does_not_survive_and_that_is_the_contract(kernel):
    """§7.2c: records hydrate, live edges RE-ESTABLISH, only in-flight work at
    an edge is lost. The task table is the sharpest case — its ids carry a
    per-process salt, so a document that restored them would hand out pids that
    name nothing. Bundles rebuild their own process state in `_boot`."""
    await _seed(kernel)
    doc = _doc(kernel)

    for rec in doc["records"]:
        assert "tasks" not in rec and "task_seq" not in rec

    fresh = Kernel()
    fresh.load(doc)
    assert fresh.task_rows() == [], "a loaded tree starts with no in-flight work"


# ─── C22 — version tolerance ────────────────────────────────────


async def test_an_unknown_top_level_key_is_ignored_rather_than_refused(kernel):
    """The loader owns keys the kernel never reads (`origin:` provenance,
    `state:` sidecars). `load` reading ONLY version+records is what lets the
    document grow without a kernel change — and what lets an OLDER kernel read a
    NEWER document instead of refusing it."""
    await _seed(kernel)
    doc = _doc(kernel)
    doc["origin"] = {"written_by": "kernel_state", "reason": "shutdown"}
    doc["state"] = {"memory": {"state.yaml": "beat: 1\n"}}
    doc["a_key_from_the_future"] = ["whatever"]

    fresh = Kernel()
    fresh.load(doc)  # must not raise
    assert set(fresh.agents) == set(kernel.ctx.agents)


async def test_an_unknown_record_field_survives_instead_of_being_dropped(kernel):
    """A record is open: every non-structural key is meta. A newer runtime's
    field must round-trip through an older one untouched, or a downgrade
    silently strips configuration."""
    await _seed(kernel)
    (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "update_agent",
                        "id": "memory",
                        "future_field": {"nested": True},
                    },
                }
            ]
        )
    )[0]["result"]
    fresh = Kernel()
    fresh.load(_doc(kernel))
    rec = {r["id"]: r for r in fresh.save()["records"]}["memory"]
    assert rec["future_field"] == {"nested": True}


async def test_a_bare_record_list_still_loads(kernel):
    """`load` accepts a bare list or a {version, records} envelope. The bare
    form predates the envelope; dropping it would break any document written
    before the version field existed."""
    await _seed(kernel)
    records = kernel.ctx.save()["records"]

    fresh = Kernel()
    fresh.load(records)
    assert set(fresh.agents) == set(kernel.ctx.agents)


# ─── the document is HAND-AUTHORED, so it can be wrong ──────────


async def test_a_reserved_key_is_refused_before_the_tree_is_touched(kernel):
    """`root_path` / `ctx` / `parent` are `Agent.__init__` parameters, and every
    non-structural record key becomes `**meta`. Before this was validated,
    `load` CLEARED THE TREE and then raised `TypeError: got multiple values for
    keyword argument 'root_path'` — leaving `agents == {}` and `root is None`.
    A kernel bricked by a half-destroyed tree, from one plausible typo in a
    hand-authored document: `root_path` is exactly the key someone writing
    "where does this tree live" would invent.

    The tree must be INTACT afterwards. That is the whole point of validating
    before the teardown."""
    import pytest

    from kernel._state import SnapshotError

    await _seed(kernel)
    doc = _doc(kernel)
    before = set(kernel.ctx.agents)
    doc["records"][0]["root_path"] = "/tmp/somewhere"

    with pytest.raises(SnapshotError, match="reserved key"):
        kernel.ctx.load(doc)

    assert set(kernel.ctx.agents) == before, "a refused load must change nothing"
    assert kernel.ctx.root is not None


async def test_the_document_never_names_where_the_tree_lives(kernel):
    """A checkpoint is portable between project dirs precisely because it is
    LOCATED, not addressed: the root's dir is the dir the document sits in.
    A path inside the document would pin it to the machine that wrote it."""
    await _seed(kernel)
    for rec in _doc(kernel)["records"]:
        assert not (set(rec) & {"root_path", "ctx", "parent"})
