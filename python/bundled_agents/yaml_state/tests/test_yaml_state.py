"""yaml_state — durable YAML memory agent."""

from __future__ import annotations

import pathlib


async def _mk(kernel, mode="data"):
    rec = await kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "yaml_state.tools", "mode": mode},
    )
    return rec["id"]


async def test_set_get_roundtrip(kernel):
    cid = await _mk(kernel)
    await kernel.send(cid, {"type": "set", "key": "user.name", "value": "Ada"})
    r = await kernel.send(cid, {"type": "read", "key": "user.name"})
    assert r["value"] == "Ada"
    # missing key → null
    assert (await kernel.send(cid, {"type": "read", "key": "nope"}))["value"] is None


async def test_get_whole_doc(kernel):
    cid = await _mk(kernel)
    await kernel.send(cid, {"type": "set", "key": "a", "value": 1})
    await kernel.send(cid, {"type": "set", "key": "b", "value": "two"})
    r = await kernel.send(cid, {"type": "read"})
    assert r["doc"] == {"a": 1, "b": "two"}


async def test_keys_survey_sorted_with_sizes(kernel):
    cid = await _mk(kernel)
    await kernel.send(cid, {"type": "set", "key": "z", "value": "hello"})
    await kernel.send(cid, {"type": "set", "key": "a", "value": [1, 2, 3]})
    r = await kernel.send(cid, {"type": "keys"})
    assert [k["key"] for k in r["keys"]] == ["a", "z"]  # sorted
    assert all(isinstance(k["size"], int) for k in r["keys"])


async def test_delete(kernel):
    cid = await _mk(kernel)
    await kernel.send(cid, {"type": "set", "key": "k", "value": "v"})
    assert (await kernel.send(cid, {"type": "delete", "key": "k"}))["deleted"] is True
    assert (await kernel.send(cid, {"type": "read", "key": "k"}))["value"] is None
    # deleting absent key → deleted false (no error)
    assert (await kernel.send(cid, {"type": "delete", "key": "k"}))["deleted"] is False


async def test_replace_and_clear(kernel):
    cid = await _mk(kernel)
    await kernel.send(cid, {"type": "set", "key": "old", "value": 1})
    await kernel.send(cid, {"type": "replace", "doc": {"new": 2}})
    assert (await kernel.send(cid, {"type": "read"}))["doc"] == {"new": 2}
    await kernel.send(cid, {"type": "replace", "doc": {}})
    assert (await kernel.send(cid, {"type": "read"}))["doc"] == {}


async def test_state_yaml(kernel):
    cid = await _mk(kernel)
    await kernel.send(cid, {"type": "set", "key": "user.name", "value": "Ada"})
    y = (await kernel.send(cid, {"type": "state_yaml"}))["yaml"]
    assert "user.name" in y and "Ada" in y
    # empty store → empty string (inject appends nothing)
    cid2 = await _mk(kernel)
    assert (await kernel.send(cid2, {"type": "state_yaml"}))["yaml"] == ""


async def test_reflect_mode_sentence(kernel):
    mem = await _mk(kernel, "mem")
    data = await _mk(kernel, "data")
    rmem = await kernel.send(mem, {"type": "reflect"})
    rdata = await kernel.send(data, {"type": "reflect"})
    assert rmem["mode"] == "mem" and "durable memory" in rmem["sentence"].lower()
    assert rdata["mode"] == "data" and "scratch-state" in rdata["sentence"].lower()
    assert "set" in rmem["verbs"] and "state_yaml" in rmem["verbs"]


async def test_disk_is_truth_and_cascade_delete(kernel):
    cid = await _mk(kernel)
    await kernel.send(cid, {"type": "set", "key": "k", "value": "v"})
    agent = kernel.ctx.agents[cid]
    yaml_file = pathlib.Path(agent._root_path) / "state.yaml"
    assert yaml_file.exists()
    # the on-disk YAML is the truth — readable + contains the value
    assert "v" in yaml_file.read_text()
    # cascade-delete removes the agent dir (and its state.yaml) for free
    await kernel.send("core", {"type": "delete_agent", "id": cid})
    assert not yaml_file.exists()
