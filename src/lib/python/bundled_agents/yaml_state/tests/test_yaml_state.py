"""yaml_state — durable YAML memory agent (persists THROUGH the loader)."""

from __future__ import annotations

import pathlib


async def _mk(kernel, mode="data"):
    """A yaml_state — persists THROUGH the loader; nothing to wire. (Persistence
    tests also depend on the `store_agent` fixture so the loader has a store.)"""
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "yaml_state.tools",
                        "mode": mode,
                    },
                }
            ]
        )
    )[0]["result"]
    return rec["id"]


async def test_set_get_roundtrip(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel)
    (
        await seeded_kernel.send(
            [
                {
                    "target": cid,
                    "payload": {"type": "set", "key": "user.name", "value": "Ada"},
                }
            ]
        )
    )[0]["result"]
    r = (
        await seeded_kernel.send(
            [{"target": cid, "payload": {"type": "read", "key": "user.name"}}]
        )
    )[0]["result"]
    assert r["value"] == "Ada"
    # missing key → null
    assert (
        (
            (
                await seeded_kernel.send(
                    [{"target": cid, "payload": {"type": "read", "key": "nope"}}]
                )
            )[0]["result"]
        )["value"]
        is None
    )


async def test_get_whole_doc(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel)
    (
        await seeded_kernel.send(
            [{"target": cid, "payload": {"type": "set", "key": "a", "value": 1}}]
        )
    )[0]["result"]
    (
        await seeded_kernel.send(
            [{"target": cid, "payload": {"type": "set", "key": "b", "value": "two"}}]
        )
    )[0]["result"]
    r = (await seeded_kernel.send([{"target": cid, "payload": {"type": "read"}}]))[0][
        "result"
    ]
    assert r["doc"] == {"a": 1, "b": "two"}


async def test_keys_survey_sorted_with_sizes(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel)
    (
        await seeded_kernel.send(
            [{"target": cid, "payload": {"type": "set", "key": "z", "value": "hello"}}]
        )
    )[0]["result"]
    (
        await seeded_kernel.send(
            [
                {
                    "target": cid,
                    "payload": {"type": "set", "key": "a", "value": [1, 2, 3]},
                }
            ]
        )
    )[0]["result"]
    r = (await seeded_kernel.send([{"target": cid, "payload": {"type": "keys"}}]))[0][
        "result"
    ]
    assert [k["key"] for k in r["keys"]] == ["a", "z"]  # sorted
    assert all(isinstance(k["size"], int) for k in r["keys"])


async def test_delete(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel)
    (
        await seeded_kernel.send(
            [{"target": cid, "payload": {"type": "set", "key": "k", "value": "v"}}]
        )
    )[0]["result"]
    assert (
        (
            (
                await seeded_kernel.send(
                    [{"target": cid, "payload": {"type": "delete", "key": "k"}}]
                )
            )[0]["result"]
        )["deleted"]
        is True
    )
    assert (
        (
            (
                await seeded_kernel.send(
                    [{"target": cid, "payload": {"type": "read", "key": "k"}}]
                )
            )[0]["result"]
        )["value"]
        is None
    )
    # deleting absent key → deleted false (no error)
    assert (
        (
            (
                await seeded_kernel.send(
                    [{"target": cid, "payload": {"type": "delete", "key": "k"}}]
                )
            )[0]["result"]
        )["deleted"]
        is False
    )


async def test_replace_and_clear(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel)
    (
        await seeded_kernel.send(
            [{"target": cid, "payload": {"type": "set", "key": "old", "value": 1}}]
        )
    )[0]["result"]
    (
        await seeded_kernel.send(
            [{"target": cid, "payload": {"type": "replace", "doc": {"new": 2}}}]
        )
    )[0]["result"]
    assert (
        (await seeded_kernel.send([{"target": cid, "payload": {"type": "read"}}]))[0][
            "result"
        ]
    )["doc"] == {"new": 2}
    (
        await seeded_kernel.send(
            [{"target": cid, "payload": {"type": "replace", "doc": {}}}]
        )
    )[0]["result"]
    assert (
        (
            (await seeded_kernel.send([{"target": cid, "payload": {"type": "read"}}]))[
                0
            ]["result"]
        )["doc"]
        == {}
    )


async def test_state_yaml(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel)
    (
        await seeded_kernel.send(
            [
                {
                    "target": cid,
                    "payload": {"type": "set", "key": "user.name", "value": "Ada"},
                }
            ]
        )
    )[0]["result"]
    y = (
        (
            await seeded_kernel.send(
                [{"target": cid, "payload": {"type": "state_yaml"}}]
            )
        )[0]["result"]
    )["yaml"]
    assert "user.name" in y and "Ada" in y
    # empty store → empty string (inject appends nothing)
    cid2 = await _mk(seeded_kernel)
    assert (
        (
            (
                await seeded_kernel.send(
                    [{"target": cid2, "payload": {"type": "state_yaml"}}]
                )
            )[0]["result"]
        )["yaml"]
        == ""
    )


