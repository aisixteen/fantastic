"""Per-agent readme system.

Every bundle ships a `readme.md` package resource. `reflect` with
`readme: true` reads it FROM THE BUNDLE — nothing is copied into an
agent's dir (owner 2026-10-14). Default reflect stays lean.
"""

from __future__ import annotations

from pathlib import Path

from _testkit import persist


async def test_persist_copies_no_readme(kernel):
    """Persisting an agent writes its record only — the readme stays in the
    bundle, so no `readme.md` or `docs/` lands in the agent's dir."""
    rec = kernel.create("file_bridge.tools", allow=["."])
    persist(kernel)
    d = Path(kernel.ctx.agents[rec["id"]]._root_path)
    assert (d / "agent.yaml").exists()
    assert not (d / "readme.md").exists() and not (d / "docs").exists()


async def test_reflect_omits_readme_by_default(seeded_kernel):
    """Plain reflect never carries `readme` — it stays lean."""
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    r = (
        await seeded_kernel.send(
            [{"target": rec["id"], "payload": {"type": "reflect"}}]
        )
    )[0]["result"]
    assert "readme" not in r


async def test_reflect_readme_attaches_content(seeded_kernel):
    """reflect with `readme: true` → reply carries the agent's
    bundle's readme.md content."""
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    r = (
        await seeded_kernel.send(
            [{"target": rec["id"], "payload": {"type": "reflect", "readme": True}}]
        )
    )[0]["result"]
    assert "readme" in r
    assert isinstance(r["readme"], str)
    assert "file" in r["readme"].lower()


async def test_reflect_readme_null_when_absent(seeded_kernel):
    """An agent whose handler ships no package (a module outside every bundle
    package) → `readme` is null (not a missing key, not a crash)."""
    agent = seeded_kernel.ctx.agents["kernel_state"]
    agent.handler_module, saved = None, agent.handler_module
    try:
        assert agent._read_readme() is None
        assert agent._readme_index() == []
    finally:
        agent.handler_module = saved


async def test_kernel_reflect_readme_is_root_readme(kernel):
    """`reflect kernel readme=true` → the root's readme, the bootstrap doc. The
    root is a `kernel_state` agent, so it is kernel_state's shipped readme."""
    r = (
        await kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "readme": True}}]
        )
    )[0]["result"]
    assert isinstance(r["readme"], str)
    assert "Fantastic kernel" in r["readme"]
    # The readme documents the reflect surface (the bootstrap doc).
    assert "reflect" in r["readme"]


async def test_root_readme_not_on_disk(kernel):
    """The bootstrap writes no `.fantastic/readme.md` or `docs/` — the root's
    manual is served from the kernel_state bundle."""
    assert not (Path(kernel._root_path) / "readme.md").exists()
    assert not (Path(kernel._root_path) / "docs").exists()
