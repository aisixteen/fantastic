"""static_loader — declarative YAML roster hydration + the deny-all closure.

The loader reads a manifest and creates the declared agents AS OPERATOR at boot.
Every one carries the Unix baseline grant plus the manifest's explicit `can_call`;
after boot the roster is CLOSED — a roster agent is deny-all and cannot spawn more.
"""

from __future__ import annotations

from pathlib import Path

from kernel import sender_context

MANIFEST = """\
agents:
  - id: memory
    handler_module: yaml_state.tools
    mode: mem
  - id: worker
    handler_module: file_bridge.tools
    ingress_rule: allow_all_ingress
    can_call:
      memory: [set, read]
"""


async def _init_loader(kernel, manifest: str = MANIFEST) -> str:
    """Write the manifest into cwd (the kernel fixture chdir'd here) and seed a
    static_loader whose create boots it → it hydrates the roster."""
    Path("fantastic.yaml").write_text(manifest)
    rec = await kernel.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "static_loader.tools",
            "id": "static_loader",
            "manifest": "fantastic.yaml",
        },
    )
    assert "id" in rec, rec
    return rec["id"]


async def test_roster_is_hydrated_as_children_of_the_loader(seeded_kernel):
    loader = await _init_loader(seeded_kernel)
    agents = seeded_kernel.ctx.agents
    assert "memory" in agents and "worker" in agents
    # Declared agents are the LOADER's children ($parent = the loader).
    assert agents["memory"].record["parent_id"] == loader
    assert agents["worker"].record["parent_id"] == loader


async def test_declared_agents_carry_baseline_plus_explicit_grants(seeded_kernel):
    await _init_loader(seeded_kernel)
    can = seeded_kernel.ctx.get("worker")["can_call"]
    # Baseline is merged in…
    assert can["$self"] == "*" and can["$parent"] == "*" and can["$children"] == "*"
    assert can["$loader"] == ["persist_blob", "load_blob"]
    # …and the manifest's explicit lateral grant is present.
    assert can["memory"] == ["set", "read"]
    # memory got the baseline even with no explicit can_call in the manifest.
    mem = seeded_kernel.ctx.get("memory")["can_call"]
    assert mem["$self"] == "*" and "$loader" in mem


async def test_roster_is_closed_a_child_cannot_create_more(seeded_kernel):
    await _init_loader(seeded_kernel)
    # deny-all: a roster agent is not granted `create_agent` on anyone → the set
    # of agents is FIXED by the manifest.
    with sender_context("worker"):
        r = await seeded_kernel.send(
            "kernel_state",
            {"type": "create_agent", "handler_module": "file_bridge.tools"},
        )
    assert r.get("reason") == "unauthorized"


async def test_worker_may_use_its_declared_edge_but_not_others(seeded_kernel):
    await _init_loader(seeded_kernel)
    with sender_context("worker"):
        ok = await seeded_kernel.send("memory", {"type": "set", "key": "k", "value": 1})
        # `delete` on memory was never granted → denied.
        no = await seeded_kernel.send("memory", {"type": "delete", "key": "k"})
    assert ok.get("reason") != "unauthorized"
    assert no.get("reason") == "unauthorized"


async def test_boot_is_idempotent_by_id(seeded_kernel):
    loader = await _init_loader(seeded_kernel)
    before = set(seeded_kernel.ctx.agents)
    # Re-boot the loader: the manifest ids already exist → all skipped, no dup error.
    r = await seeded_kernel.send(loader, {"type": "boot"})
    assert r["booted"] is True
    assert set(r["skipped"]) == {"memory", "worker"} and r["created"] == []
    assert set(seeded_kernel.ctx.agents) == before


async def test_missing_manifest_reports_not_crashes(seeded_kernel):
    rec = await seeded_kernel.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "static_loader.tools",
            "id": "sl2",
            "manifest": "nope.yaml",
        },
    )
    # create_agent's validate_meta refuses an unreadable manifest at write time.
    assert rec.get("created") is False and "not readable" in rec.get("error", "")
