"""`user.import_subtree` against a real kernel: export, then import (mirrors the
Rust `import_subtree` tests)."""

from __future__ import annotations

import json
import shutil
import types

import pytest
from _testkit import as_agent

from kernel import _bundles
from user import tools
from user.export import strip_runtime


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
    """A send from outside every agent: the shell."""
    return (await k.send([{"target": target, "payload": payload}]))[0]["result"]


async def call(k, agent, target, verb):
    """`agent` calls `verb` on `target`: the gate judges it."""
    with as_agent(k, agent):
        return await op(k, target, {"type": verb})


async def from_(k, agent, target):
    return await call(k, agent, target, "ping")


def meta(k, id):
    return dict(k.ctx.agents[id].record)


def parent(k, id):
    a = k.ctx.agents.get(id)
    return a.record.get("parent_id") if a is not None else None


def count(k):
    return len(k.ctx.agents)


@pytest.fixture
async def setup(kernel, monkeypatch):
    """root - user {memory, memory2 (stores), exec, other} - a {b {c}}: a and c reach
    memory, b reaches exec, memory grants a and c back."""
    for name in ("sqlite_store.tools", "exec_bridge.tools", "echo.tools"):
        monkeypatch.setitem(_bundles._UNLISTED, name, _echo_module(name))
    k = kernel
    root = k.id
    for id, par, hm, extra in [
        ("user", root, "user.tools", {}),
        (
            "memory",
            "user",
            "sqlite_store.tools",
            {"callable_by": {"a": "*", "c": "*"}},
        ),
        ("memory2", "user", "sqlite_store.tools", {}),
        ("exec", "user", "exec_bridge.tools", {"callable_by": {"b": ["run"]}}),
        ("other", "user", "echo.tools", {}),
        (
            "a",
            "user",
            "echo.tools",
            {
                "can_call": {"memory": "*", "b": "*"},
                "memory_store_id": "memory",
                "credentials": {"store": "memory", "ref": "r1"},
                "notes": "top",
            },
        ),
        (
            "b",
            "a",
            "echo.tools",
            {
                "can_call": {"exec": ["run"]},
                "callable_by": {"a": "*"},
                "cwd": ".",
            },
        ),
        ("c", "b", "echo.tools", {"can_call": {"memory": "*"}, "notes": "plain"}),
    ]:
        p = {
            "type": "create_agent",
            "parent_id": par,
            "id": id,
            "handler_module": hm,
            **extra,
        }
        r = await op(k, root, p)
        assert "error" not in r, f"{p}: {r}"
    return k


def dir_(tmp_path, tag):
    d = tmp_path / f"fu-import-{tag}"
    shutil.rmtree(d, ignore_errors=True)
    return d


async def exported(k, tmp_path, tag):
    """Export `a` into a fresh folder."""
    d = dir_(tmp_path, tag)
    r = await op(
        k, "user", {"type": "export_subtree", "agent_id": "a", "path": str(d)}
    )
    assert r["exported"] == 3, r
    assert r["ports"] == ["@exec", "@memory"], r
    return d


def written(tmp_path, tag, yaml, files=()):
    """A hand-written blueprint folder."""
    d = dir_(tmp_path, tag)
    d.mkdir(parents=True)
    (d / "blueprint.yaml").write_text(yaml)
    for f, t in files:
        p = d / f
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(t)
    return d


async def import_(k, d, **extra):
    p = {
        "type": "import_subtree",
        "path": str(d),
        "parent_id": "other",
        "bind": {"@memory": "memory2", "@exec": "exec"},
        **extra,
    }
    return await op(k, "user", p)


def renamed(m, ids):
    """`m` with every grant key and id field renamed by `ids`."""

    def r(s):
        return ids.get(s, s)

    out = dict(m)
    for g in ("can_call", "callable_by"):
        if isinstance(m.get(g), dict):
            out[g] = {r(k): v for k, v in m[g].items()}
    if isinstance(m.get("memory_store_id"), str):
        out["memory_store_id"] = r(m["memory_store_id"])
    c = m.get("credentials")
    if isinstance(c, dict) and isinstance(c.get("store"), str):
        out["credentials"] = {**c, "store": r(c["store"])}
    return out


def plain_meta(k, id):
    m = meta(k, id)
    for key in ("id", "handler_module", "parent_id"):
        m.pop(key, None)
    return strip_runtime(m)


async def test_import_without_approve_creates_nothing_and_shows_reaches(
    setup, tmp_path
):
    k = setup
    d = await exported(k, tmp_path, "preview")
    n = count(k)
    r = await import_(k, d)
    assert r["approval_required"] is True, r
    assert r["agents"] == 3, r
    assert r["new_ids"] == {"a": "a_1", "b": "b_1", "c": "c_1"}, r
    assert r["ports"] == {"@exec": "exec", "@memory": "memory2"}, r
    assert r["code"] == [], r
    reaches = r["reaches"]
    assert len(reaches) == 3
    assert reaches[1]["id"] == "b_1"
    assert reaches[1]["ports"] == {"@exec": "exec"}, r
    assert count(k) == n, "nothing created"
    assert "a_1" not in (meta(k, "memory2").get("callable_by") or {})


