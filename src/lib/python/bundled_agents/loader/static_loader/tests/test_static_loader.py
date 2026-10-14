"""static_loader — declarative YAML roster hydration + the deny-all closure.

The loader reads a manifest and creates the declared agents AS OPERATOR at boot.
Every one carries the Unix baseline grant plus the manifest's explicit `can_call`;
after boot the roster is CLOSED — a roster agent is deny-all and cannot spawn more.
"""

from __future__ import annotations

from pathlib import Path

from _testkit import as_agent

MANIFEST = """\
agents:
  - id: memory
    handler_module: file_bridge.tools
    allow: ['.']
    ingress_rule: allow_all_ingress
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
    assert can["$self"] == "*" and "$parent" not in can and "$subtree" not in can
    # …and the manifest's explicit lateral grant is present.
    assert can["memory"] == ["write", "read"]
    # memory got the baseline even with no explicit can_call in the manifest.
    mem = seeded_kernel.ctx.get("memory")["can_call"]
    assert mem["$self"] == "*"
    # NOTHING else is auto-granted.
    assert set(mem) == {"$self"}


async def test_roster_is_closed_a_child_cannot_create_more(seeded_kernel):
    await _init_loader(seeded_kernel)
    # deny-all: a roster agent is not granted `create_agent` on anyone → the set
    # of agents is FIXED by the manifest.
    with as_agent(seeded_kernel, "worker"):
        r = (
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
    assert r.get("reason") == "unauthorized"


async def test_worker_may_use_its_declared_edge_but_not_others(seeded_kernel):
    await _init_loader(seeded_kernel)
    with as_agent(seeded_kernel, "worker"):
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
        # `edit` on memory was never granted → denied.
        no = (
            await seeded_kernel.send(
                [{"target": "memory", "payload": {"type": "edit", "path": "k.txt"}}]
            )
        )[0]["result"]
    assert ok.get("reason") != "unauthorized"
    assert no.get("reason") == "unauthorized"


async def test_the_manifest_wins_over_the_hydrated_record(seeded_kernel):
    """THE MANIFEST IS TRUTH; a store is only a MEDIUM. Editing a grant in the
    yaml and rebooting must change the live agent — never skip an existing id and
    let the hydrated record silently win."""
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
            "  - id: memory\n    handler_module: file_bridge.tools",
            "  - id: memory\n    handler_module: scheduler.tools",
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
    # validate_meta checks the SHAPE only, so the record is written; the READ is
    # the boot's, against the kernel's running dir, and it reports — not raises.
    assert rec.get("id") == "sl2", rec
    r = (await seeded_kernel.send([{"target": "sl2", "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    assert r["booted"] is False, r
    assert r["error"].startswith("static_loader: cannot read manifest 'nope.yaml': "), r


async def test_a_malformed_manifest_field_is_refused_at_write_time(seeded_kernel):
    for bad in ("", 5):
        rec = (
            await seeded_kernel.send(
                [
                    {
                        "target": "kernel_state",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": "kernel_state",
                            "handler_module": "static_loader.tools",
                            "id": "sl3",
                            "manifest": bad,
                        },
                    }
                ]
            )
        )[0]["result"]
        assert rec.get("reason") == "invalid_meta", rec
        assert rec["error"] == (
            "static_loader: manifest must be a non-empty path string"
        ), rec
        assert "sl3" not in seeded_kernel.ctx.agents


async def test_a_roster_entry_without_an_id_is_refused_loud(seeded_kernel):
    """#105 — the roster is keyed by id, so an id-less entry can never match the
    agent the LAST boot minted: every reboot created ANOTHER one beside it and
    the flush persisted them all, with no word to the operator."""
    before = set(seeded_kernel.ctx.agents)
    loader = await _init_loader(
        seeded_kernel,
        "agents:\n  - handler_module: file_bridge.tools\n",
    )
    r = (await seeded_kernel.send([{"target": loader, "payload": {"type": "boot"}}]))[
        0
    ]["result"]
    assert r["booted"] is False, r
    assert "explicit id" in r["error"] and "file_bridge.tools" in r["error"], r
    # NOTHING was created — not on this boot, and not by the create-time boot.
    assert set(seeded_kernel.ctx.agents) == before | {loader}


async def test_an_id_less_entry_is_refused_before_the_valid_ones_hydrate(
    seeded_kernel,
):
    """The refusal is a REFUSAL, not a skip: boot stops at the bad entry."""
    loader = await _init_loader(
        seeded_kernel,
        "agents:\n"
        "  - handler_module: file_bridge.tools\n"
        "  - id: memory\n"
        "    handler_module: file_bridge.tools\n",
    )
    r = (await seeded_kernel.send([{"target": loader, "payload": {"type": "boot"}}]))[
        0
    ]["result"]
    assert r["booted"] is False, r
    assert "memory" not in seeded_kernel.ctx.agents


# ─── each boot answer, pinned (reduction candidate J: its single-item sends) ─


async def _boot_with(kernel, aid: str, manifest_text: str | None, **meta) -> dict:
    if manifest_text is not None:
        Path(f"{aid}.yaml").write_text(manifest_text)
        meta.setdefault("manifest", f"{aid}.yaml")
    kernel.create("static_loader.tools", id=aid, **meta)
    return (await kernel.send([{"target": aid, "payload": {"type": "boot"}}]))[0][
        "result"
    ]


async def test_each_manifest_fault_has_its_own_text(seeded_kernel):
    k = seeded_kernel
    assert await _boot_with(k, "sl_none", None) == {
        "error": "static_loader: no 'manifest' path on the record",
        "booted": False,
    }
    r = await _boot_with(k, "sl_yaml", "agents: [unclosed\n")
    assert r["booted"] is False and r["error"].startswith(
        "static_loader: manifest 'sl_yaml.yaml' is not valid YAML: "
    ), r
    assert await _boot_with(k, "sl_list", "- a\n- b\n") == {
        "error": "static_loader: manifest 'sl_list.yaml' must be a mapping with `agents:`",
        "booted": False,
    }
    assert await _boot_with(k, "sl_map", "agents: {a: 1}\n") == {
        "error": "static_loader: `agents:` in 'sl_map.yaml' must be a list",
        "booted": False,
    }
    assert await _boot_with(k, "sl_hm", "agents:\n  - id: x\n") == {
        "error": "static_loader: each roster entry needs a handler_module: {'id': 'x'}",
        "booted": False,
    }
    # an empty manifest hydrates nothing, successfully
    assert await _boot_with(k, "sl_empty", "") == {
        "booted": True,
        "created": [],
        "reapplied": [],
    }


async def test_a_refused_create_and_a_refused_reapply_are_named(seeded_kernel):
    k = seeded_kernel
    bad = "agents:\n  - id: 'a/b'\n    handler_module: file_bridge.tools\n"
    r = await _boot_with(k, "sl_bad", bad)
    assert r["booted"] is False
    assert r["error"].startswith(
        "static_loader: create failed for {'id': 'a/b', 'handler_module': "
        "'file_bridge.tools'}: {'error': \"create_agent: id must be a single path "
        "segment (no separators); got 'a/b'\""
    ), r
    ok = "agents:\n  - id: fb_r\n    handler_module: file_bridge.tools\n    allow: ['.']\n"
    assert await _boot_with(k, "sl_ok", ok) == {
        "booted": True,
        "created": ["fb_r"],
        "reapplied": [],
    }
    # the same id re-applied with a field its bundle refuses
    Path("sl_ok.yaml").write_text(
        "agents:\n  - id: fb_r\n    handler_module: file_bridge.tools\n"
        "    allow: '../escape'\n"
    )
    r = (await k.send([{"target": "sl_ok", "payload": {"type": "boot"}}]))[0]["result"]
    assert r["booted"] is False and r["error"].startswith(
        "static_loader: re-applying 'fb_r': "
    ), r
    Path("sl_ok.yaml").write_text(ok)
    r = (await k.send([{"target": "sl_ok", "payload": {"type": "boot"}}]))[0]["result"]
    assert r == {"booted": True, "created": [], "reapplied": ["fb_r"]}


# ─── who may boot: the kernel's own boot only ────────────────────────────────


async def test_an_agent_sent_boot_is_refused_and_re_runs_nothing(seeded_kernel):
    """The roster pass runs AS OPERATOR, so only the kernel's own boot (origin
    none) may start it: otherwise a roster agent whose grant the operator revoked
    could send `boot` to its loader and get the grant back, and a deleted roster
    agent would come back. The edge is NAMED here on both ends, so the
    refusal is the loader's own, whatever the gate makes of `boot`."""
    k = seeded_kernel
    manifest = MANIFEST.replace(
        "      memory: [write, read]\n",
        "      memory: [write, read]\n      static_loader: [boot]\n",
    )
    Path("fantastic.yaml").write_text(manifest)
    rec = (
        await k.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "static_loader.tools",
                        "id": "static_loader",
                        "manifest": "fantastic.yaml",
                        "callable_by": {"worker": ["boot"]},
                    },
                }
            ]
        )
    )[0]["result"]
    assert rec.get("id") == "static_loader", rec
    # The operator revokes worker's lateral edge and deletes `memory`.
    for p in (
        {
            "type": "update_agent",
            "id": "worker",
            "can_call": {
                "$self": "*",
                "$parent": "*",
                "$subtree": "*",
                "static_loader": ["boot"],
            },
        },
        {"type": "delete_agent", "id": "memory"},
    ):
        r = (await k.send([{"target": "kernel", "payload": p}]))[0]["result"]
        assert not r.get("error"), r
    with as_agent(seeded_kernel, "worker"):
        r = (await k.send([{"target": "static_loader", "payload": {"type": "boot"}}]))[
            0
        ]["result"]
    assert r == {
        "error": "static_loader: an agent-sent boot is refused — hydrating the "
        "roster is the kernel's own bookkeeping, so only the kernel's own boot "
        "(cold boot, create_agent) runs it",
        "reason": "unauthorized",
        "hint": "change an agent's record with update_agent (its parent writes "
        "it); the operator re-boots the loader",
    }
    assert "memory" not in k.ctx.agents
    assert "memory" not in k.ctx.get("worker")["can_call"]
    # The operator's boot is the kernel's own: it re-runs the manifest.
    r = (await k.send([{"target": "static_loader", "payload": {"type": "boot"}}]))[0][
        "result"
    ]
    assert r == {"booted": True, "created": ["memory"], "reapplied": ["worker"]}, r
