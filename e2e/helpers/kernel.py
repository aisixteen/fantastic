"""Open a kernel for a probe — spawned and driven over REST.

ONE call, so a probe never encodes which runtime it is on:

    root, core, close = await open_kernel()      # RUNTIME= picks it

`RUNTIME=rust` spawns that kernel. `RUNTIME=python` (the default) is refused:
the Python product keeps the old grant model (#170), and the in-process branch
is gone rather than serving two models.
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
SPAWNED = ("rust",)

#: THE PROVIDER SEAM, in process only. ai_core builds an AI agent's provider for
#: every pass from its live record (`core._build_provider`); `seat_providers`
#: patches that ONE function to consult this table by agent id. An entry is
#: `async wrap(build)`, `build()` building the provider ai_core would have used:
#: a reader WRAPS it (`wrap_with`), fresh each pass as the product builds it, so an
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
    `inner` becomes the provider ai_core built for this pass."""

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


async def open_kernel(prefix: str = "probe_", reach: list[str] | None = None,
                      root_verbs: list[str] | None = None):
    """`(root, core, close)` for the named runtime.

    `root_verbs` — root BUNDLE verbs (`reload_bundle`) the legs' line on the root
    names beside `reflect` (`HostKernel`), so `rest` can hand an AI that line.

    `core` is the in-process `ai_core.core` module when there is one and `None`
    otherwise — the connector wraps the provider (`WRAPS`) when it is there, and
    reads the agent's history when it is absent.
    """
    rt = runtime_name()
    if rt == "python":
        raise SystemExit("RUNTIME=python: #170 (the Python product keeps the old grant model)")

    from helpers.host.driver import HostKernel, ensure_runtime
    from helpers.host.wire import WireRoot

    repo = e2e_root().parent
    spec = ensure_runtime(repo, rt)
    wd = Path(tempfile.mkdtemp(prefix=f"{rt}_{prefix}"))
    os.chdir(wd)
    # `reach` names the ids OUTSIDE `rest`'s tree that the legs call; the seed
    # writes them into the legs' own `can` (nothing raises its own lines).
    # Agents a probe creates under `rest` need no entry: position reaches them.
    k = HostKernel(spec, wd, reach=list(reach or []), root_verbs=list(root_verbs or [])).boot()
    root = WireRoot(k)
    return root, None, root.close


async def reopen_kernel(root):
    """A NEW KERNEL PROCESS on the SAME directory — the wire twin of
    `_testkit.boot_root()` after `ctx.shutdown()`.

    The first process must already be gone (`root.ctx.shutdown()` stops it and
    waits). The tree under `.fantastic` is what survives: records, rows, the
    message log. `HostKernel.seed` is idempotent on an existing tree — it boots
    a seed process only when `agent.yaml` is absent, writes the root and the
    stores only on the first seed (their runtime grants survive), and rewrites
    the driver's own records (web/ws/rest, a fresh port) — so booting again here
    is the restart the probe means, not a fresh kernel wearing the old directory.

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
    k2 = HostKernel(spec, k.workdir, reach=list(k.reach),
                    root_verbs=list(getattr(k, "root_verbs", []))).boot()
    return WireRoot(k2)


async def restart(root, core):
    """STOP this kernel and START A NEW ONE on the same directory — the records on
    disk and the memory store's tables carried across. Returns `(root2, stop)`,
    `stop` being the stop's own report (`{"exit": 0}`).

    A `ram_store` dies with its process, so the harness carries its tables the way a
    durable store keeps them by itself: `dump` → stop → new kernel → `inject`, BEFORE
    the new kernel's first pass reads anything. The guid and the id counter come back
    with the root record, so every carried row keeps its key and no id repeats.

    The stop is the operator's signal (SIGTERM, a graceful shutdown): process verbs
    over REST are operator only. The dump is taken BEFORE it, so what the stop itself
    settles is not carried.
    """
    from helpers import memory

    if core is not None:
        raise SystemExit("restart in process: #170 (the Python product keeps the old grant model)")
    tables = await memory.dump(root)
    stop = await root.ctx.shutdown()
    root2 = await reopen_kernel(root)
    await memory.inject(root2, tables)
    return root2, stop


def home(root) -> str:
    """Where a probe CREATES its agents. In process the probe is the operator and
    places them under the root. Over a wire every call is the `rest` leg, and a
    mutation must come from the operand's direct parent — so the leg's own subtree
    is the test's world."""
    return getattr(root, "leg_id", None) or root.id
