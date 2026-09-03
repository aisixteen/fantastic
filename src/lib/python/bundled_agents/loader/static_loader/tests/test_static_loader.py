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
    handler_module: text_file.tools
    callable_by:
      worker: [write, read]
  - id: worker
    handler_module: file_bridge.tools
    ingress_rule: allow_all_ingress
    can_call:
      memory: [write, read]
"""


async def _init_loader(kernel, manifest: str = MANIFEST) -> str:
    """Write the manifest into cwd (the kernel fixture chdir'd here) and seed a
    static_loader whose create boots it → it hydrates the roster."""
    Path("fantastic.yaml").write_text(manifest)
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "static_loader.tools",
                        "id": "static_loader",
                        "manifest": "fantastic.yaml",
                    },
                }
            ]
        )
    )[0]["result"]
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
    # …and the manifest's explicit lateral grant is present.
    assert can["memory"] == ["write", "read"]
    # memory got the baseline even with no explicit can_call in the manifest.
    mem = seeded_kernel.ctx.get("memory")["can_call"]
    assert mem["$self"] == "*"
    # NOTHING is auto-granted. `$loader` is real reach on the kernel's control
    # surface (ps/cancel/ledger/shutdown_kernel resolve through it), and only an
    # agent that keeps its OWN sidecar — an AI's chat file, the scheduler's
    # schedules — has any use for persist_blob/load_blob. A file surface or a
    # runner never persists anything of its own, so a roster-wide grant would be
    # reach handed out for nothing. A manifest that needs it declares it.
    assert "$loader" not in can and "$loader" not in mem


async def test_roster_is_closed_a_child_cannot_create_more(seeded_kernel):
    await _init_loader(seeded_kernel)
    # deny-all: a roster agent is not granted `create_agent` on anyone → the set
    # of agents is FIXED by the manifest.
    with sender_context("worker"):
        r = (
            await seeded_kernel.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": "kernel_state",
                            "handler_module": "file_bridge.tools",
                        },
                    }
                ]
            )
        )[0]["result"]
    assert r.get("reason") == "unauthorized"


async def test_worker_may_use_its_declared_edge_but_not_others(seeded_kernel):
    await _init_loader(seeded_kernel)
    with sender_context("worker"):
        ok = (
            await seeded_kernel.send(
                [
                    {
                        "target": "memory",
                        "payload": {"type": "write", "path": "k.txt", "bytes": "1"},
                    }
                ]
            )
        )[0]["result"]
        # `patch` on memory was never granted → denied.
        no = (
            await seeded_kernel.send(
                [{"target": "memory", "payload": {"type": "patch", "path": "k.txt"}}]
            )
        )[0]["result"]
    assert ok.get("reason") != "unauthorized"
    assert no.get("reason") == "unauthorized"


async def test_the_manifest_wins_over_the_hydrated_record(seeded_kernel):
    """THE MANIFEST IS TRUTH; a store is only a MEDIUM. Editing a grant in the
    yaml and rebooting must change the live agent — it used to change nothing,
    because an existing id was skipped and the hydrated record silently won."""
    loader = await _init_loader(seeded_kernel)
    assert seeded_kernel.ctx.get("worker")["can_call"]["memory"] == ["write", "read"]
    Path("fantastic.yaml").write_text(
        MANIFEST.replace("memory: [write, read]", "memory: [read]")
    )
    r = (await seeded_kernel.send([{"target": loader, "payload": {"type": "boot"}}]))[
        0
    ]["result"]
    assert r["booted"] is True, r
    assert seeded_kernel.ctx.get("worker")["can_call"]["memory"] == ["read"], (
        "the manifest must be re-applied over the hydrated record"
    )


async def test_a_manifest_that_reassigns_a_handler_module_fails_loud(seeded_kernel):
    """A bundle cannot be swapped under a live agent, so the entry is refused by
    NAME rather than silently ignored."""
    loader = await _init_loader(seeded_kernel)
    Path("fantastic.yaml").write_text(
        MANIFEST.replace(
            "  - id: memory\n    handler_module: text_file.tools",
            "  - id: memory\n    handler_module: file_bridge.tools",
        )
    )
    r = (await seeded_kernel.send([{"target": loader, "payload": {"type": "boot"}}]))[
        0
    ]["result"]
    assert r["booted"] is False, r
    assert "memory" in r["error"] and "handler_module" in r["error"], r


async def test_boot_is_idempotent_by_id(seeded_kernel):
    loader = await _init_loader(seeded_kernel)
    before = set(seeded_kernel.ctx.agents)
    # Re-boot the loader: the manifest ids already exist → re-applied, never duplicated.
    r = (await seeded_kernel.send([{"target": loader, "payload": {"type": "boot"}}]))[
        0
    ]["result"]
    assert r["booted"] is True
    assert set(r["reapplied"]) == {"memory", "worker"} and r["created"] == []
    assert set(seeded_kernel.ctx.agents) == before


async def test_missing_manifest_reports_not_crashes(seeded_kernel):
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "static_loader.tools",
                        "id": "sl2",
                        "manifest": "nope.yaml",
                    },
                }
            ]
        )
    )[0]["result"]
    # create_agent's validate_meta refuses an unreadable manifest at write time.
    assert rec.get("reason") == "invalid_meta" and "not readable" in rec.get(
        "error", ""
    )


async def test_a_roster_entry_without_an_id_is_refused_loud(seeded_kernel):
    """#105 — the roster is keyed by id, so an id-less entry can never match the
    agent the LAST boot minted: every reboot created ANOTHER one beside it and
    the flush persisted them all, with no word to the operator."""
    before = set(seeded_kernel.ctx.agents)
    loader = await _init_loader(
        seeded_kernel,
        "agents:\n  - handler_module: text_file.tools\n",
    )
    r = (await seeded_kernel.send([{"target": loader, "payload": {"type": "boot"}}]))[
        0
    ]["result"]
    assert r["booted"] is False, r
    assert "explicit id" in r["error"] and "text_file.tools" in r["error"], r
    # NOTHING was created — not on this boot, and not by the create-time boot.
    assert set(seeded_kernel.ctx.agents) == before | {loader}


async def test_an_id_less_entry_is_refused_before_the_valid_ones_hydrate(
    seeded_kernel,
):
    """The refusal is a REFUSAL, not a skip: boot stops at the bad entry."""
    loader = await _init_loader(
        seeded_kernel,
        "agents:\n"
        "  - handler_module: text_file.tools\n"
        "  - id: memory\n"
        "    handler_module: text_file.tools\n",
    )
    r = (await seeded_kernel.send([{"target": loader, "payload": {"type": "boot"}}]))[
        0
    ]["result"]
    assert r["booted"] is False, r
    assert "memory" not in seeded_kernel.ctx.agents
