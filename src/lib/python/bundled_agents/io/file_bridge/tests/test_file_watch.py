"""`watch_path`: a path under the disk watcher, any writer's change
emitted on the bridge's own inbox as `{type: changed, path, kind}`. Against the real
disk: a direct write (an editor), an atomic save (temp file renamed over), a write
through the bridge, a directory's entries; refusals; `unwatch_path` ends it."""

from __future__ import annotations

import asyncio
import os
import sys
from pathlib import Path

import pytest


async def _call(k, target, payload):
    return (await k.send([{"target": target, "payload": payload}]))[0]["result"]


def _inbox(k, src: str, watcher: str = "_t_watch"):
    q = k.ctx.ensure_inbox(watcher)
    k.watch(src, watcher)
    return q


async def _next_changed(q, path: str, timeout: float = 5.0) -> dict:
    """The next `changed` event for `path` (other events skipped)."""
    end = asyncio.get_event_loop().time() + timeout
    while True:
        left = end - asyncio.get_event_loop().time()
        assert left > 0, f"no `changed` for {path!r} within {timeout}s"
        ev = await asyncio.wait_for(q.get(), left)
        if ev.get("type") == "changed" and ev.get("path") == path:
            return ev


async def _quiet(q, path: str, wait: float = 0.6) -> bool:
    """No `changed` for `path` within `wait`."""
    try:
        await _next_changed(q, path, wait)
    except (AssertionError, asyncio.TimeoutError):
        return True
    return False


async def _changes_for(q, path: str, secs: float = 1.0) -> list[str]:
    """The kinds of every `changed` for `path` that arrive within `secs`, in arrival order."""
    kinds: list[str] = []
    end = asyncio.get_event_loop().time() + secs
    while True:
        left = end - asyncio.get_event_loop().time()
        if left <= 0:
            return kinds
        try:
            ev = await asyncio.wait_for(q.get(), left)
        except asyncio.TimeoutError:
            return kinds
        if ev.get("type") == "changed" and ev.get("path") == path:
            kinds.append(ev["kind"])


async def test_a_direct_write_an_atomic_save_and_a_bridge_write_are_seen(
    seeded_kernel, file_bridge
):
    k, fb = seeded_kernel, file_bridge
    Path("prompt.md").write_text("one\n")
    q = _inbox(k, fb)
    r = await _call(k, fb, {"type": "watch_path", "path": "prompt.md"})
    assert r == {"watching": "prompt.md", "paths": ["prompt.md"]}, r
    await asyncio.sleep(0.3)  # the watcher is up
    Path("prompt.md").write_text("two\n")  # an editor's in-place save
    ev = await _next_changed(q, "prompt.md")
    assert ev["kind"] in ("modified", "added"), ev
    Path(".prompt.md.tmp").write_text("three\n")  # an atomic save
    os.replace(".prompt.md.tmp", "prompt.md")
    await _next_changed(q, "prompt.md")
    w = await _call(k, fb, {"type": "write", "path": "prompt.md", "bytes": "four\n"})
    assert w.get("version"), w
    await _next_changed(q, "prompt.md")
    os.remove("prompt.md")
    ev = await _next_changed(q, "prompt.md")
    assert ev["kind"] == "deleted", ev


