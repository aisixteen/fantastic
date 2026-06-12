"""Test helpers for the decoupled-persistence kernel.

`Agent` no longer touches disk — a loader agent does, driven by the
state stream + a debounced flush. For deterministic tests we expose:

  - `boot_root()` — production-style bootstrap in the cwd: read the
    `.fantastic` tree (or seed a fresh `kernel_state` root, `id="kernel_state"`),
    rebuild it in memory, materialize the root's agent.json + readme.
    Returns the root Agent. Does NOT start the flush loop, so the
    ~480 logic tests stay pure in-memory (no background task, no
    per-create disk write).
  - `persist(root)` — synchronously write the whole live tree to disk
    via the loader (mirrors a full flush: every non-ephemeral record +
    its seeded readme). Deterministic, no debounce. Disk-lifecycle /
    reboot tests call this to materialize on-disk state.

Mirrors Rust's `StorageMode::{InMemory, Disk}` split: logic tests use
the in-memory shape; persistence tests opt into real disk.
"""

from __future__ import annotations

from pathlib import Path

from kernel_state.tools import read_tree, write_record

from kernel import Kernel


def boot_root(*, compose_cli: bool = False):
    """Bootstrap a root `kernel_state` agent in the cwd, like `main._bootstrap`.

    Reads an existing `.fantastic` tree if present (reboot), else seeds a
    fresh root. Returns the root Agent. The flush loop is NOT started —
    call `persist()` for on-disk state."""
    k = Kernel()
    root_dir = Path(".fantastic")
    records = read_tree(root_dir)
    if not records:
        records = [{"id": "kernel_state", "handler_module": "kernel_state.tools"}]
    k.load(records, root_path=root_dir)
    write_record(k.root._root_path, k.root.record)  # root agent.json + readme
    if compose_cli:
        from cli import Cli

        Cli(k, parent=k.root)
    return k.root


def persist(root) -> None:
    """Synchronously persist the whole live tree to disk via the loader
    (mirrors a full debounced flush): write every non-ephemeral agent's
    record + seed its readme."""
    for a in list(root.ctx.agents.values()):
        if not type(a).ephemeral:
            write_record(a._root_path, a.record)
