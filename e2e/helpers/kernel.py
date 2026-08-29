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


async def open_kernel(prefix: str = "probe_"):
    """`(root, core, close)` for the named runtime.

    `core` is the in-process `ai_core.core` module when there is one and `None`
    otherwise — the connector uses it to install the provider-wrapping reader,
    and falls back to reading the agent's history when it is absent.
    """
    rt = runtime_name()
    if rt == "python":
        os.chdir(tempfile.mkdtemp(prefix=prefix))
        from _testkit import boot_root  # noqa: E402
        from ai_core import core  # noqa: E402
        from cli import Cli  # noqa: E402

        root = boot_root()
        Cli(root.ctx, parent=root)

        async def close() -> None:
            return None

        return root, core, close

    from helpers.host.driver import HostKernel, ensure_runtime  # noqa: E402
    from helpers.host.wire import WireRoot  # noqa: E402

    repo = e2e_root().parent
    spec = ensure_runtime(repo, rt)
    wd = Path(tempfile.mkdtemp(prefix=f"{rt}_{prefix}"))
    os.chdir(wd)
    # Every id a probe creates must be declared: authority flows from ABOVE and
    # the REST leg may not raise its own (see driver docstring).
    reach = [s for s in os.environ.get("REACH", "").split(",") if s]
    k = HostKernel(spec, wd, reach=reach).boot()
    root = WireRoot(k)
    return root, None, root.close
