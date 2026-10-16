"""Test helpers for the decoupled-persistence kernel.

`Agent` never touches disk — a loader agent does, driven by the
state stream + a debounced flush. For deterministic tests we expose:

  - `boot_root()` — production-style bootstrap in the cwd: read the
    `.fantastic` tree (or seed a fresh `kernel_state` root, `id="kernel_state"`),
    rebuild it in memory, materialize the root's agent.yaml + readme.
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

from file_bridge import fs
from kernel_state.tools import read_tree, write_record

from kernel import Kernel


def boot_root(*, compose_cli: bool = False):
    """Bootstrap a root `kernel_state` agent in the cwd, like `main._bootstrap`.

    Reads an existing `.fantastic` tree if present (reboot), else seeds a
    fresh root. Returns the root Agent. The flush loop is NOT started —
    call `persist()` for on-disk state."""
    k = Kernel()
    root_dir = Path(".fantastic")
    records = read_tree(fs.Root(root_dir.resolve().parent, root_dir))
    if not any(r.get("parent_id") is None for r in records):  # no root record on disk
        records = [
            {"id": "kernel_state", "handler_module": "kernel_state.tools"}
        ] + records
    k.load(records, root_path=root_dir)
    write_record(fs.Root(k.workdir, k.root._root_path), k.root.record)  # + readme
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
            write_record(fs.Root(root.ctx.workdir, a._root_path), a.record)


# ─── the OS a test runs on ───────────────────────────────────────────────────
#
# THE tests' one OS switch — the twin of the packages' `_sys` modules (which hold
# every OS-specific fixture a test reads: `file_bridge._sys.set_mode`,
# `exec_bridge._sys.session_members`, …). A test of a concept Windows does not
# have is marked `posix_only("<why>")`; a value that differs by OS is `per_os`.
import os as _os  # noqa: E402 — after the module prose above
import re as _re  # noqa: E402
import shlex as _shlex  # noqa: E402
import signal as _signal  # noqa: E402
import subprocess as _subprocess  # noqa: E402

import pytest as _pytest  # noqa: E402

WINDOWS = _os.name == "nt"


def posix_only(reason: str):
    """Skip on Windows: the test is of a concept Windows does not have — `reason`
    says which, in one line."""
    return _pytest.mark.skipif(WINDOWS, reason=reason)


def per_os(*, posix, nt):
    """`posix` on POSIX, `nt` on Windows — a fixture value that differs by OS."""
    return nt if WINDOWS else posix


#: Popen kwargs for a child a test stops politely (`polite_stop`): on Windows it
#: leads its own process group, the one a CTRL_BREAK addresses.
POLITE_SPAWN = (
    {"creationflags": _subprocess.CREATE_NEW_PROCESS_GROUP} if WINDOWS else {}
)


def polite_stop(p) -> None:
    """Ask a child spawned with `POLITE_SPAWN` to stop — its SIGTERM: SIGTERM on
    POSIX, CTRL_BREAK (the kernel's SIGBREAK) on Windows."""
    p.send_signal(_signal.CTRL_BREAK_EVENT if WINDOWS else _signal.SIGTERM)


#: A python program's first line, making its stdout/stderr end a line with `\n`:
#: Windows' text mode writes `\r\n`, and a job keeps the `\r` as data.
LF_STDIO = (
    "import sys; sys.stdout.reconfigure(newline='\\n'); "
    "sys.stderr.reconfigure(newline='\\n')\n"
)


def command_rx(program: str) -> str:
    """An `allowed` regex fragment matching `program` as exec_bridge's shell-joined
    line spells it (`shlex.quote` — a Windows path with `\\` is quoted)."""
    return _re.escape(_shlex.quote(program))


# ─── running a PYTHON helper on the command runner ──────────────────────────
#
# exec_bridge is a general-purpose COMMAND runner. A test that wants a python
# child speaking the kernel wire composes it itself — the wire client is DATA (`CONNECTOR_SRC`), so the
# test prepends it and runs `python -c <program>` like any other command.
import sys as _sys  # noqa: E402 — after the module prose above


def child_env(*names: str) -> list[str]:
    """An `inherit_env` naming `names` plus what a child needs from the OS just to
    start (`exec_bridge._sys.OS_ENV`: nothing on POSIX, SYSTEMROOT on Windows —
    the connector's socket needs it)."""
    from exec_bridge._sys import OS_ENV

    return [*names, *OS_ENV]


def child_environ(**env: str) -> dict:
    """A whole environment for a child a test spawns itself: `env` plus the values
    of `exec_bridge._sys.OS_ENV` from this process (none on POSIX)."""
    from exec_bridge._sys import OS_ENV

    return {**{n: _os.environ[n] for n in OS_ENV if n in _os.environ}, **env}


def py_runner_meta(**extra) -> dict:
    """create_agent meta for an exec_bridge leg permitted to run `python -c …`."""
    os_env = child_env()
    return {
        "handler_module": "exec_bridge.tools",
        "allowed": [f"{command_rx(_sys.executable)} -c .*"],
        "ingress_rule": "allow_all_ingress",
        "cwd": ".",
        **({"inherit_env": os_env} if os_env else {}),
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


# ─── an AI agent's MEMORY STORE ──────────────────────────────────────────────
#
# An AI agent keeps its conversation in the memory store its record names, and the
# gates between the two are EXPLICIT on both ends (no defaults). Tests compose it the
# way an operator would: one `ram_store` under the root, the agent granted `can_call`
# on it, the store's `callable_by` opened for the agent. The store keys rows by the
# kernel's guid, which a booted root mints; an unbooted test root gets one here.


# ─── acting as an agent ──────────────────────────────────────────────────────


def as_agent(kernel, agent_id: str):
    """The test standing in for `agent_id`'s HANDLER: the enclosed sends are that
    agent's, exactly as when the kernel runs the agent's own code
    (`Agent._as_self` — the dispatch identity, the only identity there is). A send
    made outside it is the operator's. A test driver, never a product path."""
    return kernel.ctx.agents[agent_id]._as_self()


# ─── explicit grants (the call gate's baseline is `{$self: "*"}` only) ───────


def wire(kernel, sender: str, target: str, verbs="*") -> None:
    """Open the edge `sender` → `target` on BOTH ends — `sender.can_call[target]`
    and `target.callable_by[sender]` — merged into the live records, as an
    operator would. `verbs` is `"*"` (every verb) or a list."""
    ctx = kernel.ctx
    for holder, field, other in (
        (sender, "can_call", target),
        (target, "callable_by", sender),
    ):
        cur = dict(ctx.agents[holder]._meta.get(field) or {})
        cur[other] = verbs
        ctx.update(holder, **{field: cur})


def ensure_memory(root, store: str = "memory") -> str:
    """The shared test memory store under the root (created on first use); the root
    gets a guid if it was never booted. Returns the store id."""
    ctx = root.ctx
    if not root.record.get("guid"):
        ctx.update(root.id, guid="test-guid")
    if store not in ctx.agents:
        rec = root.create("ram_store.tools", id=store)
        assert rec.get("id") == store, rec
    return store


def memory_meta(
    root, ai_id: str, can_call: dict | None = None, store: str = "memory"
) -> dict:
    """create_agent meta for AI agent `ai_id`: `memory_store_id` + `can_call` on the
    store (merged into `can_call`). Creates the store on first use and opens its
    `callable_by` for `ai_id`."""
    ctx = root.ctx
    ensure_memory(root, store)
    cb = dict(ctx.agents[store]._meta.get("callable_by") or {})
    cb[ai_id] = "*"
    ctx.update(store, callable_by=cb)
    return {"memory_store_id": store, "can_call": {**(can_call or {}), store: "*"}}
