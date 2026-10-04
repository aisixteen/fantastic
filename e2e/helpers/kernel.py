"""Open a kernel for a probe — in-process for python, spawned for the rest.

ONE call, so a probe never encodes which runtime it is on:

    root, core, close = await open_kernel()      # RUNTIME= picks it

`RUNTIME` unset means python, which is the runtime a probe is developed
against; naming any other spawns that kernel and drives it over REST. The
returned `root` speaks the same `.id` / `.send([...])` either way.
"""

from __future__ import annotations

import os
import tempfile
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Any

from helpers import e2e_root

#: Runtimes that are SPAWNED and driven over REST. python runs in-process
#: because a probe's own asserts read the live tree directly.
SPAWNED = ("rust", "swift")

#: THE PROVIDER SEAM, in process only. ai_core builds an AI agent's provider for
#: every turn from its live record (`core._build_provider`); `seat_providers`
#: patches that ONE function to consult this table by agent id. An entry is
#: `async wrap(build)`, `build()` building the provider ai_core would have used:
#: a reader WRAPS it (`wrap_with`), fresh each turn as the product builds it, so an
#: `update_agent` mid-run reaches the model; a stub REPLACES it (`replace_with`) and
#: nothing is built. An id with no entry turns on the product's own provider.
WRAPS: dict[str, Callable[[Callable[[], Awaitable[Any]]], Awaitable[Any]]] = {}

_real_build = None


async def _seated_build(self_id: str, kernel, cfg):
    wrap = WRAPS.get(self_id)
    if wrap is None:
        return await _real_build(self_id, kernel, cfg)
    return await wrap(lambda: _real_build(self_id, kernel, cfg))


def seat_providers(core) -> None:
    """Route `core._build_provider` through `WRAPS` — once per process, where the
    harness boots its in-process kernel."""
    global _real_build
    if _real_build is None:
        _real_build = core._build_provider
        core._build_provider = _seated_build


def wrap_with(reader):
    """A `WRAPS` entry that puts `reader` between ai_core and the model: its
    `inner` becomes the provider ai_core built for this turn."""

    async def wrap(build):
        reader.inner = await build()
        return reader

    return wrap


def replace_with(provider):
    """A `WRAPS` entry that hands ai_core `provider` instead of building one."""

    async def wrap(build):
        return provider

    return wrap


def runtime_name() -> str:
    rt = os.environ.get("RUNTIME", "python")
    if rt not in ("python", *SPAWNED):
        raise SystemExit(f"RUNTIME must be python|{'|'.join(SPAWNED)} (got {rt!r})")
    return rt


async def open_kernel(prefix: str = "probe_", reach: list[str] | None = None):
    """`(root, core, close)` for the named runtime.

    `core` is the in-process `ai_core.core` module when there is one and `None`
    otherwise — the connector wraps the provider (`WRAPS`) when it is there, and
    reads the agent's history when it is absent.
    """
    rt = runtime_name()
    if rt == "python":
        # TEST-ONLY BUNDLES, in process. A spawned kernel runs
        # `probes/scenario_probes.py` (see `helpers/host/driver.py`); the
        # in-process one is this process, so the same directory goes on
        # `sys.path` and the probes are REGISTERED here — the kernel resolves a
        # handler_module only from its catalog plus `register_unlisted`. Without
        # it `handler_module: user_probe.tools` is unresolvable and the human
        # loads as a record that answers nothing.
        import sys

        probes = str(e2e_root() / "probes")
        if probes not in sys.path:
            sys.path.insert(0, probes)
        import scenario_probes

        scenario_probes.register()
        os.chdir(tempfile.mkdtemp(prefix=prefix))
        from _testkit import boot_root
        from ai_core import core

        from cli import Cli

        seat_providers(core)
        root = boot_root()
        Cli(root.ctx, parent=root)
        # THE MEMORY STORE, then the start sequence — as the daemon does.
        from helpers import creds, memory

        await memory.wire_in_process(root)
        # THE KEYS — a backend reads its key from the `creds` store, never the env.
        creds.wire_in_process(root, creds.env_refs())
        # THE ROOT accepts every declared id — kernel verbs (`list_agents`,
        # `create_agent`) are gated on its `callable_by` like any other target, and
        # nothing is stamped. The same seed the spawned-kernel driver writes.
        if reach:
            cb = dict(root._meta.get("callable_by") or {})
            cb.update({t: "*" for t in reach if t not in (root.id, "kernel")})
            root.ctx.update(root.id, callable_by=cb)

        async def close() -> None:
            return None

        return root, core, close

    from helpers.host.driver import HostKernel, ensure_runtime
    from helpers.host.wire import WireRoot

    repo = e2e_root().parent
    spec = ensure_runtime(repo, rt)
    wd = Path(tempfile.mkdtemp(prefix=f"{rt}_{prefix}"))
    os.chdir(wd)
    # Every id the probe will touch must be DECLARED: authority flows from above
    # and the REST leg may not raise its own (see the driver docstring). In
    # process this is free — the probe is the operator — so only the wire needs
    # it, and the probe passes the same list it already writes into `CAN_CALL`.
    k = HostKernel(spec, wd, reach=list(reach or [])).boot()
    root = WireRoot(k)
    return root, None, root.close


