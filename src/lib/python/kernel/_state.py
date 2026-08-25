"""KernelState — the serializable snapshot of a kernel's agent tree.

Mirrors Rust `state.rs`: a FLAT list of agent records where `parent_id`
encodes structure. `Kernel.save()` produces it; `Kernel.load()` rebuilds
the live tree from it (DFS from root, weak-load). The MEDIUM (disk /
in-memory / bridge) is owned by a LOADER agent, never the kernel — the
kernel only converts between the live tree and this flat list.
"""

from __future__ import annotations

# Snapshot schema version. Bump when the wire shape breaks; a kernel
# reads `version <= CURRENT` and refuses `version > CURRENT`.
CURRENT_VERSION = 1


class SnapshotError(ValueError):
    """A KernelState snapshot failed validation (bad version, no/multiple
    roots, dangling parent_id, duplicate id)."""


# Constructor parameters of `Agent.__init__` that a record key would collide
# with. Not meta, and never writable from a document.
RESERVED_RECORD_KEYS = frozenset({"root_path", "ctx", "parent"})


def validate_records(records: list[dict], *, version: int = CURRENT_VERSION) -> str:
    """Validate a flat record list; return the single root id.

    Raises SnapshotError on: version > CURRENT, missing/duplicate id, not
    exactly one root (`parent_id` null/absent), a `parent_id` that doesn't
    resolve within the set, or a RESERVED key. Mirrors Rust `Kernel::load`'s
    checks.

    RESERVED KEYS. Every non-structural record key becomes `**meta` on
    `Agent.__init__`, so a record carrying `root_path` / `ctx` / `parent`
    collides with a real parameter. Measured: `Kernel.load` CLEARS THE TREE and
    only then raises `TypeError: Agent() got multiple values for keyword
    argument 'root_path'` — leaving `agents == {}` and `root is None`, a kernel
    bricked by a half-destroyed tree, from a document a human hand-authored.
    `root_path` is exactly the key someone writing a "where does this tree live"
    file would invent, so this is a likely typo rather than a theoretical one.
    It has to be caught HERE, because this runs before the teardown.
    """
    if version > CURRENT_VERSION:
        raise SnapshotError(
            f"snapshot version {version} exceeds this kernel's max ({CURRENT_VERSION})"
        )
    ids: set[str] = set()
    roots: list[str] = []
    for r in records:
        rid = r.get("id")
        if not rid:
            raise SnapshotError("snapshot record missing id")
        reserved = RESERVED_RECORD_KEYS & set(r)
        if reserved:
            raise SnapshotError(
                f"record {rid!r} carries reserved key(s) {sorted(reserved)} —"
                " these are Agent constructor parameters, not meta; the tree"
                " lives where its document lives and is never named in a record"
            )
        if rid in ids:
            raise SnapshotError(f"duplicate agent id {rid!r} in snapshot")
        ids.add(rid)
        if r.get("parent_id") is None:
            roots.append(rid)
    if not roots:
        raise SnapshotError("snapshot has no root (no record with parent_id == null)")
    if len(roots) > 1:
        raise SnapshotError(f"snapshot has {len(roots)} roots; expected exactly one")
    for r in records:
        pid = r.get("parent_id")
        if pid is not None and pid not in ids:
            raise SnapshotError(f"agent {r['id']!r} parent {pid!r} not in snapshot")
    return roots[0]
