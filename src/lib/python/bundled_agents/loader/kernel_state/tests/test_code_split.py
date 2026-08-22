"""The loader lays a program down as its own file.

A record field that is PROGRAM TEXT round-trips through `main.py` beside
`agent.json` instead of being embedded as an escaped JSON string:

    agents/<id>/agent.json      the record
    agents/<id>/main.py         the program

Nothing about the in-memory record changes — a bundle still does
`kernel.update(id, code=...)` and still reads `record["code"]`. This is purely how
the loader lays it down, so a storeless kernel behaves exactly as before and a
program on disk is greppable, diffable and editable by anything.
"""

from __future__ import annotations

import json
import pathlib

import pytest
from kernel_state.tools import read_tree, write_record

from _testkit import boot_root

PROG = "import sys\nprint('hello from main')\n"


def _seed(tmp_path, monkeypatch) -> pathlib.Path:
    # `fs` clamps every root inside the RUNNING dir, so the test must live there.
    monkeypatch.chdir(tmp_path)
    root = tmp_path / ".fantastic"
    root.mkdir()
    write_record(root, {"id": "kernel_state", "handler_module": "kernel_state.tools"})
    return root


def test_program_lands_beside_the_record_not_inside_it(tmp_path, monkeypatch):
    d = _seed(tmp_path, monkeypatch) / "agents" / "tool"
    d.mkdir(parents=True)
    write_record(d, {"id": "tool", "handler_module": "exec_bridge.tools", "code": PROG})

    assert (d / "main.py").read_text() == PROG
    assert "code" not in json.loads((d / "agent.json").read_text())


def test_hydration_folds_the_program_back_onto_the_record(tmp_path, monkeypatch):
    root = _seed(tmp_path, monkeypatch)
    d = root / "agents" / "tool"
    d.mkdir(parents=True)
    write_record(d, {"id": "tool", "handler_module": "exec_bridge.tools", "code": PROG})

    recs = {r["id"]: r for r in read_tree(root)}
    assert recs["tool"]["code"] == PROG


def test_an_agent_without_a_program_gets_no_file_and_no_key(tmp_path, monkeypatch):
    root = _seed(tmp_path, monkeypatch)
    d = root / "agents" / "plain"
    d.mkdir(parents=True)
    write_record(d, {"id": "plain", "handler_module": "cli.tools"})

    assert not (d / "main.py").exists()
    assert "code" not in {r["id"]: r for r in read_tree(root)}["plain"]


@pytest.mark.parametrize("bad", ["", None, 123, {"not": "text"}])
def test_only_real_program_text_is_split_out(tmp_path, monkeypatch, bad):
    """Empty / absent / non-str `code` is not program text — it stays whatever it was
    and no file appears. The split must not invent one."""
    d = _seed(tmp_path, monkeypatch) / "agents" / "odd"
    d.mkdir(parents=True)
    write_record(d, {"id": "odd", "handler_module": "cli.tools", "code": bad})
    assert not (d / "main.py").exists()


async def test_the_provider_write_path_splits_too(tmp_path, monkeypatch):
    """The WARM path — records persisted through a wired store, not through `fs`.
    Both write paths must split, or a program's home depends on which one ran."""
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    await root.send(
        root.id,
        {
            "type": "create_agent",
            "handler_module": "file_bridge.tools",
            "id": "kernel_store",
            "root": ".fantastic",
            "ingress_rule": "allow_all_ingress",
        },
    )
    await root.send(
        root.id,
        {
            "type": "persist_record",
            "record": {
                "id": "warm_tool",
                "handler_module": "exec_bridge.tools",
                "parent_id": root.id,
                "code": PROG,
            },
        },
    )
    d = tmp_path / ".fantastic" / "agents" / "warm_tool"
    assert (d / "main.py").read_text() == PROG
    assert "code" not in json.loads((d / "agent.json").read_text())
