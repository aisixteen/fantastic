"""Per-agent readme system.

Every bundle ships a `readme.md` package resource. On create, the
substrate copies it into the agent's dir (`<agent_dir>/readme.md`).
`reflect` with `return_readme: true` attaches that file's content;
default reflect stays lean.
"""

from __future__ import annotations

from pathlib import Path


async def test_create_seeds_readme_from_bundle(kernel):
    """create_agent copies the bundle's shipped readme.md into the
    new agent's directory."""
    rec = kernel.create("file.tools")
    aid = rec["id"]
    readme = Path(kernel.ctx.agents[aid]._root_path) / "readme.md"
    assert readme.exists(), "readme.md not seeded on create"
    body = readme.read_text()
    assert "file" in body.lower()


async def test_seed_readme_copy_if_missing(kernel):
    """Re-seeding never clobbers an operator-edited readme. The copy
    only happens when the dest is absent."""
    kernel.create("file.tools", id="fa")
    readme = Path(kernel.ctx.agents["fa"]._root_path) / "readme.md"
    readme.write_text("OPERATOR EDIT")
    # Re-run the seed — should be a no-op.
    kernel.ctx.agents["fa"]._seed_readme()
    assert readme.read_text() == "OPERATOR EDIT"


async def test_seed_readme_degrades_quietly(kernel):
    """`_seed_readme` must never crash creation — a bad/absent package
    resource is swallowed. Here scheduler DOES ship one (happy path);
    the broad `except` in `_seed_readme` covers the absent case."""
    kernel.create("scheduler.tools", id="sch")
    readme = Path(kernel.ctx.agents["sch"]._root_path) / "readme.md"
    assert readme.exists()


async def test_reflect_omits_readme_by_default(seeded_kernel):
    """Plain reflect never carries `readme` — it stays lean."""
    rec = await seeded_kernel.send(
        "core", {"type": "create_agent", "handler_module": "file.tools"}
    )
    r = await seeded_kernel.send(rec["id"], {"type": "reflect"})
    assert "readme" not in r


async def test_reflect_return_readme_attaches_content(seeded_kernel):
    """reflect with `return_readme: true` → reply carries the agent's
    readme.md content."""
    rec = await seeded_kernel.send(
        "core", {"type": "create_agent", "handler_module": "file.tools"}
    )
    r = await seeded_kernel.send(rec["id"], {"type": "reflect", "return_readme": True})
    assert "readme" in r
    assert isinstance(r["readme"], str)
    assert "file" in r["readme"].lower()


async def test_reflect_return_readme_null_when_absent(seeded_kernel):
    """An agent with no readme.md on disk → `readme` is null (not a
    missing key, not a crash)."""
    rec = await seeded_kernel.send(
        "core", {"type": "create_agent", "handler_module": "file.tools"}
    )
    # Remove the seeded file to simulate a readme-less agent.
    readme = Path(seeded_kernel.ctx.agents[rec["id"]]._root_path) / "readme.md"
    readme.unlink()
    r = await seeded_kernel.send(rec["id"], {"type": "reflect", "return_readme": True})
    assert r["readme"] is None


async def test_kernel_reflect_return_readme_is_root_readme(kernel):
    """`reflect kernel` with return_readme → the root's readme
    (`.fantastic/readme.md`), the bootstrap primer."""
    r = await kernel.send("kernel", {"type": "reflect", "return_readme": True})
    assert "readme" in r
    assert isinstance(r["readme"], str)
    # Core's readme is the bootstrap primer.
    assert "Fantastic kernel" in r["readme"]
    assert "return_readme" in r["readme"]


async def test_root_readme_seeded_on_disk(kernel):
    """Core seeds `.fantastic/readme.md` on construction."""
    readme = Path(kernel._root_path) / "readme.md"
    assert readme.exists()
    assert "Fantastic kernel" in readme.read_text()