async def test_a_directory_reports_its_entries(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    Path("notes").mkdir()
    q = _inbox(k, fb)
    assert (await _call(k, fb, {"type": "watch_path", "path": "notes"}))[
        "watching"
    ] == "notes"
    await asyncio.sleep(0.3)
    Path("notes/a.md").write_text("x")
    ev = await _next_changed(q, "notes/a.md")
    assert ev["kind"] in ("added", "modified"), ev


async def test_only_the_watched_path_is_reported(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    Path("prompt.md").write_text("one\n")
    q = _inbox(k, fb)
    await _call(k, fb, {"type": "watch_path", "path": "prompt.md"})
    await asyncio.sleep(0.3)
    Path("other.md").write_text("noise")  # same directory, not watched
    assert await _quiet(q, "other.md")


async def test_unwatch_ends_it_and_a_path_is_held_per_caller(
    seeded_kernel, file_bridge
):
    k, fb = seeded_kernel, file_bridge
    Path("prompt.md").write_text("one\n")
    q = _inbox(k, fb)
    await _call(k, fb, {"type": "watch_path", "path": "prompt.md"})
    r = await _call(k, fb, {"type": "unwatch_path", "path": "prompt.md"})
    assert r == {"watching": False, "paths": []}, r
    await asyncio.sleep(0.3)
    Path("prompt.md").write_text("two\n")
    assert await _quiet(q, "prompt.md")


async def test_what_is_not_exposed_or_not_there_is_refused(seeded_kernel):
    k = seeded_kernel
    rec = (
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
    )[0]["result"]
    fb = rec["id"]
    Path("secret.md").write_text("no")
    r = await _call(k, fb, {"type": "watch_path", "path": "secret.md"})
    assert r.get("reason") == "not_found", r  # not exposed reads as absent
    r = await _call(k, fb, {"type": "watch_path", "path": "docs/missing/deep.md"})
    assert r.get("reason") == "not_found", r
    r = await _call(k, fb, {"type": "watch_path"})  # no path = the root, as every verb
    assert r.get("reason") == "not_found", r


async def test_deleting_the_bridge_stops_its_watcher(seeded_kernel, file_bridge):
    from file_bridge.tools import _WATCH

    k, fb = seeded_kernel, file_bridge
    Path("prompt.md").write_text("one\n")
    await _call(k, fb, {"type": "watch_path", "path": "prompt.md"})
    task = _WATCH[fb]["task"]
    await _call(k, k.id, {"type": "delete_agent", "id": fb})
    await asyncio.sleep(0.1)
    assert task.cancelled() or task.done()
    assert fb not in _WATCH


async def test_a_path_the_bridge_stops_exposing_reports_nothing(
    seeded_kernel, file_bridge
):
    """`allow` is read live: narrowed after the watch was armed, the path's
    changes stop reaching watchers."""
    k, fb = seeded_kernel, file_bridge
    Path("notes").mkdir()
    Path("notes/a.md").write_text("x")
    await asyncio.sleep(0.6)  # the creation is old news before the watch arms
    q = _inbox(k, fb)
    await _call(k, fb, {"type": "watch_path", "path": "notes"})
    await asyncio.sleep(0.3)
    k.ctx.update(fb, allow=["other.md"])
    await asyncio.sleep(0.3)
    while not q.empty():  # anything the watcher saw before the narrowing
        q.get_nowait()
    Path("notes/a.md").write_text("y")
    assert await _quiet(q, "notes/a.md")


async def test_create_arm_write_gives_exactly_one_modified(seeded_kernel, file_bridge):
    """A file created before the arm, then written after it: one `modified`. The
    platform may replay the create (macOS FSEvents), which must not read as `added`."""
    k, fb = seeded_kernel, file_bridge
    Path("f.md").write_text("one\n")
    q = _inbox(k, fb)
    await _call(k, fb, {"type": "watch_path", "path": "f.md"})
    await asyncio.sleep(0.3)
    Path("f.md").write_text("one, written longer\n")
    assert await _changes_for(q, "f.md") == ["modified"]


async def test_an_atomic_save_after_arm_gives_added_and_a_no_op_event_gives_nothing(
    seeded_kernel, file_bridge
):
    k, fb = seeded_kernel, file_bridge
    Path("f.md").write_text("one\n")
    q = _inbox(k, fb)
    await _call(k, fb, {"type": "watch_path", "path": "f.md"})
    await asyncio.sleep(0.3)
    Path(".f.md.tmp").write_text("two\n")
    os.replace(".f.md.tmp", "f.md")  # a new inode at the path
    assert await _changes_for(q, "f.md") == ["added"]
    with open("f.md", "r+b"):  # an open and a close, no write
        pass
    assert await _quiet(q, "f.md")


@pytest.mark.skipif(
    sys.platform == "win32" or not hasattr(os, "geteuid") or os.geteuid() == 0,
    reason="needs a posix permission check that root does not fail",
)
async def test_a_watch_that_cannot_arm_is_refused(seeded_kernel, file_bridge):
    k, fb = seeded_kernel, file_bridge
    locked = Path("locked")
    locked.mkdir()
    locked.chmod(0)
    try:
        r = await _call(k, fb, {"type": "watch_path", "path": "locked"})
        assert r.get("reason") == "transport_error", r
        assert "watching" not in r, r
        u = await _call(k, fb, {"type": "unwatch_path", "path": "locked"})
        assert u == {"watching": False, "paths": []}, u
    finally:
        locked.chmod(0o755)


def test_judge_covers_every_row():
    from file_bridge.tools import _judge

    a = (1, 10, 10, 4)
    assert _judge(None, a) == "added"
    assert _judge(a, None) == "deleted"
    assert _judge(a, (2, 10, 10, 4)) == "added"  # a different inode
    assert _judge(a, (1, 10, 10, 5)) == "modified"  # size
    assert _judge(a, (1, 10, 11, 4)) == "modified"  # mtime
    assert _judge(a, (1, 11, 10, 4)) == "modified"  # ctime
    assert _judge(a, a) is None
    assert _judge(None, None) is None


def test_stamp_reads_a_file_and_nothing_at_a_missing_path(tmp_path):
    from file_bridge.tools import _stamp

    f = tmp_path / "abc.txt"
    f.write_text("abc")
    s = _stamp(str(f))
    assert s is not None and s[3] == 3, s
    assert _stamp(str(f)) == s
    assert _stamp(str(tmp_path / "missing.txt")) is None


async def test_a_rearm_drops_the_old_stream(seeded_kernel, file_bridge):
    from file_bridge.tools import _WATCH

    k, fb = seeded_kernel, file_bridge
    for name in ("a.md", "b.md", "c.md"):
        Path(name).write_text(name)
    q = _inbox(k, fb)
    await _call(k, fb, {"type": "watch_path", "path": "a.md"})
    await _call(k, fb, {"type": "watch_path", "path": "b.md"})
    await asyncio.sleep(0.3)
    old = _WATCH[fb]["task"]
    await _call(k, fb, {"type": "watch_path", "path": "c.md"})  # a re-arm
    new = _WATCH[fb]["task"]
    assert new is not old
    await asyncio.sleep(0.1)
    assert old.cancelled() or old.done()
    await asyncio.sleep(0.3)
    Path("a.md").write_text("a, written again")
    assert await _changes_for(q, "a.md") == ["modified"]
