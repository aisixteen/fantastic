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
from pathlib import Path

from helpers import e2e_root

#: Runtimes that are SPAWNED and driven over REST. python runs in-process
#: because a probe's own asserts read the live tree directly.
SPAWNED = ("rust", "swift")


def runtime_name() -> str:
    rt = os.environ.get("RUNTIME", "python")
    if rt not in ("python", *SPAWNED):
        raise SystemExit(f"RUNTIME must be python|{'|'.join(SPAWNED)} (got {rt!r})")
    return rt


async def open_kernel(prefix: str = "probe_", reach: list[str] | None = None):
    """`(root, core, close)` for the named runtime.

    `core` is the in-process `ai_core.core` module when there is one and `None`
    otherwise — the connector uses it to install the provider-wrapping reader,
    and falls back to reading the agent's history when it is absent.
    """
    rt = runtime_name()
    if rt == "python":
        # TEST-ONLY BUNDLES, in process. A spawned kernel gets `e2e/probes` on
        # its PYTHONPATH (see `helpers/host/driver.py`); the in-process one is
        # this process, so the same directory goes on `sys.path` here. Without
        # it `handler_module: user_probe.tools` is unimportable and the human
        # loads as a bare record that answers nothing.
        import sys

        probes = str(e2e_root() / "probes")
        if probes not in sys.path:
            sys.path.insert(0, probes)
        os.chdir(tempfile.mkdtemp(prefix=prefix))
        from _testkit import boot_root
        from ai_core import core

        from cli import Cli

        root = boot_root()
        Cli(root.ctx, parent=root)
        # THE MEMORY STORE, then the start sequence — as the daemon does.
        from helpers import memory

        await memory.wire_in_process(root)

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
        core._providers.clear()
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
