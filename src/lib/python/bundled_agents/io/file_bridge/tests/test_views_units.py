"""The pure parts of the view agents: path scoping, the view kind check, and a disk
change's kind as the disk now is. No kernel: each function is judged on its own."""

from __future__ import annotations

import os

from file_bridge import tools, views


def test_a_file_view_names_only_its_own_path():
    assert views.bridge_path("file", "a/b.md", "") == "a/b.md"
    assert views.bridge_path("file", "a/b.md", "a/b.md") == "a/b.md"
    assert views.bridge_path("file", "a/b.md", "./a/b.md") == "a/b.md"
    assert views.bridge_path("file", "a/b.md", "a/c.md") is None
    assert views.bridge_path("file", "a/b.md", "../a/b.md") is None


def test_a_folder_view_names_paths_inside_its_folder():
    assert views.bridge_path("folder", "docs", "") == "docs"
    assert views.bridge_path("folder", "docs", "x/y.md") == "docs/x/y.md"
    assert views.bridge_path("folder", "docs", "x/../y.md") == "docs/y.md"
    assert views.bridge_path("folder", "docs", "../etc") is None
    assert views.bridge_path("folder", "docs", "x/../../etc") is None
    assert views.bridge_path("folder", "docs", "/etc") is None  # absolute: refused
    assert views.bridge_path("folder", ".", "x.md") == "x.md"  # the root view


def test_scope_refuses_a_non_string_path_and_names_the_view_path():
    target, err = views.scope("read", "file", "a.md", 7)
    assert target is None and err["reason"] == "bad_arg"
    target, err = views.scope("read", "file", "a.md", "b.md")
    assert target is None and err["reason"] == "unauthorized"
    assert "outside this agent's path 'a.md'" in err["error"]


def test_relay_rule_maps_a_file_and_a_folder():
    file_rule = views._relay_rule(views.FILE, "a.md")
    assert file_rule({"path": "a.md", "kind": "added"}) == {
        "type": "changed",
        "path": "a.md",
        "kind": "added",
    }
    assert file_rule({"path": "b.md", "kind": "added"}) is None
    folder_rule = views._relay_rule(views.FOLDER, "docs")
    assert folder_rule({"path": "docs/n.md", "kind": "deleted"}) == {
        "type": "removed",
        "path": "docs/n.md",
        "name": "n.md",
    }
    assert folder_rule({"path": "docs/n.md", "kind": "modified"})["type"] == "changed"
    assert folder_rule({"path": "docs/sub/n.md", "kind": "added"}) is None  # deeper
    assert (
        folder_rule({"path": "docs", "kind": "modified"}) is None
    )  # the folder itself
    root_rule = views._relay_rule(views.FOLDER, ".")
    assert root_rule({"path": "top.md", "kind": "added"})["type"] == "added"


def test_a_change_is_judged_by_the_disk_as_it_is(tmp_path):
    gone = str(tmp_path / "gone.md")
    assert tools._kind(gone, "added") == "deleted"  # a spurious `added` after unlink
    here = tmp_path / "here.md"
    here.write_text("x")
    assert tools._kind(str(here), "added") == "added"
    assert tools._kind(str(here), "modified") == "modified"
    assert tools._kind(str(here), "deleted") == "modified"  # present now: not gone
    assert os.path.lexists(str(here))
