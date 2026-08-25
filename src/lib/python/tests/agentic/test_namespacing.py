"""`namespacing` — a child's state belongs to its POSITION in the tree.

The manifest declares two wings that are deliberately indistinguishable except
by where they sit: each `static_loader` owns one `yaml_state` memory and one
`file_bridge` space, the two memories persist through the SAME `.fantastic`
provider, and the two spaces are sibling directories. Nothing is separated by
medium, by id convention, or by a naming scheme. If the wings stay apart, the
tree position is the only thing that can be keeping them apart.

What the general runner already owns, and this file therefore does not repeat:
the declared shape (PC-1), whether each declared edge is reachable and which
half refuses (PC-2), that the roster is closed (PC-3), and that a reboot brings
back the same roster (PC-4). Those are claims about the WIRING. C20 is a claim
about the CONTENT that wiring carries — which document a `read` sees, which file
a path names, where the sidecar lands, and what survives rehydration — so every
test here asserts a VALUE or an EFFECT, never a denial shape on its own.

Two measured facts shape what is asserted:

- The sidecar path is the TREE path. `_blob_reldir` (`kernel_state/tools.py:774`)
  resolves the agent's own `_root_path` and makes it store-relative, so a memory
  lands at `agents/<loader>/agents/<id>/state.yaml`. `yaml_state`'s own module
  docstring still says `agents/<id>/state.yaml` — the flat, by-convention form.
  Under one loader the two are indistinguishable; under two they are not, which
  is exactly why this scenario declares two.

- The namespace key rides in the PAYLOAD. `persist_blob` takes `agent_id` from
  the caller (`kernel_state/tools.py:788-800`) and the store layer has nothing
  with which to tell a truthful key from a forged one. The call gate is the
  entire defence, so the forged write is asserted by its EFFECT on the victim's
  document, not by the refusal it came back with.
"""

from __future__ import annotations

import pytest

from _engine import check, dehydrate, denied, reach, reboot

SCENARIO = "namespacing"

# (loader, memory, file space) per wing. The loader is the sender in every probe:
# it reaches its own children on the structural baseline (`$children`), so the
# probes need no grant that the scenario would then be quietly testing instead of
# the namespacing.
WINGS = (("wing_a", "notes_a", "space_a"), ("wing_b", "notes_b", "space_b"))


@pytest.fixture
async def ns(hydrate_scenario):
    """The hydrated two-wing kernel. `require=True` (the default) refuses to hand
    back a kernel whose hydration did not do what the config declared — a
    half-built roster would make every isolation claim below vacuously true."""
    return await hydrate_scenario(SCENARIO)


async def _set(h, wing: str, memory: str, key: str, value) -> dict:
    """Write one key into a wing's own memory, insisting it actually landed.
    `yaml_state.set` failfasts when no store is wired (`no store wired —
    persistence not available`), and an unnoticed failfast reads as isolation."""
    reply = await reach(
        h.kernel, wing, memory, {"type": "set", "key": key, "value": value}
    )
    check(
        "C20",
        reply.get("set") is True,
        f"{wing} → {memory} set {key!r} did not land: {reply!r}",
    )
    return reply


async def _read(h, wing: str, memory: str, key: str) -> dict:
    """Read one key back, insisting the reply is an ANSWER and not a refusal. A
    denial also carries no `value`, so without this an accidentally-sealed edge
    would masquerade as a namespace boundary — the same class of false pass a
    field check produces."""
    reply = await reach(h.kernel, wing, memory, {"type": "read", "key": key})
    check(
        "C20",
        not denied(reply),
        f"{wing} → {memory} read was refused, so it proves nothing"
        f" about namespacing: {reply!r}",
    )
    return reply


async def test_a_key_set_in_one_wings_memory_is_absent_from_the_others(ns):
    """Both memories are `yaml_state` agents of the same mode, writing the same
    key through the same provider. The only difference between them is their
    parent — so if `notes_b` cannot see what `notes_a` wrote, position is what
    separated the two documents."""
    await _set(ns, "wing_a", "notes_a", "room", "a")

    mine = await _read(ns, "wing_a", "notes_a", "room")
    check("C20", mine.get("value") == "a", f"a memory lost its own key: {mine!r}")

    theirs = await _read(ns, "wing_b", "notes_b", "room")
    check(
        "C20",
        theirs.get("value") is None,
        f"notes_b read notes_a's key through the shared store: {theirs!r}",
    )


async def test_each_memorys_sidecar_lands_under_its_own_loader(ns):
    """The positional claim, read off the disk the two memories SHARE.

    One `.fantastic` provider serves both wings, so the store cannot be what
    keeps them apart — the path is. `_blob_reldir` makes the agent's own
    `_root_path` store-relative, which yields `agents/<loader>/agents/<id>`; the
    flat `agents/<id>` form that `yaml_state`'s docstring still describes would
    put both wings in one flat namespace, where two agents named alike under
    different parents would collide.

    Written through `persist_blob`, which writes to the provider immediately —
    unlike agent records, which wait for the debounced flush the test harness
    deliberately never starts."""
    for wing, memory, _ in WINGS:
        await _set(ns, wing, memory, "room", wing)

    files = set((await dehydrate(ns.kernel, disk=True))["disk_files"])
    for wing, memory, _ in WINGS:
        positional = f"agents/{wing}/agents/{memory}/state.yaml"
        check(
            "C20",
            positional in files,
            f"{memory}'s sidecar is not at its tree path {positional!r}:"
            f" {sorted(files)}",
        )
        check(
            "C20",
            f"agents/{memory}/state.yaml" not in files,
            f"{memory}'s sidecar ALSO landed in a flat by-id namespace: "
            f"{sorted(files)}",
        )


