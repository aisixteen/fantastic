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


# ---- OWNED FILES BEYOND THE DIRECTIVE (#268) ----

MAX_FILE = 1 << 20


def folder_tree(view):
    recs = tree()
    for r in recs:
        if r["id"] in ("a", "b"):
            r["handler_module"] = "py_ab12"
        if r["id"] == "a":
            r["meta"]["view"] = view
    return recs


def py_files(hm):
    if hm != "py_ab12":
        return []
    return [
        ("agent.py", "print('hi')\n", None),
        ("bundle.yaml", "base: py_agent\nsentence: x\n", None),
        ("panel.html", "<p>panel</p>", None),
    ]


def export_with(recs, rb):
    return build(recs, "a", ROOT, None, none, rb)


def test_view_bytes_body_is_exported_beside_the_blueprint():
    recs = folder_tree({"kind": "html", "bytes": "<p>hi</p>", "calls": ["a"]})
    ex = export_with(recs, lambda hm: [])
    assert ("a/view.html", "<p>hi</p>") in ex.files, ex.files
    v = find(doc(ex), "a")["view"]
    assert v["bytes_file"] == "a/view.html"
    assert "bytes" not in v, ex.yaml
    assert v["calls"] == ["a"]
    assert v["kind"] == "html"


def test_folder_bundle_files_are_exported_once_per_bundle_and_marked_as_code():
    ex = export_with(folder_tree({}), py_files)
    paths = [p for p, _ in ex.files]
    for f in ["agent.py", "bundle.yaml", "panel.html"]:
        want = f"bundles/py_ab12/{f}"
        assert paths.count(want) == 1, paths
        assert want in ex.code, ex.code
        assert f"# code: {want}\n" in ex.yaml, ex.yaml
    assert len(ex.code) == 3, ex.code
    assert paths.index("bundles/py_ab12/agent.py") < paths.index(
        "bundles/py_ab12/bundle.yaml"
    )


def test_view_file_is_code_and_listed_in_code():
    ex = export_with(folder_tree({"kind": "html", "bytes": "<b>x</b>"}), lambda hm: [])
    assert ex.code == ["a/view.html"]
    assert "# code: a/view.html\n" in ex.yaml, ex.yaml


def test_unreadable_or_oversized_bundle_files_are_noted_not_written():
    big = "x" * (MAX_FILE + 1)

    def rb(hm):
        if hm != "py_ab12":
            return []
        return [
            ("agent.py", "ok", None),
            ("link.py", None, "a symlink"),
            ("blob.bin", None, "not UTF-8 text"),
            ("big.txt", big, None),
            (".env", "K=1", None),
        ]

    ex = export_with(folder_tree({}), rb)
    paths = [p for p, _ in ex.files]
    assert "bundles/py_ab12/agent.py" in paths, paths
    for bad in ["link.py", "blob.bin", "big.txt", ".env"]:
        assert not any(p.endswith(bad) for p in paths), (bad, paths)
        assert any(bad in n for n in ex.notes), (bad, ex.notes)
        assert f"# NOTE: bundle py_ab12: file {bad}" in ex.yaml, ex.yaml


def _write(d, ex):
    d.mkdir()
    (d / BLUEPRINT_FILE).write_text(ex.yaml)
    for rel, text in ex.files:
        p = d / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)


def test_owned_files_leave_the_blueprint_free_of_secrets_and_absolute_paths(tmp_path):
    ex = export_with(folder_tree({"kind": "html", "bytes": "<p>hi</p>"}), py_files)
    _write(tmp_path / "owned", ex)
    assert scan_secrets(tmp_path / "owned") == []
    assert "<p>hi</p>" not in ex.yaml, ex.yaml


def test_an_sk_token_inside_an_exported_code_file_is_found_by_the_scan(tmp_path):
    def rb(hm):
        return [("agent.py", "KEY = 'sk-abcd1234efgh'\n", None)] if hm == "py_ab12" else []

    ex = export_with(folder_tree({}), rb)
    _write(tmp_path / "sk", ex)
    hits = scan_secrets(tmp_path / "sk")
    assert len(hits) == 1, hits
    assert "bundles/py_ab12/agent.py" in hits[0], hits


def test_no_owned_files_when_the_record_has_none():
    ex = export(tree())
    assert ex.files == [] and ex.code == [], ex.files
    assert "# code:" not in ex.yaml


def test_a_secret_hit_names_the_file_with_slashes_on_every_os(tmp_path):
    d = tmp_path / "bundles" / "py_ab12"
    d.mkdir(parents=True)
    (d / "agent.py").write_text("KEY = 'sk-abcd1234efgh'\n")
    hits = scan_secrets(tmp_path)
    assert hits == ["bundles/py_ab12/agent.py: an sk- key token"], hits
    assert all("\\" not in h for h in hits), hits