async def test_reflect_mode_sentence(seeded_kernel, store_agent):
    mem = await _mk(seeded_kernel, "mem")
    data = await _mk(seeded_kernel, "data")
    rmem = (
        await seeded_kernel.send([{"target": mem, "payload": {"type": "reflect"}}])
    )[0]["result"]
    rdata = (
        await seeded_kernel.send([{"target": data, "payload": {"type": "reflect"}}])
    )[0]["result"]
    assert rmem["mode"] == "mem" and "durable memory" in rmem["sentence"].lower()
    assert rdata["mode"] == "data" and "scratch-state" in rdata["sentence"].lower()
    assert "set" in rmem["verbs"] and "state_yaml" in rmem["verbs"]
    # No per-agent persistence wiring is surfaced — it persists through the loader.
    assert "file_bridge_id" not in rmem


async def test_no_store_failfasts_no_silent_ram(seeded_kernel):
    """No store wired at the loader ⇒ a yaml_state can't persist — `set` failfasts
    (no silent RAM). The operator/LLM wires the `.fantastic` store; the loader finds it."""
    cid = await _mk(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [{"target": cid, "payload": {"type": "set", "key": "k", "value": "v"}}]
        )
    )[0]["result"]
    assert "error" in r and "no store" in r["error"]
    # a read just sees an empty store (nothing to read), no crash
    assert (
        (
            (await seeded_kernel.send([{"target": cid, "payload": {"type": "read"}}]))[
                0
            ]["result"]
        )["doc"]
        == {}
    )


async def test_sealed_store_surfaces_denied_write(seeded_kernel):
    """The loader's discovered `.fantastic` store is SEALED (deny-all) — the write is
    refused and surfaced, not silently lost."""
    (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload":  # rooted at .fantastic (so the loader discovers it) but NO ingress_rule ⇒ sealed
                    {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "handler_module": "file_bridge.tools",
                        "root": ".fantastic",
                    },
                }
            ]
        )
    )[0]["result"]
    cid = await _mk(seeded_kernel, "mem")
    r = (
        await seeded_kernel.send(
            [{"target": cid, "payload": {"type": "set", "key": "k", "value": "v"}}]
        )
    )[0]["result"]
    assert "error" in r
    assert r.get("reason") == "unauthorized" or "unauthorized" in r["error"]


async def test_disk_is_truth_next_to_record(seeded_kernel, store_agent):
    # The loader's `.fantastic` store (the `store_agent` fixture) is discovered; the
    # store-relative path `agents/<id>/state.yaml` lands the sidecar NEXT TO its
    # agent.json, no nest — and yaml_state wired NOTHING to make it happen.
    cid = await _mk(seeded_kernel)
    (
        await seeded_kernel.send(
            [{"target": cid, "payload": {"type": "set", "key": "k", "value": "v"}}]
        )
    )[0]["result"]
    yaml_file = pathlib.Path(".fantastic") / "agents" / cid / "state.yaml"
    assert yaml_file.exists()  # next to .fantastic/agents/<cid>/agent.json, NOT nested
    assert "v" in yaml_file.read_text()
    # cascade-delete detaches the agent from the live tree (dir rmtree'd by the
    # loader on the `removed` event — covered in the kernel_state tests).
    (
        await seeded_kernel.send(
            [{"target": "kernel_state", "payload": {"type": "delete_agent", "id": cid}}]
        )
    )[0]["result"]
    assert seeded_kernel.get(cid) is None
    assert cid not in seeded_kernel.ctx.agents


async def test_append_extends_list(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel)
    (
        await seeded_kernel.send(
            [
                {
                    "target": cid,
                    "payload": {
                        "type": "append",
                        "key": "log",
                        "value": {"role": "user", "content": "a"},
                    },
                }
            ]
        )
    )[0]["result"]
    # a LIST value extends (batch in one call)
    (
        await seeded_kernel.send(
            [
                {
                    "target": cid,
                    "payload": {
                        "type": "append",
                        "key": "log",
                        "value": [
                            {"role": "assistant", "content": "b"},
                            {"role": "user", "content": "c"},
                        ],
                    },
                }
            ]
        )
    )[0]["result"]
    r = (
        await seeded_kernel.send(
            [{"target": cid, "payload": {"type": "read", "key": "log"}}]
        )
    )[0]["result"]
    assert [t["content"] for t in r["value"]] == ["a", "b", "c"]


async def test_chat_mode_reflect(seeded_kernel, store_agent):
    cid = await _mk(seeded_kernel, "chat")
    r = (await seeded_kernel.send([{"target": cid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert r["mode"] == "chat" and "append" in r["sentence"].lower()
