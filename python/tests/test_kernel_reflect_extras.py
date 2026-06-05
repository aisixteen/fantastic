"""Uniform `reflect` surface — the composable flags (tree / bundles /
readme), the `description` field, and the guarantee that the old
root-only `primer` keys are GONE (transports/wire docs moved into the
root readme; `available_bundles` is now the `bundles` flag).
"""

from __future__ import annotations

_PRIMER_KEYS_GONE = (
    "transports",
    "primitive",
    "envelope",
    "universal_verb",
    "binary_protocol",
    "browser_bus",
    "well_known",
    "agent_count",
    "available_bundles",
)


async def test_reflect_root_uniform_no_primer_keys(seeded_kernel):
    """Root reflect is the uniform identity — no special primer shape."""
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    assert r["id"] == "fs_loader"
    assert r["sentence"].startswith("Fantastic kernel")
    assert r["parent_id"] is None
    for k in _PRIMER_KEYS_GONE:
        assert k not in r, f"deleted primer key {k!r} still present"


async def test_reflect_root_has_runtime(seeded_kernel):
    """Root reflect carries the kernel `runtime` id + deployment context
    (`env`, `version`) so a client that hops to this kernel can gate behavior
    from one round-trip. Non-root agents omit all three."""
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    assert r["runtime"] == "python"
    # No FANTASTIC_ENV / FANTASTIC_VERSION in a bare test process → host defaults.
    assert r["env"] == "host"
    assert "version" in r and r["version"] is None
    child = await seeded_kernel.send("cli", {"type": "reflect"})
    assert "runtime" not in child
    assert "env" not in child and "version" not in child


async def test_reflect_root_env_version_from_environ(seeded_kernel, monkeypatch):
    """`env`/`version` are read at reflect time from the optional FANTASTIC_ENV /
    FANTASTIC_VERSION envs the container bakes in — so a client learns it is
    talking to a disposable container and which build, in one round-trip."""
    monkeypatch.setenv("FANTASTIC_ENV", "container")
    monkeypatch.setenv("FANTASTIC_VERSION", "v9.9.9")
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    assert r["env"] == "container"
    assert r["version"] == "v9.9.9"


async def test_kernel_alias_equals_core(seeded_kernel):
    """`kernel` is an alias for the root; reflecting it == reflecting fs_loader."""
    via_alias = await seeded_kernel.send("kernel", {"type": "reflect"})
    via_id = await seeded_kernel.send("fs_loader", {"type": "reflect"})
    assert via_alias == via_id


# ─── tree tiers ────────────────────────────────────────────────


async def test_reflect_tree_all_default(seeded_kernel):
    """Default tree=all: nested distilled subtree rooted at the agent."""
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    tree = r["tree"]
    assert tree["id"] == "fs_loader"
    child_ids = {c["id"] for c in tree.get("children", [])}
    assert "cli" in child_ids


async def test_reflect_tree_ids(seeded_kernel):
    """tree=ids: a flat descendant-id list (self first)."""
    r = await seeded_kernel.send("kernel", {"type": "reflect", "tree": "ids"})
    assert isinstance(r["tree"], list)
    assert r["tree"][0] == "fs_loader"
    assert "cli" in r["tree"]


async def test_reflect_tree_none_omits(seeded_kernel):
    """tree=none: no tree key at all."""
    r = await seeded_kernel.send("kernel", {"type": "reflect", "tree": "none"})
    assert "tree" not in r


# ─── bundles tiers ─────────────────────────────────────────────


async def test_reflect_bundles_default_omitted(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    assert "bundles" not in r


async def test_reflect_bundles_all(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect", "bundles": "all"})
    bundles = r["bundles"]
    assert isinstance(bundles, list)
    names = [b["name"] for b in bundles]
    assert "cli" in names and "file" in names
    assert names == sorted(names)
    for b in bundles:
        assert set(b.keys()) == {"name", "handler_module"}
        assert b["handler_module"].endswith(".tools")


async def test_reflect_bundles_ids(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect", "bundles": "ids"})
    assert isinstance(r["bundles"], list)
    assert all(isinstance(n, str) for n in r["bundles"])
    assert "file" in r["bundles"]
    assert r["bundles"] == sorted(r["bundles"])


# ─── readme tier ───────────────────────────────────────────────


async def test_reflect_readme_default_omitted(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    assert "readme" not in r


async def test_reflect_readme_flag_and_legacy(seeded_kernel):
    """readme=true attaches the root readme; legacy return_readme also works."""
    new = await seeded_kernel.send("kernel", {"type": "reflect", "readme": True})
    legacy = await seeded_kernel.send(
        "kernel", {"type": "reflect", "return_readme": True}
    )
    assert isinstance(new["readme"], str)
    assert "Fantastic kernel" in new["readme"]
    assert legacy["readme"] == new["readme"]


# ─── description field ─────────────────────────────────────────


async def test_reflect_description_surfaces_top_and_tree(seeded_kernel):
    """description set at create surfaces both at top-level reflect of the
    agent AND in its node inside a tree=all walk of the parent."""
    rec = await seeded_kernel.send(
        "fs_loader",
        {
            "type": "create_agent",
            "handler_module": "file.tools",
            "description": "holds my notes",
        },
    )
    fid = rec["id"]
    own = await seeded_kernel.send(fid, {"type": "reflect"})
    assert own["description"] == "holds my notes"
    root = await seeded_kernel.send("kernel", {"type": "reflect"})
    node = next(c for c in root["tree"]["children"] if c["id"] == fid)
    assert node["description"] == "holds my notes"