async def test_export_then_import_round_trips_with_fresh_ids(setup, tmp_path):
    k = setup
    d = await exported(k, tmp_path, "round")
    r = await import_(k, d, approve=True)
    assert r["imported"] == 3, r
    assert r["parent_id"] == "other"
    assert parent(k, "a_1") == "other"
    assert parent(k, "b_1") == "a_1"
    assert parent(k, "c_1") == "b_1"
    ids = {"a": "a_1", "b": "b_1", "c": "c_1", "memory": "memory2"}
    for old, new in [("a", "a_1"), ("b", "b_1"), ("c", "c_1")]:
        assert plain_meta(k, new) == renamed(plain_meta(k, old), ids), f"{old} -> {new}"
    # the bound agents granted the new ids back
    assert meta(k, "memory2")["callable_by"]["a_1"] == "*"
    assert meta(k, "memory2")["callable_by"]["c_1"] == "*"
    assert meta(k, "exec")["callable_by"]["b_1"] == ["run"]
    # the originals are as they were
    assert meta(k, "a")["can_call"]["b"] == "*"


async def test_import_twice_under_same_parent_gives_two_independent_copies(
    setup, tmp_path
):
    k = setup
    d = await exported(k, tmp_path, "twice")
    one = await import_(k, d, approve=True)
    two = await import_(k, d, approve=True)
    assert one["ids"]["a"] == "a_1", one
    assert two["ids"] == {"a": "a_2", "b": "b_2", "c": "c_2"}, two
    for id in ("a_2", "b_2", "c_2"):
        text = json.dumps(meta(k, id), default=str)
        for first in ("a_1", "b_1", "c_1"):
            assert first not in text, f"{id} names {first}: {text}"
    assert meta(k, "a_2")["can_call"]["b_2"] == "*"
    # change one copy, delete it: the other stays as it was
    r = await op(k, k.id, {"type": "update_agent", "id": "c_1", "notes": "changed"})
    assert "error" not in r, r
    assert meta(k, "c_2")["notes"] == "plain"
    r = await op(k, k.id, {"type": "delete_agent", "id": "a_1"})
    assert r["deleted"] is True, r
    for id in ("a_1", "b_1", "c_1"):
        assert parent(k, id) is None, f"{id} gone"
    assert parent(k, "c_2") == "b_2"
    assert meta(k, "memory2")["callable_by"]["a_2"] == "*"
    got = await from_(k, "c_2", "memory2")
    assert got["echo"]["type"] == "ping", "copy 2 still reaches its store"


async def test_import_refuses_missing_binding_secret_and_code_with_messages_that_name_the_fix(
    setup, tmp_path
):
    k = setup
    n = count(k)
    d = await exported(k, tmp_path, "refuse")
    r = await op(
        k,
        "user",
        {
            "type": "import_subtree",
            "path": str(d),
            "parent_id": "other",
            "bind": {"@exec": "exec"},
            "approve": True,
        },
    )
    assert r["reason"] == "unbound_ports", r
    msg = r["error"]
    assert "@memory (memory)" in msg and "pass bind" in msg, msg

    key = "sk-planted0123456789"
    s = written(
        tmp_path,
        "secret",
        f"agents:\n  - id: a\n    handler_module: echo.tools\n    api_key: {key}\n",
    )
    r = await import_(k, s, approve=True, bind={})
    assert r["reason"] == "secret_found", r
    msg = r["error"]
    assert "agent a: a api_key key" in msg and "export again" in msg, msg
    assert key not in msg, msg

    c = written(
        tmp_path,
        "code",
        "agents:\n  - id: s\n    handler_module: py_ab12.tools\n",
        [("s/bundle.yaml", "name: s\n"), ("s/tools.py", "print(1)\n")],
    )
    r = await import_(k, c, approve=True, bind={})
    assert r["reason"] == "code_not_allowed", r
    msg = r["error"]
    assert "py_ab12.tools" in msg and "s/tools.py" in msg and "allow_code: true" in msg, msg
    assert count(k) == n, "nothing created"
    # allowed, the code comes in
    r = await import_(k, c, approve=True, bind={}, allow_code=True)
    assert r["imported"] == 1, r


