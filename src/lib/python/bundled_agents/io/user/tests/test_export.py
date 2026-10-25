"""The pure exporter: records in, blueprint out (mirrors the Rust export tests)."""

from __future__ import annotations

import shutil

import pytest
import yaml

from user.export import BLUEPRINT_FILE, build, scan_secrets

ROOT = "kernel_state"


def rec(id, parent, hm, meta=None):
    return {
        "id": id,
        "handler_module": hm,
        "parent_id": parent,
        "meta": dict(meta or {}),
    }


def tree(a_meta=None, b_meta=None, c_meta=None):
    """root - user {memory (sqlite_store), mem2 (ram_store), exec, files, model
    (a backend), mcp_x (unknown)} - team {a {b {c}}}"""
    return [
        rec(ROOT, None, "kernel_state.tools"),
        rec("user", ROOT, "user.tools"),
        rec("memory", "user", "sqlite_store.tools"),
        rec("mem2", "user", "ram_store.tools"),
        rec("exec", "user", "exec_bridge.tools"),
        rec("files", "user", "file_bridge.tools"),
        rec("credentials", "user", "credential_store.tools"),
        rec("model", "user", "anthropic_backend.tools"),
        rec("mcp_x", "user", "mcp_client.tools"),
        rec("a", "user", "ai_chat.tools", a_meta),
        rec("b", "a", "ai_chat.tools", b_meta),
        rec("c", "b", "noop.tools", c_meta),
    ]


def none(_id, _path):
    return None


def export(recs):
    return build(recs, "a", ROOT, None, none)


def doc(ex):
    return yaml.safe_load(ex.yaml)


def find(v, id):
    """The entry for `id`, anywhere in the nesting."""
    for e in v.get("agents") or []:
        if e["id"] == id:
            return e
        if (f := find(e, id)) is not None:
            return f
    return None


def flatten(lst, parent, out):
    """seed-style flattening: (id, parent, record minus agents)."""
    for e in lst or []:
        r = {k: v for k, v in e.items() if k != "agents"}
        out.append((e["id"], parent, r))
        flatten(e.get("agents"), e["id"], out)


def test_three_level_subtree_nests_without_parent_id():
    ex = export(tree())
    assert ex.agents == 3
    d = doc(ex)
    a = d["agents"][0]
    assert a["id"] == "a"
    assert a["agents"][0]["id"] == "b"
    assert a["agents"][0]["agents"][0]["id"] == "c"
    assert len(d["agents"]) == 1, ex.yaml
    assert "parent_id" not in ex.yaml
    assert find(d, "user") is None and find(d, ROOT) is None


def test_round_trip_reparse_gives_same_records_and_grants():
    recs = tree(
        {
            "display_name": "A",
            "can_call": {"b": "*", "$self": "*"},
            "callable_by": {"b": ["send"]},
        },
        {
            "model": "m1",
            "can_call": {"a": ["send"], "c": "*"},
            "callable_by": {"a": "*"},
        },
        {"n": 3, "callable_by": {"b": "*"}},
    )
    ex = export(recs)
    flat = []
    flatten(doc(ex)["agents"], None, flat)
    assert len(flat) == 3
    for id, parent, r in flat:
        orig = next(x for x in recs if x["id"] == id)
        want_parent = None if id == "a" else orig["parent_id"]
        assert parent == want_parent, id
        assert r["handler_module"] == orig["handler_module"], id
        want = {**orig["meta"], "id": id, "handler_module": orig["handler_module"]}
        assert r == want, f"{id}: every inside record and grant comes back"


def test_inside_grant_stays_an_id():
    ex = export(tree({"can_call": {"b": "*"}}, {"callable_by": {"a": "*"}}))
    d = doc(ex)
    assert find(d, "a")["can_call"]["b"] == "*"
    assert find(d, "b")["callable_by"]["a"] == "*"
    assert ex.ports == []


def test_outside_grant_becomes_a_port():
    ex = export(tree({"can_call": {"memory": "*"}, "memory_store_id": "memory"}))
    d = doc(ex)
    a = find(d, "a")
    assert a["can_call"]["@memory"] == "*", ex.yaml
    assert "memory" not in a["can_call"]
    assert a["memory_store_id"] == "@memory"
    assert d["ports"]["@memory"]["kind"] == "memory"
    assert d["ports"]["@memory"]["used_by"] == ["a"]
    assert "reaches memory" in d["ports"]["@memory"]["why"]


def test_outside_backend_becomes_model_port():
    ex = export(tree({"backend_id": "model", "can_call": {"model": "*"}}))
    d = doc(ex)
    assert find(d, "a")["backend_id"] == "@model"
    assert d["ports"]["@model"]["kind"] == "model"


def test_outside_exec_becomes_exec_port():
    ex = export(tree({}, {"can_call": {"exec": ["run"], "files": "*"}}))
    d = doc(ex)
    b = find(d, "b")
    assert b["can_call"]["@exec"] == ["run"]
    assert b["can_call"]["@files"] == "*"
    assert d["ports"]["@exec"]["kind"] == "exec"
    assert d["ports"]["@files"]["kind"] == "files"


def test_outside_unknown_becomes_connector_port():
    ex = export(tree({"can_call": {"mcp_x": "*", "ghost": "*"}}))
    d = doc(ex)
    a = find(d, "a")
    assert a["can_call"]["@connector:mcp_x"] == "*"
    assert a["can_call"]["@connector:ghost"] == "*", "an id nowhere in the tree"
    assert d["ports"]["@connector:mcp_x"]["kind"] == "connector"


