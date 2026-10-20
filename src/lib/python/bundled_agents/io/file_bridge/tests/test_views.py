"""The view agents over a file_bridge: `file` and `folder` (`file_bridge/views.py`).

A view reaches its bridge AS ITSELF, for its own path only, with the verbs its grant
names; one view per path (`open_view` reuses it); a view's watch relays the bridge's
`changed` events onto its own inbox, filtered to its path."""

from __future__ import annotations

import asyncio
from pathlib import Path


async def _call(k, target, payload):
    return (await k.send([{"target": target, "payload": payload}]))[0]["result"]


async def _open(k, fb, kind, path):
    return await _call(k, fb, {"type": "open_view", "kind": kind, "path": path})


def _inbox(k, src: str, watcher: str = "_t_view"):
    q = k.ctx.ensure_inbox(watcher)
    k.watch(src, watcher)
    return q


async def _next(q, pred, timeout: float = 5.0) -> dict:
    """The next event on `q` matching `pred` (others skipped)."""
    end = asyncio.get_event_loop().time() + timeout
    while True:
        left = end - asyncio.get_event_loop().time()
        assert left > 0, "no matching event within the timeout"
        ev = await asyncio.wait_for(q.get(), left)
        if pred(ev):
            return ev


async def test_open_view_makes_a_file_view_and_reuses_it(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    Path("notes.md").write_text("hi\n")
    first = await _open(k, fb, "file", "notes.md")
    assert first["created"] is True and first["kind"] == "file", first
    again = await _open(k, fb, "file", "notes.md")
    assert again == {**first, "created": False}, again  # one agent per path
    rec = k.get(first["id"])
    assert rec["handler_module"] == "file_bridge.tools" and rec["view"] == "file"
    assert rec["bridge"] == fb and rec["path"] == "notes.md"
    assert rec["parent_id"] == fb  # the view is the bridge's child
    # its grant on the bridge: its own verbs only, and the bridge admits exactly those
    assert rec["can_call"][fb] == [
        "read",
        "write",
        "watch_path",
        "unwatch_path",
        "watch",
    ]
    assert k.get(fb)["callable_by"][first["id"]] == [
        "read",
        "unwatch_path",
        "watch",
        "watch_path",
        "write",
    ]


async def test_a_folder_view_is_one_per_folder(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    Path("docs").mkdir()
    a = await _open(k, fb, "folder", "docs")
    b = await _open(k, fb, "folder", "docs")
    assert a["kind"] == "folder" and a["created"] is True
    assert b["id"] == a["id"] and b["created"] is False
    assert k.get(a["id"])["view"] == "folder"


async def test_open_view_refuses_what_the_bridge_does_not_expose(seeded_kernel):
    k = seeded_kernel
    Path("docs").mkdir()
    Path("secret").mkdir()
    fb = (
        await k.send(
            [
                {
                    "target": k.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": k.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["docs"],
                        "ingress_rule": "allow_all_ingress",
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    assert (await _open(k, fb, "folder", "docs"))["created"] is True
    r = await _open(k, fb, "folder", "secret")
    assert r["reason"] == "not_found", r
    assert (await _open(k, fb, "weird", "docs"))["reason"] == "bad_arg"
    assert (await _open(k, fb, "file", "."))["reason"] == "bad_arg"


async def test_a_file_view_reads_and_writes_its_own_path(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    Path("a.txt").write_text("one\n")
    v = (await _open(k, fb, "file", "a.txt"))["id"]
    r = await _call(k, v, {"type": "read"})
    assert [ln["text"] for ln in r["lines"]] == ["one"], r
    w = await _call(k, v, {"type": "write", "bytes": "two\n", "if_match": r["version"]})
    assert w["version"], w
    assert Path("a.txt").read_text() == "two\n"
    stale = await _call(k, v, {"type": "write", "bytes": "x", "if_match": r["version"]})
    assert stale["reason"] == "version_mismatch", stale


async def test_a_file_view_refuses_another_path(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    Path("a.txt").write_text("one\n")
    Path("b.txt").write_text("two\n")
    v = (await _open(k, fb, "file", "a.txt"))["id"]
    r = await _call(k, v, {"type": "read", "path": "b.txt"})
    assert r["reason"] == "unauthorized" and "outside" in r["error"], r
    assert (await _call(k, v, {"type": "read", "path": "a.txt"}))["lines"]
    w = await _call(k, v, {"type": "write", "path": "b.txt", "bytes": "x"})
    assert w["reason"] == "unauthorized", w
    assert Path("b.txt").read_text() == "two\n"


async def test_a_view_has_only_its_grant_on_the_bridge(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    Path("a.txt").write_text("one\n")
    v = (await _open(k, fb, "file", "a.txt"))["id"]
    # a verb outside the view's own set is unhandled by the view, never forwarded to
    # the bridge (the bridge's delete and mkdir are not in the view's grant)
    for verb in ("delete", "mkdir", "rename"):
        r = await _call(
            k,
            v,
            {"type": "delete", "path": "a.txt"}
            if verb == "delete"
            else {"type": verb, "path": "a.txt", "new_path": "b.txt"},
        )
        assert r["reason"] == "unhandled", (verb, r)
    assert Path("a.txt").exists()
    assert (
        "watch_path" in k.get(v)["can_call"][fb]
        and "delete" not in k.get(v)["can_call"][fb]
    )


async def test_a_folder_view_lists_inside_its_folder_only(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    Path("docs").mkdir()
    Path("docs/a.md").write_text("a")
    Path("other.md").write_text("x")
    v = (await _open(k, fb, "folder", "docs"))["id"]
    listed = await _call(k, v, {"type": "list"})
    assert [f["name"] for f in listed["files"]] == ["a.md"], listed
    escape = await _call(k, v, {"type": "list", "path": "../"})
    assert escape["reason"] == "unauthorized", escape
    assert (await _call(k, v, {"type": "list", "path": "/etc"}))[
        "reason"
    ] == "unauthorized"


async def test_reveal_shows_the_way_down(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    Path("docs/a/b").mkdir(parents=True)
    Path("docs/a/b/c.md").write_text("c")
    v = (await _open(k, fb, "folder", "docs"))["id"]
    r = await _call(k, v, {"type": "reveal", "path": "a/b/c.md"})
    assert r == {"revealed": "a/b/c.md", "folder_path": "docs", "dirs": ["a", "a/b"]}, r
    missing = await _call(k, v, {"type": "reveal", "path": "a/nope.md"})
    assert missing["reason"] == "not_found", missing


async def test_a_file_view_relays_its_changes(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    Path("live.txt").write_text("one\n")
    Path("other.txt").write_text("x\n")
    v = (await _open(k, fb, "file", "live.txt"))["id"]
    q = _inbox(k, v)
    r = await _call(k, v, {"type": "watch"})
    assert r["watching"] == "live.txt" and r["relayed"] is False, r
    assert (await _call(k, v, {"type": "watch"}))["relayed"] is True  # idempotent
    await asyncio.sleep(0.3)  # the disk watcher is up
    Path("other.txt").write_text("changed\n")  # another path: not this view's
    Path("live.txt").write_text("two\n")
    ev = await _next(q, lambda e: e.get("type") == "changed")
    assert ev == {"type": "changed", "path": "live.txt", "kind": ev["kind"]}, ev
    assert ev["kind"] in ("modified", "added"), ev


async def test_a_folder_view_relays_added_changed_removed(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    Path("box").mkdir()
    v = (await _open(k, fb, "folder", "box"))["id"]
    q = _inbox(k, v)
    await _call(k, v, {"type": "watch"})
    await asyncio.sleep(0.3)
    Path("box/n.md").write_text("n")
    ev = await _next(q, lambda e: e.get("type") in ("added", "changed"))
    assert ev["path"] == "box/n.md" and ev["name"] == "n.md", ev
    Path("box/n.md").unlink()
    ev = await _next(q, lambda e: e.get("type") == "removed")
    assert ev["path"] == "box/n.md", ev


async def test_two_views_of_one_file_share_one_disk_watch(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    Path("shared.txt").write_text("s\n")
    v = (await _open(k, fb, "file", "shared.txt"))["id"]
    await _call(k, v, {"type": "watch"})
    assert (await _open(k, fb, "file", "shared.txt"))["id"] == v  # the same agent
    from file_bridge import views

    # RELAYS is process-wide (one kernel per process): count this bridge's relays of this path
    held = [
        s
        for s in views.RELAYS.values()
        if s["bridge"] == fb and s["path"] == "shared.txt"
    ]
    assert len(held) == 1, held
    assert v in views.RELAYS


async def test_deleting_a_view_ends_its_relay(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    Path("gone.txt").write_text("g\n")
    v = (await _open(k, fb, "file", "gone.txt"))["id"]
    await _call(k, v, {"type": "watch"})
    from file_bridge import views

    assert v in views.RELAYS
    await k.delete(v)
    assert v not in views.RELAYS


async def _docs(k, **args):
    return await _call(k, "kernel", {"type": "docs", **args})


async def test_docs_of_a_view_lists_only_the_views_own_verbs(
    seeded_kernel, file_bridge
):
    """A view answers its own table, so `docs of=<view>` (and `advertises`) name
    only what it answers: never the bridge's delete or rename, which it refuses."""
    k, fb = seeded_kernel, file_bridge
    Path("a.txt").write_text("one\n")
    Path("box").mkdir()
    f = (await _open(k, fb, "file", "a.txt"))["id"]
    d = (await _open(k, fb, "folder", "box"))["id"]
    assert sorted((await _docs(k, of=f))["verbs"]) == [
        "read",
        "reflect",
        "watch",
        "write",
    ]
    assert sorted((await _docs(k, of=d))["verbs"]) == [
        "list",
        "reflect",
        "reveal",
        "watch",
    ]
    assert (await _docs(k, of=f, verb="delete"))["reason"] == "bad_arg"
    assert k.get(f) and k.ctx.agents[f].advertises("read")
    assert not k.ctx.agents[f].advertises("delete")
    # the bridge itself still answers the whole table
    assert "delete" in (await _docs(k, of=fb))["verbs"]
    assert k.ctx.agents[fb].advertises("delete")


async def test_read_reports_whether_the_file_ends_in_a_newline(
    seeded_kernel, file_bridge
):
    """`newline` is what a whole-document save must keep: a file with no final newline
    is not given one by the editor."""
    k, fb = seeded_kernel, file_bridge
    Path("with.txt").write_text("a\nb\n")
    Path("without.txt").write_text("a\nb")
    Path("empty.txt").write_text("")
    for name, want in (
        ("with.txt", True),
        ("without.txt", False),
        ("empty.txt", False),
    ):
        r = await _call(k, fb, {"type": "read", "path": name})
        assert r["newline"] is want, (name, r)
