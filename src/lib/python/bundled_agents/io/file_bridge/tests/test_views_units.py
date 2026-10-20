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


class _Parent:
    id = "rest"


class _Bridge:
    """Just what `view_callers` reads of a bridge: its grants and its parent."""

    def __init__(self, callable_by, parent=_Parent()):
        self._meta = {"callable_by": callable_by}
        self.parent = parent


def test_a_view_admits_whoever_reaches_its_bridge_for_the_verbs_it_answers():
    # `*` is every verb the view answers (reflect too); a list keeps what it answers
    b = _Bridge(
        {"$self": "*", "$parent": "*", "user": ["read", "list", "edit", "pump"]}
    )
    got = views.view_callers(b, views.FILE)
    assert got == {"rest": ["read", "reflect", "watch", "write"], "user": ["read"]}
    folder = views.view_callers(b, views.FOLDER)
    assert folder == {"rest": ["list", "reflect", "reveal", "watch"], "user": ["list"]}


def test_the_bridges_own_self_is_no_caller_and_a_parent_is_named_literally():
    b = _Bridge({"$self": "*", "$parent": ["read"]}, parent=_Parent())
    assert views.view_callers(b, views.FILE) == {"rest": ["read"]}
    orphan = _Bridge({"$parent": "*"}, parent=None)
    assert views.view_callers(orphan, views.FILE) == {}, "no parent, no caller named"


def test_a_caller_with_no_verb_the_view_answers_is_not_admitted():
    b = _Bridge({"ai": ["edit", "pump"]})
    assert views.view_callers(b, views.FILE) == {}


def test_a_relay_event_without_a_known_kind_is_dropped_and_logged(caplog):
    # the bridge reports added / modified / deleted; anything else is dropped, never
    # relabelled as a change
    file_rule = views._relay_rule(views.FILE, "a.md")
    folder_rule = views._relay_rule(views.FOLDER, "docs")
    for rule, path in ((file_rule, "a.md"), (folder_rule, "docs/n.md")):
        for ev in (
            {"path": path},
            {"path": path, "kind": None},
            {"path": path, "kind": "moved"},
        ):
            with caplog.at_level("WARNING", logger="file_bridge.views"):
                assert rule(ev) is None, ev
    assert "dropped event with kind" in caplog.text
    assert "'moved'" in caplog.text


class _Ctx:
    def __init__(self):
        self.inboxes = {}


class _Kernel:
    """Just what the bridge scope reads: records by id and the inboxes."""

    def __init__(self, recs):
        self.recs = recs
        self.ctx = _Ctx()

    def get(self, id):
        return self.recs.get(id)


def test_an_empty_path_admits_nothing():
    assert views.inside("file", "", "") is False
    assert views.inside("folder", "", "x") is False
    assert views.inside("folder", ".", "x") is True  # the root is a real path


def test_a_scope_token_names_the_kind_and_the_path():
    assert views.scope_token("file", "a/b.md") == "file:a/b.md"
    assert views.scope_token("folder", ".") == "folder:."


def test_the_caller_scope_is_the_bridge_entry_for_the_caller():
    k = _Kernel(
        {
            "fb": {
                "callable_by": {
                    "fv": ["read", "file:a.md"],
                    "fd": ["list", "folder:docs"],
                    "two": ["file:a", "folder:b"],
                    "bare": ["read"],
                    "$self": "*",
                },
            },
            "fv": {"view": "file", "bridge": "fb", "path": "a.md"},
            "fd": {"view": "folder", "bridge": "fb", "path": "docs"},
            "two": {"view": "file", "bridge": "fb", "path": "a"},
            "bare": {"view": "file", "bridge": "fb", "path": "a"},
            "plain": {},
        }
    )
    assert views.caller_scope(k, "fb", "fv") == ("file", "a.md")
    assert views.caller_scope(k, "fb", "fd") == ("folder", "docs")
    # a view with two scope tokens, or none, is refused in full (an empty path)
    assert views.caller_scope(k, "fb", "two") == ("file", "")
    assert views.caller_scope(k, "fb", "bare") == ("file", "")
    # a view of ANOTHER bridge has no standing here
    k.recs["fv"]["bridge"] = "other"
    assert views.caller_scope(k, "fb", "fv") == ("file", "")
    # an operator is not path-scoped, nor is a plain agent with no token
    assert views.caller_scope(k, "fb", None) is None
    assert views.caller_scope(k, "fb", "plain") is None


def test_a_change_for_a_view_with_no_relay_inbox_is_dropped_and_logged(caplog):
    k = _Kernel({})
    with caplog.at_level("WARNING", logger="file_bridge.views"):
        assert views.deliver(k, "gone", {"type": "changed"}) is False
    assert "no relay inbox" in caplog.text
    assert "gone" not in k.ctx.inboxes


def test_deliver_puts_the_change_and_drops_the_oldest_when_full():
    import asyncio

    k = _Kernel({})
    q = asyncio.Queue(maxsize=2)
    k.ctx.inboxes[views.relay_inbox("v")] = q
    assert views.deliver(k, "v", {"n": 1}) is True
    assert views.deliver(k, "v", {"n": 2}) is True
    assert views.deliver(k, "v", {"n": 3}) is True  # full: the oldest goes
    assert [q.get_nowait(), q.get_nowait()] == [{"n": 2}, {"n": 3}]