async def test_the_same_relative_path_in_two_file_spaces_names_two_files(ns):
    """The file-space half of the same property. Both spaces are open
    `file_bridge` legs and both are handed the identical relative path; the roots
    are one directory apart. A `read` therefore answers with whichever wing's
    bytes the caller's own root contains.

    Then the mismatched pairing: reaching the neighbour's file by path is refused
    by the clamp in `fs.resolve` (`file_bridge/fs.py:86`), not by the call gate —
    `space_b` is allowed to talk to `space_b`, and it is the ROOT that will not
    let the path out. Both defences are needed and only one of them is a grant."""
    for wing, _, space in WINGS:
        wrote = await reach(
            ns.kernel,
            wing,
            space,
            {"type": "write", "path": "note.txt", "content": f"from {wing}"},
        )
        check("C20", wrote.get("written") is True, f"{space} write failed: {wrote!r}")

    for wing, _, space in WINGS:
        got = await reach(ns.kernel, wing, space, {"type": "read", "path": "note.txt"})
        check(
            "C20",
            got.get("content") == f"from {wing}",
            f"{space} read the wrong wing's note.txt: {got!r}",
        )
        listed = await reach(ns.kernel, wing, space, {"type": "list", "path": ""})
        check(
            "C20",
            [f["name"] for f in listed.get("files", [])] == ["note.txt"],
            f"{space} sees more than its own root holds: {listed!r}",
        )

    escape = await reach(
        ns.kernel, "wing_b", "space_b", {"type": "read", "path": "../a/note.txt"}
    )
    check(
        "C20",
        "escapes root" in escape.get("error", ""),
        f"space_b read ACROSS its root into wing_a's space: {escape!r}",
    )
    check(
        "C20",
        "content" not in escape,
        f"a refused read still returned bytes: {escape!r}",
    )


async def test_a_child_cannot_write_into_a_neighbours_namespace_through_the_loader(ns):
    """`persist_blob` names its namespace in the PAYLOAD
    (`kernel_state/tools.py:788-800`): `agent_id` is the caller's word for whose
    sidecar this is, and the store layer has nothing with which to check it.
    Below the gate, `notes_a` writing its own document and `notes_a` claiming to
    be `notes_b` are the same call.

    So the manifest hands `notes_a` the egress it would need
    (`can_call: {kernel_state: [persist_blob]}`) and the write STILL cannot land:
    the root accepts `$children`, meaning its direct children, and a wing's
    memory is a grandchild. The refusal is positional — nobody validated the
    claimed `agent_id`, and nobody had to.

    The config declares that probe as an edge, so PC-2 already walks it and pins
    the SHAPE of the refusal — denied, at `ingress`, with a hint. Repeating that
    here would be two truths about one send. What is left, and what only this
    file can say, is the EFFECT: a denial that had written anyway satisfies every
    shape check unchanged, and the victim's document is the only place it shows.
    """
    await _set(ns, "wing_b", "notes_b", "room", "b")

    await reach(
        ns.kernel,
        "notes_a",
        ns.kernel.id,
        {
            "type": "persist_blob",
            "agent_id": "notes_b",
            "name": "state.yaml",
            "content": "room: forged\n",
        },
    )

    after = await _read(ns, "wing_b", "notes_b", "room")
    check(
        "C20",
        after.get("value") == "b",
        f"notes_b's document was overwritten from outside its namespace: {after!r}",
    )


async def test_each_wings_memory_returns_to_its_own_namespace_after_a_reboot(ns):
    """A sidecar is found by recomputing the path from the LIVE tree, so a reboot
    that rehydrated an agent anywhere but where it was would not error — it would
    quietly read an empty document, or somebody else's.

    PC-4 asserts the roster comes back identical; it never opens a sidecar. This
    is the content half: the reboot is the same one PC-4 runs (persist → fresh
    bootstrap → the production boot pass), and every memory must still answer
    with its own value and only its own."""
    for wing, memory, _ in WINGS:
        await _set(ns, wing, memory, "room", wing)

    after = await reboot(ns)

    for wing, memory, _ in WINGS:
        got = await _read(after, wing, memory, "room")
        check(
            "C20",
            got.get("value") == wing,
            f"{memory} came back with {got.get('value')!r}, not {wing!r}"
            f" — its sidecar was orphaned or crossed: {got!r}",
        )
        other = await _read(after, wing, memory, "neighbour")
        check(
            "C20",
            other.get("value") is None,
            f"{memory} rehydrated a key it never wrote: {other!r}",
        )
