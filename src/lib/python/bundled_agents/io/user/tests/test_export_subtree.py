"""`user.export_subtree` against a real kernel: the folder it writes."""

from __future__ import annotations

import os
import sys
import types

import pytest
import yaml

from kernel import _bundles
from user import export, tools

KEY = "sk-planted0123456789"
ROW = "the planted conversation row"
ABS = "/private/planted/abs/path"


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


@pytest.fixture
async def setup(kernel, monkeypatch):
    # Echo stands in for the real handlers for THIS test only: the unlisted table
    # is process-wide and wins over the catalog.
    for name in ("sqlite_store.tools", "exec_bridge.tools", "echo.tools"):
        monkeypatch.setitem(_bundles._UNLISTED, name, _echo_module(name))
    k = kernel
    root = k.id
    for id, parent, hm, extra in [
        ("user", root, "user.tools", {}),
        ("memory", "user", "sqlite_store.tools", {}),
        ("exec", "user", "exec_bridge.tools", {}),
        (
            "a",
            "user",
            "echo.tools",
            {
                "can_call": {"memory": "*", "b": "*"},
                "memory_store_id": "memory",
                "credentials": {"store": "memory", "ref": "r1", "api_key": KEY},
                "history": [ROW],
            },
        ),
        (
            "b",
            "a",
            "echo.tools",
            {
                "can_call": {"exec": ["run"]},
                "callable_by": {"a": "*"},
                "cwd": ABS,
                "rows": [{"text": ROW}],
                "last_reply": ROW,
            },
        ),
        ("c", "b", "echo.tools", {"token": KEY, "notes": "plain"}),
    ]:
        p = {
            "type": "create_agent",
            "parent_id": parent,
            "id": id,
            "handler_module": hm,
            **extra,
        }
        r = await op(k, root, p)
        assert "error" not in r, f"{p}: {r}"
    return k


def out_dir(tmp_path, tag):
    return tmp_path / f"fu-export-{tag}"


def every_file(d):
    return [os.path.join(r, f) for r, _, fs in os.walk(d) for f in fs]


def leftovers(d):
    return [p for p in os.listdir(d.parent) if p.startswith(f".{d.name}.export-")]


async def test_export_subtree_verb_writes_folder_and_reports_ports(setup, tmp_path):
    k = setup
    d = out_dir(tmp_path, "verb")
    r = await op(k, "user", {"type": "export_subtree", "agent_id": "a", "path": str(d)})
    assert r["exported"] == 3, r
    assert r["ports"] == ["@exec", "@memory", "@path:cwd"], r
    assert r["files"] == ["blueprint.yaml"], r
    v = yaml.safe_load((d / "blueprint.yaml").read_text())
    assert v["agents"][0]["id"] == "a"
    assert v["agents"][0]["agents"][0]["agents"][0]["id"] == "c"
    assert v["agents"][0]["credentials"]["ref"] == "r1"
    if sys.platform != "win32":
        assert (d.stat().st_mode & 0o777) == 0o700
        assert ((d / "blueprint.yaml").stat().st_mode & 0o777) == 0o600


async def test_secrets_scan_over_output_folder_finds_nothing(setup, tmp_path):
    k = setup
    d = out_dir(tmp_path, "scan")
    r = await op(k, "user", {"type": "export_subtree", "agent_id": "a", "path": str(d)})
    assert "error" not in r, r
    files = every_file(d)
    assert files
    hits = []
    for f in files:
        t = open(f).read()
        for planted in [KEY, ROW, ABS, "api_key", "/private"]:
            if planted in t:
                hits.append(f"{f}: {planted}")
    assert hits == []
    assert export.scan_secrets(d) == []


async def test_a_secret_in_the_subtree_is_refused_naming_only_file_and_kind(
    setup, tmp_path
):
    k = setup
    (tmp_path / "prompt.md").write_text(f"use {KEY} now")
    await op(
        k,
        "kernel",
        {"type": "update_agent", "id": "a", "system_prompt_file": "prompt.md"},
    )
    d = out_dir(tmp_path, "secret")
    r = await op(k, "user", {"type": "export_subtree", "agent_id": "a", "path": str(d)})
    assert r["reason"] == "secret_found", r
    assert "a/directive.md: an sk- key token" in r["error"], r
    assert KEY not in r["error"]
    assert not d.exists()
    assert leftovers(d) == []


async def test_export_subtree_refuses_existing_dir_and_dotdot(setup, tmp_path):
    k = setup
    d = out_dir(tmp_path, "exists")
    d.mkdir()
    (d / "keep.txt").write_text("mine")
    r = await op(k, "user", {"type": "export_subtree", "agent_id": "a", "path": str(d)})
    assert r["reason"] == "exists", r
    assert (d / "keep.txt").read_text() == "mine", "left as it was"
    r = await op(
        k, "user", {"type": "export_subtree", "agent_id": "a", "path": "x/../../y"}
    )
    assert r["reason"] == "bad_arg", r
    # an EMPTY folder is taken
    e = out_dir(tmp_path, "empty")
    e.mkdir()
    r = await op(k, "user", {"type": "export_subtree", "agent_id": "b", "path": str(e)})
    assert r["exported"] == 2, r
    # the root is refused, and nothing is written
    n = out_dir(tmp_path, "root")
    r = await op(
        k, "user", {"type": "export_subtree", "agent_id": k.id, "path": str(n)}
    )
    assert r["reason"] == "bad_arg", r
    assert not n.exists()


