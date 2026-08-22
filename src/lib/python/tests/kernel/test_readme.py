"""Per-agent readme system.

Every bundle ships a `readme.md` package resource. The loader copies it
into the agent's dir (`<agent_dir>/readme.md`) when it persists the
record (copy-if-missing) — `Agent` itself never touches disk. `reflect`
with `readme: true` attaches that file's content; default reflect
stays lean.

Tests call `persist()` (a synchronous full flush via the loader) to
materialize on-disk sidecars deterministically.
"""

from __future__ import annotations

from pathlib import Path

from _testkit import persist


async def test_create_seeds_readme_from_bundle(kernel):
    """Persisting a freshly created agent copies the bundle's shipped
    readme.md into the new agent's directory."""
    rec = kernel.create("file_bridge.tools")
    aid = rec["id"]
    persist(kernel)
    readme = Path(kernel.ctx.agents[aid]._root_path) / "readme.md"
    assert readme.exists(), "readme.md not seeded on persist"
    body = readme.read_text()
    assert "file" in body.lower()


async def test_seed_readme_copy_if_missing(kernel):
    """Re-seeding never clobbers an operator-edited readme. The copy
    only happens when the dest is absent."""
    kernel.create("file_bridge.tools", id="fa")
    persist(kernel)
    readme = Path(kernel.ctx.agents["fa"]._root_path) / "readme.md"
    readme.write_text("OPERATOR EDIT")
    # Re-persist — readme seeding is copy-if-missing, so it's a no-op.
    persist(kernel)
    assert readme.read_text() == "OPERATOR EDIT"


async def test_seed_readme_degrades_quietly(kernel):
    """Readme seeding must never crash a persist — a bad/absent package
    resource is swallowed. Here scheduler DOES ship one (happy path);
    the broad `except` in `_seed_readme` covers the absent case."""
    kernel.create("scheduler.tools", id="sch")
    persist(kernel)
    readme = Path(kernel.ctx.agents["sch"]._root_path) / "readme.md"
    assert readme.exists()


async def test_reflect_omits_readme_by_default(seeded_kernel):
    """Plain reflect never carries `readme` — it stays lean."""
    rec = await seeded_kernel.send(
        "kernel_state", {"type": "create_agent", "handler_module": "file_bridge.tools"}
    )
    r = await seeded_kernel.send(rec["id"], {"type": "reflect"})
    assert "readme" not in r


async def test_reflect_readme_attaches_content(seeded_kernel):
    """reflect with `readme: true` → reply carries the agent's
    readme.md content (seeded by the loader on persist)."""
    rec = await seeded_kernel.send(
        "kernel_state", {"type": "create_agent", "handler_module": "file_bridge.tools"}
    )
    persist(seeded_kernel)
    r = await seeded_kernel.send(rec["id"], {"type": "reflect", "readme": True})
    assert "readme" in r
    assert isinstance(r["readme"], str)
    assert "file" in r["readme"].lower()


async def test_reflect_readme_null_when_absent(seeded_kernel):
    """An agent with no readme.md on disk → `readme` is null (not a
    missing key, not a crash)."""
    rec = await seeded_kernel.send(
        "kernel_state", {"type": "create_agent", "handler_module": "file_bridge.tools"}
    )
    # No persist → no readme on disk for this agent.
    r = await seeded_kernel.send(rec["id"], {"type": "reflect", "readme": True})
    assert r["readme"] is None


async def test_kernel_reflect_readme_is_root_readme(kernel):
    """`reflect kernel readme=true` → the root's readme
    (`.fantastic/readme.md`), the bootstrap doc. The root is an
    `kernel_state` agent, so the bootstrap seeds kernel_state's readme there."""
    r = await kernel.send("kernel", {"type": "reflect", "readme": True})
    assert isinstance(r["readme"], str)
    assert "Fantastic kernel" in r["readme"]
    # The readme documents the reflect surface (the bootstrap doc).
    assert "reflect" in r["readme"]


async def test_root_readme_seeded_on_disk(kernel):
    """The bootstrap seeds `.fantastic/readme.md` (the root loader's
    own readme) when it materializes the root record."""
    readme = Path(kernel._root_path) / "readme.md"
    assert readme.exists()
    assert "Fantastic kernel" in readme.read_text()
