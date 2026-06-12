"""yaml_state — durable YAML memory agent (persists THROUGH a gated file_bridge)."""

from __future__ import annotations

import pathlib


async def _mk(kernel, store_agent, mode="data"):
    """A yaml_state wired to an OPEN file_bridge provider (the `store_agent` fixture)."""
    rec = await kernel.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "yaml_state.tools",
            "mode": mode,
            "file_bridge_id": store_agent,
        },
    )
    return rec["id"]


async def test_set_get_roundtrip(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel, store_agent)
    await seeded_kernel.send(cid, {"type": "set", "key": "user.name", "value": "Ada"})
    r = await seeded_kernel.send(cid, {"type": "read", "key": "user.name"})
    assert r["value"] == "Ada"
    # missing key → null
    assert (await seeded_kernel.send(cid, {"type": "read", "key": "nope"}))[
        "value"
    ] is None


async def test_get_whole_doc(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel, store_agent)
    await seeded_kernel.send(cid, {"type": "set", "key": "a", "value": 1})
    await seeded_kernel.send(cid, {"type": "set", "key": "b", "value": "two"})
    r = await seeded_kernel.send(cid, {"type": "read"})
    assert r["doc"] == {"a": 1, "b": "two"}


async def test_keys_survey_sorted_with_sizes(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel, store_agent)
    await seeded_kernel.send(cid, {"type": "set", "key": "z", "value": "hello"})
    await seeded_kernel.send(cid, {"type": "set", "key": "a", "value": [1, 2, 3]})
    r = await seeded_kernel.send(cid, {"type": "keys"})
    assert [k["key"] for k in r["keys"]] == ["a", "z"]  # sorted
    assert all(isinstance(k["size"], int) for k in r["keys"])


async def test_delete(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel, store_agent)
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
    cid = await _mk(seeded_kernel, store_agent)
    await seeded_kernel.send(cid, {"type": "set", "key": "old", "value": 1})
    await seeded_kernel.send(cid, {"type": "replace", "doc": {"new": 2}})
    assert (await seeded_kernel.send(cid, {"type": "read"}))["doc"] == {"new": 2}
    await seeded_kernel.send(cid, {"type": "replace", "doc": {}})
    assert (await seeded_kernel.send(cid, {"type": "read"}))["doc"] == {}


async def test_state_yaml(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel, store_agent)
    await seeded_kernel.send(cid, {"type": "set", "key": "user.name", "value": "Ada"})
    y = (await seeded_kernel.send(cid, {"type": "state_yaml"}))["yaml"]
    assert "user.name" in y and "Ada" in y
    # empty store → empty string (inject appends nothing)
    cid2 = await _mk(seeded_kernel, store_agent)
    assert (await seeded_kernel.send(cid2, {"type": "state_yaml"}))["yaml"] == ""


async def test_reflect_mode_sentence(seeded_kernel, store_agent):
    mem = await _mk(seeded_kernel, store_agent, "mem")
    data = await _mk(seeded_kernel, store_agent, "data")
    rmem = await seeded_kernel.send(mem, {"type": "reflect"})
    rdata = await seeded_kernel.send(data, {"type": "reflect"})
    assert rmem["mode"] == "mem" and "durable memory" in rmem["sentence"].lower()
    assert rdata["mode"] == "data" and "scratch-state" in rdata["sentence"].lower()
    assert "set" in rmem["verbs"] and "state_yaml" in rmem["verbs"]
    assert rmem["file_bridge_id"] == store_agent


async def test_unwired_failfasts_no_silent_ram(seeded_kernel):
    """DENY BY DEFAULT: a yaml_state with NO file_bridge_id can't persist — `set`
    failfasts (no silent RAM write). The operator/LLM must wire + open a provider."""
    rec = await seeded_kernel.send(
        "kernel_state",
        {"type": "create_agent", "handler_module": "yaml_state.tools", "mode": "mem"},
    )
    cid = rec["id"]
    r = await seeded_kernel.send(cid, {"type": "set", "key": "k", "value": "v"})
    assert "error" in r and "file_bridge_id" in r["error"]
    # a read just sees an empty store (nothing to read), no crash
    assert (await seeded_kernel.send(cid, {"type": "read"}))["doc"] == {}


async def test_sealed_provider_surfaces_denied_write(seeded_kernel):
    """A wired but SEALED provider (deny-all) refuses the write — yaml_state surfaces
    the denial rather than losing the value silently."""
    rec = await seeded_kernel.send(
        seeded_kernel.id,
        # NO ingress_rule ⇒ sealed
        {"type": "create_agent", "handler_module": "file_bridge.tools"},
    )
    sealed = rec["id"]
    cid = await _mk(seeded_kernel, sealed)
    r = await seeded_kernel.send(cid, {"type": "set", "key": "k", "value": "v"})
    assert "error" in r and ("unauthorized" in r["error"] or "refused" in r["error"])


async def test_disk_is_truth_next_to_record(seeded_kernel):
    # Wire to the `.fantastic` STORE (the single canonical provider) — store-relative
    # path `agents/<id>/state.yaml` lands the sidecar NEXT TO its agent.json, no nest.
    rec = await seeded_kernel.send(
        seeded_kernel.id,
        {
            "type": "create_agent",
            "handler_module": "file_bridge.tools",
            "root": ".fantastic",
            "ingress_rule": "allow_all",
        },
    )
    store = rec["id"]
    cid = await _mk(seeded_kernel, store)
    await seeded_kernel.send(cid, {"type": "set", "key": "k", "value": "v"})
    yaml_file = pathlib.Path(".fantastic") / "agents" / cid / "state.yaml"
    assert yaml_file.exists()  # next to .fantastic/agents/<cid>/agent.json, NOT nested
    assert "v" in yaml_file.read_text()
    # cascade-delete detaches the agent from the live tree (dir rmtree'd by the
    # loader on the `removed` event — covered in the kernel_state tests).
    await seeded_kernel.send("kernel_state", {"type": "delete_agent", "id": cid})
    assert seeded_kernel.get(cid) is None
    assert cid not in seeded_kernel.ctx.agents