async def test_export_subtree_requires_agent_id_and_path(setup):
    k = setup
    for p in [
        {"type": "export_subtree", "path": "x"},
        {"type": "export_subtree", "agent_id": "a"},
        {"type": "export_subtree", "agent_id": 3, "path": "x"},
        {"type": "export_subtree", "agent_id": "a", "path": ""},
    ]:
        r = await op(k, "user", p)
        assert r["reason"] == "bad_arg", f"{p}: {r}"


async def test_export_subtree_is_in_verbs_and_docs(setup):
    assert "export_subtree" in tools.VERBS
    d = await op(setup, "kernel", {"type": "docs", "of": "user"})
    assert "export_subtree" in d["verbs"], d


async def test_export_subtree_missing_parent_folder_is_not_found(setup, tmp_path):
    d = tmp_path / "nope" / "out"
    r = await op(
        setup, "user", {"type": "export_subtree", "agent_id": "a", "path": str(d)}
    )
    assert r["reason"] == "not_found", r


async def test_export_subtree_write_failure_is_failed(setup, tmp_path, monkeypatch):
    def boom(p, text):
        raise OSError("disk full")

    monkeypatch.setattr("kernel_state.export.write_file", boom)
    d = out_dir(tmp_path, "fail")
    r = await op(
        setup, "user", {"type": "export_subtree", "agent_id": "a", "path": str(d)}
    )
    assert r["reason"] == "failed", r
    assert not d.exists()
    assert leftovers(d) == []


# ---- OWNED FILES, and the same engine on the root (#268) ----


@pytest.fixture
async def disk_setup(kernel, monkeypatch, tmp_path):
    """root - user - a (a folder-bundle agent with an html view) {b {c}}, and the
    folder `.fantastic/bundles/py_ab12/` under the running dir."""
    for name in ("py_ab12", "echo.tools"):
        monkeypatch.setitem(_bundles._UNLISTED, name, _echo_module(name))
    k = kernel
    for id, parent, hm, extra in [
        ("user", k.id, "user.tools", {}),
        (
            "a",
            "user",
            "py_ab12",
            {"view": {"kind": "html", "bytes": "<p>hi</p>", "calls": ["a"]}},
        ),
        ("b", "a", "py_ab12", {}),
        ("c", "b", "echo.tools", {}),
    ]:
        p = {"type": "create_agent", "parent_id": parent, "id": id, "handler_module": hm}
        r = await op(k, k.id, {**p, **extra})
        assert "error" not in r, f"{p}: {r}"
    b = tmp_path / ".fantastic" / "bundles" / "py_ab12"
    b.mkdir(parents=True)
    (b / "bundle.yaml").write_text("base: py_agent\nsentence: x\n")
    (b / "agent.py").write_text("print('hi')\n")
    (b / "panel.html").write_text("<p>panel</p>")
    return k


async def test_export_subtree_writes_view_and_bundle_files_for_a_folder_bundle_agent(
    disk_setup, tmp_path
):
    k = disk_setup
    r = await op(
        k, "user", {"type": "export_subtree", "agent_id": "a", "path": "out_owned"}
    )
    assert r["exported"] == 3, r
    assert r["files"] == [
        "blueprint.yaml",
        "a/view.html",
        "bundles/py_ab12/agent.py",
        "bundles/py_ab12/bundle.yaml",
        "bundles/py_ab12/panel.html",
    ], r
    d = tmp_path / "out_owned"
    assert (d / "a/view.html").read_text() == "<p>hi</p>"
    assert (d / "bundles/py_ab12/agent.py").read_text() == "print('hi')\n"
    text = (d / "blueprint.yaml").read_text()
    assert "# code: a/view.html\n" in text, text
    assert "# code: bundles/py_ab12/panel.html\n" in text, text
    v = yaml.safe_load(text)
    assert v["agents"][0]["view"]["bytes_file"] == "a/view.html"
    assert "bytes" not in v["agents"][0]["view"], text
    if sys.platform != "win32":
        for f in ["a/view.html", "bundles/py_ab12/agent.py", "bundles/py_ab12/panel.html"]:
            assert ((d / f).stat().st_mode & 0o777) == 0o600, f
        assert ((d / "bundles/py_ab12").stat().st_mode & 0o777) == 0o700


async def test_export_subtree_refuses_with_secret_found_when_agent_py_holds_a_key(
    disk_setup, tmp_path
):
    k = disk_setup
    (tmp_path / ".fantastic/bundles/py_ab12/agent.py").write_text(
        "KEY = 'sk-abcd1234efgh'\n"
    )
    r = await op(
        k, "user", {"type": "export_subtree", "agent_id": "a", "path": "out_keyed"}
    )
    assert r["reason"] == "secret_found", r
    assert "bundles/py_ab12/agent.py" in r["error"], r
    assert not (tmp_path / "out_keyed").exists()
