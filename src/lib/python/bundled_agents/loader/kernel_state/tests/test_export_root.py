"""`export_subtree` on the kernel ROOT (#268): the same engine `user` answers."""

from __future__ import annotations

import types

import pytest
import yaml

from kernel import _bundles
from kernel_state import tools

pytestmark = pytest.mark.usefixtures("kernel")


async def _echo(id, payload, agent):
    if payload.get("type") in ("boot", "shutdown"):
        return None
    return {"echo": payload}


def _echo_module(name):
    m = types.ModuleType(name)
    m.handler = _echo
    m.VERBS = {}
    return m


async def op(k, target, payload):
    return (await k.send([{"target": target, "payload": payload}]))[0]["result"]


async def mk(k, id, parent, hm="noop.tools", **extra):
    r = await op(
        k,
        k.id,
        {
            "type": "create_agent",
            "parent_id": parent,
            "id": id,
            "handler_module": hm,
            **extra,
        },
    )
    assert r.get("id") == id, r


@pytest.fixture
async def tree3(kernel, monkeypatch):
    monkeypatch.setitem(_bundles._UNLISTED, "noop.tools", _echo_module("noop.tools"))
    await mk(kernel, "a", kernel.id, note="a")
    await mk(kernel, "b", "a", note="b")
    await mk(kernel, "c", "b", note="c")
    return kernel


def test_root_export_subtree_is_in_the_roots_verbs_and_docs():
    assert "export_subtree" in tools.VERBS
    doc = tools.VERBS["export_subtree"].__doc__
    assert "agent_id:str (req), path:str (req)" in doc, doc
    assert "{exported, path, ports, files}" in doc, doc


async def test_root_export_subtree_exports_a_three_level_subtree(tree3, tmp_path):
    k = tree3
    r = await op(k, k.id, {"type": "export_subtree", "agent_id": "a", "path": "out_a"})
    assert r.get("exported") == 3, r
    assert r["path"] == "out_a", r
    assert r["files"] == ["blueprint.yaml"], r
    text = (tmp_path / "out_a/blueprint.yaml").read_text()
    d = yaml.safe_load(text)
    assert d["agents"][0]["id"] == "a"
    assert d["agents"][0]["agents"][0]["id"] == "b"
    assert d["agents"][0]["agents"][0]["agents"][0]["id"] == "c"
    assert "parent_id" not in text, text


async def test_root_export_subtree_refuses_the_root_id_and_unknown_id_and_bad_args(
    tree3,
):
    k = tree3
    for p in [
        {"type": "export_subtree", "agent_id": k.id, "path": "o1"},
        {"type": "export_subtree", "agent_id": "nope", "path": "o2"},
        {"type": "export_subtree", "agent_id": "a"},
        {"type": "export_subtree", "path": "o3"},
        {"type": "export_subtree", "agent_id": "a", "path": "../o4"},
    ]:
        r = await op(k, k.id, p)
        assert r["reason"] == "bad_arg", (p, r)
        assert r["error"].startswith("kernel_state.export_subtree:"), (p, r)


async def test_non_root_loader_refuses_export_subtree(kernel, tmp_path):
    await mk(kernel, "inner", kernel.id, hm="kernel_state.tools")
    r = await op(
        kernel, "inner", {"type": "export_subtree", "agent_id": "inner", "path": "o"}
    )
    assert r["reason"] == "bad_arg", r
    assert "kernel_state.export_subtree: the ROOT answers it" in r["error"], r
    assert not (tmp_path / "o").exists()


async def test_user_and_root_export_the_same_folder(tree3, tmp_path):
    k = tree3
    await mk(k, "user", k.id, hm="user.tools")
    u = await op(k, "user", {"type": "export_subtree", "agent_id": "a", "path": "by_user"})
    r = await op(k, k.id, {"type": "export_subtree", "agent_id": "a", "path": "by_root"})
    assert (u["exported"], u["files"], u["ports"]) == (
        r["exported"],
        r["files"],
        r["ports"],
    ), (u, r)
    for f in u["files"]:
        assert (tmp_path / "by_user" / f).read_bytes() == (
            tmp_path / "by_root" / f
        ).read_bytes(), f
