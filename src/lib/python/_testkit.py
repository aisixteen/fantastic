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

from kernel_state.tools import hydrate_messages, read_tree, write_record

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
    # HYDRATE THE MESSAGE STORE, cold, before the first send mints an id. The kernel
    # starts empty; the loader bundle's reader fills it from above and sets the counter.
    hydrate_messages(k, root_dir)
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


# ─── running a PYTHON helper on the command runner ──────────────────────────
#
# exec_bridge is a general-purpose COMMAND runner: no `code`, no `shim`, no
# interpreter allowlist. A test that wants a python child speaking the kernel
# wire composes it itself — the wire client is DATA (`CONNECTOR_SRC`), so the
# test prepends it and runs `python -c <program>` like any other command.
import sys as _sys  # noqa: E402 — after the module prose above


def py_runner_meta(**extra) -> dict:
    """create_agent meta for an exec_bridge leg permitted to run `python -c …`."""
    return {
        "handler_module": "exec_bridge.tools",
        "allowed": [f"{_sys.executable} -c .*"],
        "ingress_rule": "allow_all_ingress",
        **extra,
    }


def py_start_payload(code: str, **extra) -> dict:
    """`start` payload running `code` under the python wire client."""
    from exec_bridge._connector import CONNECTOR_SRC

    return {
        "type": "start",
        "command": _sys.executable,
        "args": ["-c", CONNECTOR_SRC + code],
        **extra,
    }