def test_two_outside_stores_get_distinct_ports():
    # mem2 is referenced first, but `memory` > `mem2` by id: mem2 is @memory
    def go():
        return export(tree({"memory_store_id": "mem2"}, {"memory_store_id": "memory"}))

    ex = go()
    d = doc(ex)
    assert find(d, "a")["memory_store_id"] == "@memory"
    assert find(d, "b")["memory_store_id"] == "@memory_2"
    assert go().yaml == ex.yaml, "deterministic"
    assert [p.name for p in ex.ports] == ["@memory", "@memory_2"]
    assert ex.ports[0].target == "mem2"


def test_root_grant_is_written_as_at_root():
    ex = export(tree({"can_call": {ROOT: "*"}, "callable_by": {"$parent": "*"}}))
    d = doc(ex)
    a = find(d, "a")
    assert a["can_call"]["@root"] == "*"
    assert a["callable_by"]["$parent"] == "*", "baseline grants stay"
    assert ROOT not in ex.yaml
    assert ex.ports == []


def test_runtime_fields_are_dropped():
    ex = export(
        tree(
            {
                "guid": "g-123",
                "pid": 7,
                "version": 2,
                "created_at": 1,
                "updated_at": 2,
                "ts": 3,
                "seen_ts": 4,
                "last_turn": "x",
                "last_error": "e",
                "boot_error": "e",
                "rows": [1],
                "history": ["hello there"],
                "cache": {},
                "usage": {"in": 1},
                "keep": "yes",
            }
        )
    )
    a = find(doc(ex), "a")
    assert sorted(a) == ["agents", "handler_module", "id", "keep"], ex.yaml
    assert "hello there" not in ex.yaml and "g-123" not in ex.yaml


def test_credentials_keep_the_ref_but_never_a_key():
    ex = export(
        tree(
            {
                "credentials": {
                    "store": "credentials",
                    "ref": "anthropic",
                    "api_key": "sk-secret",
                },
                "api_key": "sk-secret",
                "nested": {"token": "sk-secret"},
            },
            {"credentials": {"store": "c", "ref": "r2"}},
        )
    )
    d = doc(ex)
    a = find(d, "a")
    assert a["credentials"]["ref"] == "anthropic"
    assert a["credentials"]["store"] == "@credentials"
    assert d["ports"]["@credentials"]["kind"] == "credentials"
    assert find(d, "b")["credentials"]["store"] == "c", "an inside store stays an id"
    assert "sk-secret" not in ex.yaml, ex.yaml
    assert "api_key" not in ex.yaml and "token" not in ex.yaml


def test_absolute_paths_become_relative_or_port():
    recs = tree(
        {
            "root": "/home/u/proj/.fantastic/agents/user/agents/a/data",
            "cwd": "/etc",
            "allow": ["/etc", "/opt"],
        },
        {"path": "C:\\Users\\u\\x.db"},
    )
    ex = build(recs, "a", ROOT, "/home/u/proj/.fantastic/agents/user/agents/a", none)
    d = doc(ex)
    a = find(d, "a")
    assert a["root"] == "data"
    assert a["cwd"] == "@path:cwd"
    assert a["allow"] == ["@path:allow", "@path:allow_2"]
    assert find(d, "b")["path"] == "@path:path"
    assert d["ports"]["@path:cwd"]["kind"] == "path"
    for p in ["/home/u", "/etc", "/opt", "C:\\"]:
        assert p not in ex.yaml, f"{p} leaked: {ex.yaml}"


def test_directive_file_is_exported_beside_the_blueprint():
    recs = tree(
        {"system_prompt_file": "prompt.md"}, {"system_prompt_file": "missing.md"}
    )

    def read(id, f):
        return "You are A." if (id, f) == ("a", "prompt.md") else None

    ex = build(recs, "a", ROOT, None, read)
    assert ex.files == [("a/directive.md", "You are A.")]
    d = doc(ex)
    assert find(d, "a")["system_prompt_file"] == "a/directive.md"
    assert find(d, "b")["system_prompt_file"] == "missing.md"
    assert len(ex.notes) == 1, ex.notes
    assert "# NOTE: agent b" in ex.yaml


def test_exporting_the_root_or_unknown_id_is_refused():
    recs = tree()
    with pytest.raises(ValueError):
        build(recs, ROOT, ROOT, None, none)
    with pytest.raises(ValueError):
        build(recs, "nope", ROOT, None, none)
    loose = recs + [rec("loose", None, None)]
    with pytest.raises(ValueError):  # a record in no tree
        build(loose, "loose", ROOT, None, none)


def test_the_scan_finds_keys_rows_and_absolute_paths(tmp_path):
    d = tmp_path / "scan"
    (d / "a").mkdir(parents=True)
    (d / BLUEPRINT_FILE).write_text(
        "agents:\n- id: a\n  root: /etc\n  rows: [x]\n  api_key: k\n"
    )
    (d / "a/directive.md").write_text("use sk-abcdef1234 now")
    hits = scan_secrets(d)
    assert len(hits) == 4, hits
    (d / BLUEPRINT_FILE).write_text("agents:\n- id: a\n  root: data\n")
    (d / "a/directive.md").write_text("ask-me about task-lists")
    assert scan_secrets(d) == []
    shutil.rmtree(d)
