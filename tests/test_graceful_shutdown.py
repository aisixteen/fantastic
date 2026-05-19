"""Graceful shutdown — depth-first hook walk, records survive.

`Kernel.shutdown()` is the substrate-level companion to cascade-delete.
Same depth-first traversal, but instead of wiping records + disk it
only invokes each bundle's process-teardown hook (`on_shutdown`, or
`on_delete` as fallback) so PTYs, subprocesses like `code serve-web`,
and uvicorn die before the daemon exits — without losing the agent
tree the next boot rehydrates from. Signal handlers in `_modes._default`
trigger this on SIGTERM/SIGINT/SIGHUP; an atexit safety net in
`main.py` catches paths that miss the in-loop finally.
"""

from __future__ import annotations

import types


def _stub_bundle(
    seeded_kernel,
    name: str,
    *,
    on_shutdown=None,
    on_delete=None,
):
    """Plant a fake handler module in sys.modules under `name` so an
    agent's `importlib.import_module(name)` resolves to a stub
    carrying whatever hooks the test wants to observe."""
    import sys

    mod = types.ModuleType(name)
    mod.VERBS = {}  # type: ignore[attr-defined]

    async def handler(id, payload, kernel):  # pragma: no cover — unused here
        return None

    mod.handler = handler  # type: ignore[attr-defined]
    if on_shutdown is not None:
        mod.on_shutdown = on_shutdown  # type: ignore[attr-defined]
    if on_delete is not None:
        mod.on_delete = on_delete  # type: ignore[attr-defined]
    sys.modules[name] = mod
    return mod


async def test_shutdown_walks_depth_first(seeded_kernel):
    """Parent's hook fires AFTER all descendants' hooks — same order
    contract as `_cascade_delete`. Lets PTYs die before terminal_webapp
    drops, serve-web before vscode_fantastic, etc."""
    order: list[str] = []

    async def make_hook(tag: str):
        async def _hook(agent):
            order.append(f"{tag}:{agent.id}")

        return _hook

    p_hook = await make_hook("p")
    c1_hook = await make_hook("c1")
    c2_hook = await make_hook("c2")
    _stub_bundle(seeded_kernel, "test_shutdown_p", on_shutdown=p_hook)
    _stub_bundle(seeded_kernel, "test_shutdown_c1", on_shutdown=c1_hook)
    _stub_bundle(seeded_kernel, "test_shutdown_c2", on_shutdown=c2_hook)

    seeded_kernel.create("test_shutdown_p", id="P")
    p = seeded_kernel.ctx.agents["P"]
    p.create("test_shutdown_c1", id="C1")
    p.create("test_shutdown_c2", id="C2")

    await seeded_kernel.ctx.shutdown()

    # Both children must have fired before the parent.
    assert order.index("c1:C1") < order.index("p:P")
    assert order.index("c2:C2") < order.index("p:P")


async def test_shutdown_does_not_touch_records_or_tree(seeded_kernel, tmp_path):
    """Shutdown is the OPPOSITE of cascade-delete: hooks run, tree
    stays. ctx.agents, parent._children, and the on-disk agent.json
    must all survive so the next boot rehydrates the same tree."""
    fired: list[str] = []

    async def hook(agent):
        fired.append(agent.id)

    _stub_bundle(seeded_kernel, "test_shutdown_keep", on_shutdown=hook)
    seeded_kernel.create("test_shutdown_keep", id="K")
    k = seeded_kernel.ctx.agents["K"]
    k.create("test_shutdown_keep", id="K_child")

    root = seeded_kernel.ctx.root
    assert "K" in seeded_kernel.ctx.agents
    assert "K_child" in seeded_kernel.ctx.agents
    assert "K" in root._children
    k_dir = k._root_path
    child_dir = seeded_kernel.ctx.agents["K_child"]._root_path
    assert k_dir.is_dir()
    assert child_dir.is_dir()

    await seeded_kernel.ctx.shutdown()

    # Hooks fired on every agent in the subtree.
    assert set(fired) == {"K", "K_child"}
    # And nothing was removed.
    assert "K" in seeded_kernel.ctx.agents
    assert "K_child" in seeded_kernel.ctx.agents
    assert "K" in root._children
    assert k_dir.is_dir()
    assert child_dir.is_dir()


async def test_shutdown_falls_back_to_on_delete(seeded_kernel):
    """Bundles that only define `on_delete` (terminal_backend,
    vscode_fantastic, etc.) get their teardown invoked unchanged —
    `on_shutdown` is the explicit name when a bundle wants to
    distinguish shutdown from delete, but for "kill subprocesses
    either way" bundles, `on_delete` is reused."""
    fired: list[str] = []

    async def od_hook(agent):
        fired.append(f"on_delete:{agent.id}")

    _stub_bundle(seeded_kernel, "test_shutdown_only_od", on_delete=od_hook)
    seeded_kernel.create("test_shutdown_only_od", id="OD")

    await seeded_kernel.ctx.shutdown()

    assert fired == ["on_delete:OD"]


async def test_shutdown_prefers_on_shutdown_over_on_delete(seeded_kernel):
    """If a bundle defines BOTH hooks, shutdown calls on_shutdown.
    on_delete stays reserved for cascade-delete (where the disk
    artifact will be wiped too)."""
    fired: list[str] = []

    async def os_hook(agent):
        fired.append(f"on_shutdown:{agent.id}")

    async def od_hook(agent):  # should NOT be called
        fired.append(f"on_delete:{agent.id}")

    _stub_bundle(
        seeded_kernel,
        "test_shutdown_both",
        on_shutdown=os_hook,
        on_delete=od_hook,
    )
    seeded_kernel.create("test_shutdown_both", id="B")

    await seeded_kernel.ctx.shutdown()

    assert fired == ["on_shutdown:B"]


async def test_shutdown_is_idempotent(seeded_kernel):
    """Signal handler + atexit can both reach `Kernel.shutdown()` on a
    cleanly-shutting daemon. Second call must be a no-op so bundles
    aren't double-torn-down."""
    fired: list[str] = []

    async def hook(agent):
        fired.append(agent.id)

    _stub_bundle(seeded_kernel, "test_shutdown_idem", on_shutdown=hook)
    seeded_kernel.create("test_shutdown_idem", id="I")

    await seeded_kernel.ctx.shutdown()
    await seeded_kernel.ctx.shutdown()

    assert fired == ["I"]
    assert seeded_kernel.ctx._shutdown_complete is True


async def test_shutdown_continues_on_hook_exception(seeded_kernel):
    """One bundle's teardown can't take down the whole shutdown —
    sibling agents and ancestors must still get their hooks called.
    Mirrors `_cascade_delete`'s exception-swallowing behaviour."""
    fired: list[str] = []

    async def good(agent):
        fired.append(agent.id)

    async def bad(agent):
        fired.append(f"raised:{agent.id}")
        raise RuntimeError("teardown blew up")

    _stub_bundle(seeded_kernel, "test_shutdown_good", on_shutdown=good)
    _stub_bundle(seeded_kernel, "test_shutdown_bad", on_shutdown=bad)
    seeded_kernel.create("test_shutdown_good", id="A")
    seeded_kernel.create("test_shutdown_bad", id="B")
    seeded_kernel.create("test_shutdown_good", id="C")

    await seeded_kernel.ctx.shutdown()

    # All three hooks were invoked even though B raised.
    assert "A" in fired
    assert "raised:B" in fired
    assert "C" in fired