async def test_imported_agents_call_only_what_they_were_granted_and_what_ports_were_bound(
    setup, tmp_path
):
    k = setup
    d = await exported(k, tmp_path, "gate")
    r = await import_(k, d, approve=True)
    assert r["imported"] == 3, r
    # the bound store answers
    assert (await from_(k, "c_1", "memory2"))["echo"]["type"] == "ping"
    assert (await from_(k, "a_1", "memory2"))["echo"]["type"] == "ping"
    assert (await call(k, "b_1", "exec", "run"))["echo"]["type"] == "run"
    # b was granted only `run` on exec
    assert "error" in await from_(k, "b_1", "exec")
    # the original store was not bound: refused
    assert "error" in await from_(k, "c_1", "memory")
    # c was never granted exec
    assert "error" in await from_(k, "c_1", "exec")
    # the original agents are not reachable from the copies
    assert "error" in await from_(k, "a_1", "b")
    assert (await from_(k, "a_1", "b_1"))["echo"]["type"] == "ping"
    assert meta(k, "memory2")["callable_by"]["c_1"] == "*"


async def test_import_failure_rolls_back(setup, tmp_path):
    k = setup
    n = count(k)
    d = written(
        tmp_path,
        "rollback",
        """agents:
  - id: a
    handler_module: echo.tools
    agents:
      - id: b
        handler_module: echo.tools
        agents:
          - id: c
            handler_module: echo.tools
            can_call: {$self: 5}
""",
    )
    r = await import_(k, d, approve=True, bind={})
    assert r["reason"] == "failed", r
    assert "agent c (c_1)" in r["error"], r
    assert count(k) == n, "no half team stays"
    assert parent(k, "a_1") is None


async def test_import_grant_refusal_takes_back_edges_and_the_team(
    setup, tmp_path, monkeypatch
):
    k = setup
    n = count(k)
    d = await exported(k, tmp_path, "edges")
    real = k.ctx.edit_grants
    calls = []

    def flaky(agent_id, edit):
        calls.append(agent_id)
        if len(calls) > 2 and agent_id not in ("memory2", "exec"):
            return real(agent_id, edit)
        if len(calls) == 3:
            return False, "no way"
        return real(agent_id, edit)

    monkeypatch.setattr(k.ctx, "edit_grants", flaky)
    r = await import_(k, d, approve=True)
    assert r["reason"] == "failed", r
    assert "was refused" in r["error"] and "nothing was kept" in r["error"], r
    monkeypatch.undo()
    assert count(k) == n
    for key in ("a_1", "b_1", "c_1"):
        assert key not in (meta(k, "memory2").get("callable_by") or {})
        assert key not in (meta(k, "exec").get("callable_by") or {})


async def test_import_requires_literal_true_for_approve_and_allow_code(
    setup, tmp_path
):
    k = setup
    n = count(k)
    d = await exported(k, tmp_path, "literal")
    r = await import_(k, d, approve="true")
    assert r["approval_required"] is True, r
    assert count(k) == n
    c = written(
        tmp_path,
        "literal-code",
        "agents:\n  - id: s\n    handler_module: py_ab12.tools\n",
    )
    r = await import_(k, c, approve=True, bind={}, allow_code="true")
    assert r["reason"] == "code_not_allowed", r
    assert count(k) == n


async def test_import_subtree_args_and_docs(setup):
    assert "import_subtree" in tools.VERBS
    k = setup
    for p in [
        {"type": "import_subtree", "parent_id": "other"},
        {"type": "import_subtree", "path": "x"},
        {"type": "import_subtree", "path": "x/../y", "parent_id": "other"},
        {"type": "import_subtree", "path": "x", "parent_id": "other", "bind": []},
    ]:
        r = await op(k, "user", p)
        assert r["reason"] == "bad_arg", f"{p}: {r}"
    r = await op(
        k, "user", {"type": "import_subtree", "path": "x", "parent_id": "nobody"}
    )
    assert r["reason"] == "not_found", r
    r = await op(
        k, "user", {"type": "import_subtree", "path": "x", "parent_id": "other"}
    )
    assert r["reason"] == "not_found", r
    d = await op(k, "kernel", {"type": "docs", "of": "user"})
    assert "import_subtree" in d["verbs"], d


async def test_import_refuses_bind_to_a_missing_agent_naming_the_port(setup, tmp_path):
    k = setup
    d = await exported(k, tmp_path, "ghost")
    n = count(k)
    for extra in (
        {"approve": True, "bind": {"@memory": "ghost", "@exec": "exec"}},
        {"bind": {"@memory": "ghost", "@exec": "exec"}},
    ):
        r = await import_(k, d, **extra)
        assert r["reason"] == "bad_binding", r
        msg = r["error"]
        assert "@memory" in msg and "ghost" in msg and "no agent" in msg, msg
        assert count(k) == n, "nothing created"
    r = await import_(
        k,
        d,
        approve=True,
        bind={"@memory": "memory2", "@exec": "exec", "@nope": "exec"},
    )
    assert r["reason"] == "bad_binding", r
    assert "@nope" in r["error"], r
    assert count(k) == n, "nothing created"
