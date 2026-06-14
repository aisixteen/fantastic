"""yaml_state — durable YAML memory agent (persists THROUGH the loader)."""

from __future__ import annotations

import pathlib


async def _mk(kernel, mode="data"):
    """A yaml_state — persists THROUGH the loader; nothing to wire. (Persistence
    tests also depend on the `store_agent` fixture so the loader has a store.)"""
    rec = await kernel.send(
        "kernel_state",
        {"type": "create_agent", "handler_module": "yaml_state.tools", "mode": mode},
    )
    return rec["id"]


async def test_set_get_roundtrip(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel)
    await seeded_kernel.send(cid, {"type": "set", "key": "user.name", "value": "Ada"})
    r = await seeded_kernel.send(cid, {"type": "read", "key": "user.name"})
    assert r["value"] == "Ada"
    # missing key → null
    assert (await seeded_kernel.send(cid, {"type": "read", "key": "nope"}))[
        "value"
    ] is None


async def test_get_whole_doc(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel)
    await seeded_kernel.send(cid, {"type": "set", "key": "a", "value": 1})
    await seeded_kernel.send(cid, {"type": "set", "key": "b", "value": "two"})
    r = await seeded_kernel.send(cid, {"type": "read"})
    assert r["doc"] == {"a": 1, "b": "two"}


async def test_keys_survey_sorted_with_sizes(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel)
    await seeded_kernel.send(cid, {"type": "set", "key": "z", "value": "hello"})
    await seeded_kernel.send(cid, {"type": "set", "key": "a", "value": [1, 2, 3]})
    r = await seeded_kernel.send(cid, {"type": "keys"})
    assert [k["key"] for k in r["keys"]] == ["a", "z"]  # sorted
    assert all(isinstance(k["size"], int) for k in r["keys"])


async def test_delete(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel)
    await seeded_kernel.send(cid, {"type": "set", "key": "k", "value": "v"})
    assert (await seeded_kernel.send(cid, {"type": "delete", "key": "k"}))[
        "deleted"
    ] is True
    assert (await seeded_kernel.send(cid, {"type": "read", "key": "k"}))[
        "value"
    ] is None
    # deleting absent key → deleted false (no error)
    assert (await seeded_kernel.send(cid, {"type": "delete", "key": "k"}))[
        "deleted"
    ] is False


async def test_replace_and_clear(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel)
    await seeded_kernel.send(cid, {"type": "set", "key": "old", "value": 1})
    await seeded_kernel.send(cid, {"type": "replace", "doc": {"new": 2}})
    assert (await seeded_kernel.send(cid, {"type": "read"}))["doc"] == {"new": 2}
    await seeded_kernel.send(cid, {"type": "replace", "doc": {}})
    assert (await seeded_kernel.send(cid, {"type": "read"}))["doc"] == {}


async def test_state_yaml(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel)
    await seeded_kernel.send(cid, {"type": "set", "key": "user.name", "value": "Ada"})
    y = (await seeded_kernel.send(cid, {"type": "state_yaml"}))["yaml"]
    assert "user.name" in y and "Ada" in y
    # empty store → empty string (inject appends nothing)
    cid2 = await _mk(seeded_kernel)
    assert (await seeded_kernel.send(cid2, {"type": "state_yaml"}))["yaml"] == ""


async def test_reflect_mode_sentence(seeded_kernel, store_agent):
    mem = await _mk(seeded_kernel, "mem")
    data = await _mk(seeded_kernel, "data")
    rmem = await seeded_kernel.send(mem, {"type": "reflect"})
    rdata = await seeded_kernel.send(data, {"type": "reflect"})
    assert rmem["mode"] == "mem" and "durable memory" in rmem["sentence"].lower()
    assert rdata["mode"] == "data" and "scratch-state" in rdata["sentence"].lower()
    assert "set" in rmem["verbs"] and "state_yaml" in rmem["verbs"]
    # No per-agent persistence wiring is surfaced — it persists through the loader.
    assert "file_bridge_id" not in rmem


async def test_no_store_failfasts_no_silent_ram(seeded_kernel):
    """No store wired at the loader ⇒ a yaml_state can't persist — `set` failfasts
    (no silent RAM). The operator/LLM wires the `.fantastic` store; the loader finds it."""
    cid = await _mk(seeded_kernel)
    r = await seeded_kernel.send(cid, {"type": "set", "key": "k", "value": "v"})
    assert "error" in r and "no store" in r["error"]
    # a read just sees an empty store (nothing to read), no crash
    assert (await seeded_kernel.send(cid, {"type": "read"}))["doc"] == {}


async def test_sealed_store_surfaces_denied_write(seeded_kernel):
    """The loader's discovered `.fantastic` store is SEALED (deny-all) — the write is
    refused and surfaced, not silently lost."""
    await seeded_kernel.send(
        seeded_kernel.id,
        # rooted at .fantastic (so the loader discovers it) but NO ingress_rule ⇒ sealed
        {
            "type": "create_agent",
            "handler_module": "file_bridge.tools",
            "root": ".fantastic",
        },
    )
    cid = await _mk(seeded_kernel, "mem")
    r = await seeded_kernel.send(cid, {"type": "set", "key": "k", "value": "v"})
    assert "error" in r
    assert r.get("reason") == "unauthorized" or "unauthorized" in r["error"]


async def test_disk_is_truth_next_to_record(seeded_kernel, store_agent):
    # The loader's `.fantastic` store (the `store_agent` fixture) is discovered; the
    # store-relative path `agents/<id>/state.yaml` lands the sidecar NEXT TO its
    # agent.json, no nest — and yaml_state wired NOTHING to make it happen.
    cid = await _mk(seeded_kernel)
    await seeded_kernel.send(cid, {"type": "set", "key": "k", "value": "v"})
    yaml_file = pathlib.Path(".fantastic") / "agents" / cid / "state.yaml"
    assert yaml_file.exists()  # next to .fantastic/agents/<cid>/agent.json, NOT nested
    assert "v" in yaml_file.read_text()
    # cascade-delete detaches the agent from the live tree (dir rmtree'd by the
    # loader on the `removed` event — covered in the kernel_state tests).
    await seeded_kernel.send("kernel_state", {"type": "delete_agent", "id": cid})
    assert seeded_kernel.get(cid) is None
    assert cid not in seeded_kernel.ctx.agents
