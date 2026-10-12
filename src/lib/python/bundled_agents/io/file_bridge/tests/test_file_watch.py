"""`watch_path` (owner, 2026-10-08): a path under the disk watcher, any writer's change
emitted on the bridge's own inbox as `{type: changed, path, kind}`. Against the real
disk: a direct write (an editor), an atomic save (temp file renamed over), a write
through the bridge, a directory's entries; refusals; `unwatch_path` ends it."""

from __future__ import annotations

import asyncio
import os
from pathlib import Path


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