async def reopen_kernel(root):
    """A NEW KERNEL PROCESS on the SAME directory — the wire twin of
    `_testkit.boot_root()` after `ctx.shutdown()`.

    The first process must already be gone (`root.ctx.shutdown()` stops it and
    waits). The tree under `.fantastic` is what survives: records, rows, the
    message log. `HostKernel.seed` is idempotent on an existing tree — it boots
    a seed process only when `agent.yaml` is absent, then rewrites the driver's
    own records (web/ws/rest) to the same bytes — so booting again here is the
    restart the probe means, not a fresh kernel wearing the old directory.

    In process there is nothing to reopen: `boot_root()` is the caller's, and it
    keeps its own `core` module state. This returns `None` there so a probe can
    branch on it the way it branches on `core`.
    """
    k = getattr(root, "k", None)
    if k is None:
        return None
    from helpers.host.wire import WireRoot
    from helpers.host.driver import HostKernel

    spec = {"cmd": k.cmd, "root_id": k.root_id, "env": {}}
    k2 = HostKernel(spec, k.workdir, reach=list(k.reach)).boot()
    return WireRoot(k2)


async def restart(root, core):
    """STOP this kernel and START A NEW ONE on the same directory — the records on
    disk and the memory store's tables carried across. Returns `(root2, stop)`,
    `stop` being the agent half's own report (`shutdown_agents`).

    A `ram_store` dies with its process, so the harness carries its tables the way a
    durable store keeps them by itself: `dump` → new kernel → `inject`, BEFORE the new
    kernel's first turn reads anything. The guid and the id counter come back with the
    root record, so every carried row keeps its key and no id repeats.

    IN PROCESS the stop is `shutdown_agents` + `Kernel.shutdown()` (drain, every call
    settled, the last rows flushed) and the dump is taken AFTER it, so it holds the
    stop's own endings. OVER THE WIRE the kernel is a process and a draining kernel
    refuses new work, so the dump is taken BEFORE the stop — what the stop itself
    settles is not carried.
    """
    from helpers import memory

    async def _k(r, payload):
        return (await r.send([{"target": "kernel", "payload": payload}]))[0]["result"]

    if core is not None:
        from _testkit import boot_root, persist
        from cli import Cli

        from kernel._kernel import shutdown_context

        stop = await root.ctx.shutdown_agents()
        await root.ctx.shutdown()
        with shutdown_context():
            tables = await memory.dump(root)
        persist(root)
        WRAPS.clear()
        core._projection.clear()
        core._compaction_mark.clear()
        root2 = boot_root()
        Cli(root2.ctx, parent=root2)
        await memory.inject(root2, tables)
        out = await root2.ctx.start(boot_all=True)
        if not out.get("started"):
            raise SystemExit(f"kernel_start refused after restart: {out}")
        return root2, stop
    tables = await memory.dump(root)
    stop = await _k(root, {"type": "shutdown_agents"})
    await root.ctx.shutdown()
    root2 = await reopen_kernel(root)
    await memory.inject(root2, tables)
    return root2, stop


def home(root) -> str:
    """Where a probe CREATES its agents. In process the probe is the operator and
    places them under the root. Over a wire every call is the `rest` leg, and a
    mutation must come from the operand's direct parent — so the leg's own subtree
    is the test's world (owner, 2026-10-04, option a)."""
    return getattr(root, "leg_id", None) or root.id
