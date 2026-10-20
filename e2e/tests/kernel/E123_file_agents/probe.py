"""kernel/file_agents — the file and folder AGENTS over a file_bridge. NO MODEL.

The kernel's own `file_bridge` (root `box/`, `allow: [docs, top.txt]`) on the packaged
host of `RUNTIME` (python | rust | swift), every call over `web_rest`, the live events
over the `ws` face. A file or a folder is a VIEW AGENT of the bridge (`open_view`), one
agent per path. The harness picks the view ids and declares them in `reach` at boot, so
the ws leg may watch them:

  1. `open_view` makes a file view and a folder view under the ids the harness chose,
     and a second open of the same path is the same agent (`created: false`); a path the
     bridge does not expose, a `..` or an absolute path is refused and makes no agent;
  2. the file view reads and writes its own path against the version read: a write
     with the current version lands, a stale one is refused and the disk is untouched;
     a file view names no other path (refused, nothing of it leaks);
  3. the folder view lists inside its folder and refuses a climb out of it; `reveal`
     shows the folders on the way down, and says `not_found` for what is not there;
  4. `watch` arms a view's relay on its bridge, once: the second `watch` of the same
     view is the same relay, not a second one;
  5. LIVE, over the ws face: a file view emits `changed` after an on-disk edit; a folder
     view emits `added` and then `removed` for an entry made and deleted in it.

The canvas drop and the TUI's views are the page's; this probe pins the kernel contract
they both stand on.
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import sys
import tempfile
import threading
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.host.ws import WSWatcher
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
GRANTS = {"$self": "*", "$parent": "*", "ws": "*"}
SECRET = "secret-in-root-not-exposed"
TOP = "top-level-content-not-exposed"
#: The view ids the harness chooses and declares at boot (so the ws leg may watch them).
FILE_VIEW = "e123-file"
FOLDER_VIEW = "e123-folder"


def watch(port: int, aid: str) -> list:
    """Collect `aid`'s events over the ws face, in a daemon thread, for the run."""
    seen: list = []

    async def run():
        w = WSWatcher(port, aid)
        await w.connect()
        async for ev in w.events():
            seen.append(ev)

    threading.Thread(target=lambda: asyncio.run(run()), daemon=True).start()
    return seen


def until(step, done, timeout: float = 30.0) -> bool:
    """Run `step(i)` and check `done()`, until it holds or the bound passes. The step is
    repeated because the disk watch may not be armed on the first edit."""
    end = time.monotonic() + timeout
    i = 0
    while not done():
        if time.monotonic() > end:
            return False
        step(i)
        i += 1
        time.sleep(0.2)
    return True


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-file-agents-"))
    box = work / "box"
    (box / "docs/sub").mkdir(parents=True)
    (box / "docs/a.md").write_text("alpha\nbeta\n")
    (box / "docs/sub/b.txt").write_text("bee\n")
    (box / "top.txt").write_text(TOP)
    (box / "secret.txt").write_text(SECRET)
    hk = HostKernel(spec, work, reach=["fb", FILE_VIEW, FOLDER_VIEW]).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def fb(payload: dict, target: str = "fb") -> dict:
        r = hk.send(target, payload, timeout=60)
        return r if isinstance(r, dict) else {"error": f"non-dict reply {r!r}"}

    def refused(r: dict, *leaks: str) -> bool:
        """A refusal (a `reason`), and none of `leaks` anywhere in the reply."""
        txt = json.dumps(r, default=lambda b: b.decode("latin-1"))
        return bool(r.get("reason")) and not any(s in txt for s in leaks)

    try:
        rec = hk.create(parent_id="rest", handler_module="file_bridge.tools", id="fb",
                        root="box", allow=["docs", "top.txt"],
                        ingress_rule="allow_all_ingress", callable_by=GRANTS)
        need(rec.get("id") == "fb", f"create fb: {str(rec)[:200]}")

        # 1 · open: one agent per path, under the chosen ids
        f1 = fb({"type": "open_view", "kind": "file", "path": "docs/a.md", "id": FILE_VIEW})
        need(f1.get("created") is True and f1.get("id") == FILE_VIEW and f1.get("kind") == "file",
             f"open a file view under its chosen id: {str(f1)[:160]}")
        fid = FILE_VIEW
        f2 = fb({"type": "open_view", "kind": "file", "path": "docs/a.md"})
        need(f2.get("id") == fid and f2.get("created") is False,
             f"a second open of the same file is the same agent: {str(f2)[:160]}")
        d1 = fb({"type": "open_view", "kind": "folder", "path": "docs", "id": FOLDER_VIEW})
        need(d1.get("created") is True and d1.get("kind") == "folder" and d1.get("id") == FOLDER_VIEW,
             f"open a folder view under its chosen id: {str(d1)[:160]}")
        did = FOLDER_VIEW
        d2 = fb({"type": "open_view", "kind": "folder", "path": "docs"})
        need(d2.get("id") == did and d2.get("created") is False, "a second open of the folder is the same agent")
        taken = fb({"type": "open_view", "kind": "file", "path": "top.txt", "id": did})
        need(taken.get("reason") == "exists", f"a chosen id that is another view's is refused: {str(taken)[:140]}")
        for kind, p in (("file", "secret.txt"), ("file", "../top.txt"), ("file", "/etc/hosts"),
                        ("file", "."), ("dir", "docs")):
            r = fb({"type": "open_view", "kind": kind, "path": p})
            need(refused(r, SECRET, TOP) and "id" not in r,
                 f"open_view {kind} {p!r} refused, no agent made: {str(r)[:140]}")
        rf = fb({"type": "reflect"}, target=fid)
        need(rf.get("kind") == "file" and rf.get("path") == "docs/a.md" and rf.get("bridge") == "fb",
             f"a file view reflects what it views: {str(rf)[:160]}")

        # 2 · the file view: its own path, against the version read
        r = fb({"type": "read"}, target=fid)
        need([x.get("text") for x in r.get("lines") or []] == ["alpha", "beta"],
             f"the file view reads its file: {str(r)[:160]}")
        ver = r.get("version")
        w = fb({"type": "write", "bytes": "alpha2\nbeta\n", "if_match": ver}, target=fid)
        need(w.get("size") == 12 and (box / "docs/a.md").read_text() == "alpha2\nbeta\n",
             f"a write against the version read lands on the disk: {str(w)[:160]}")
        stale = fb({"type": "write", "bytes": "lost\n", "if_match": ver}, target=fid)
        need(refused(stale) and (box / "docs/a.md").read_text() == "alpha2\nbeta\n",
             f"a write against a version that moved on is refused, disk untouched: {str(stale)[:140]}")
        other = fb({"type": "read", "path": "top.txt"}, target=fid)
        need(refused(other, TOP) and other.get("reason") == "unauthorized",
             f"the file view names no other path, and leaks none of it: {str(other)[:140]}")
        lst = fb({"type": "list"}, target=fid)
        need(lst.get("reason") is not None and TOP not in json.dumps(lst),
             f"a file view has no listing: {str(lst)[:120]}")

        # 3 · the folder view: inside its folder
        lst = fb({"type": "list"}, target=did)
        names = sorted(e.get("name", "") for e in lst.get("files") or [])
        need(names == ["a.md", "sub"], f"the folder view lists its folder: {names}")
        sub = fb({"type": "list", "path": "sub"}, target=did)
        need([e.get("name") for e in sub.get("files") or []] == ["b.txt"],
             f"a folder view lists a folder inside it: {str(sub)[:140]}")
        up = fb({"type": "list", "path": "../"}, target=did)
        need(refused(up, TOP, SECRET) and up.get("reason") == "unauthorized",
             f"a folder view refuses a climb out of its folder: {str(up)[:140]}")
        rv = fb({"type": "reveal", "path": "sub/b.txt"}, target=did)
        need(rv.get("revealed") == "sub/b.txt" and rv.get("dirs") == ["sub"],
             f"reveal shows the way down: {str(rv)[:160]}")
        nf = fb({"type": "reveal", "path": "nope.txt"}, target=did)
        need(nf.get("reason") == "not_found", f"reveal of nothing is not_found: {str(nf)[:120]}")

        # 4 · watch: a view arms its relay on the bridge, once
        wf = fb({"type": "watch"}, target=fid)
        need(wf.get("relayed") is False and wf.get("agent") == fid,
             f"the file view's watch arms its relay: {str(wf)[:120]}")
        need(fb({"type": "watch"}, target=fid).get("relayed") is True,
             "a second watch of the same view is the same relay")
        wd = fb({"type": "watch"}, target=did)
        need(wd.get("relayed") is False and wd.get("watching") == "docs",
             f"the folder view's watch arms its own relay: {str(wd)[:120]}")

        # 5 · live, over the ws face: the events the armed relays emit
        fseen = watch(hk.port, fid)
        dseen = watch(hk.port, did)
        changed = lambda e: e.get("type") == "changed" and e.get("path") == "docs/a.md"  # noqa: E731
        until(lambda i: (box / "docs/a.md").write_text(f"live {i}\n"),
              lambda: any(changed(e) for e in fseen))
        need(any(changed(e) for e in fseen),
             f"a file view emits `changed` after an on-disk edit: {fseen[:2]}")
        added = lambda e: e.get("type") == "added" and e.get("name") == "live.md"  # noqa: E731
        removed = lambda e: e.get("type") == "removed" and e.get("name") == "live.md"  # noqa: E731
        until(lambda i: (box / "docs/live.md").write_text(f"{i}\n"), lambda: any(added(e) for e in dseen))
        need(any(added(e) for e in dseen), f"a folder view emits `added` for a new entry: {dseen[:3]}")
        until(lambda i: (box / "docs/live.md").unlink(missing_ok=True),
              lambda: any(removed(e) for e in dseen))
        need(any(removed(e) for e in dseen), f"a folder view emits `removed` for a deleted entry: {dseen[-3:]}")
        need(all(e.get("path", "").startswith("docs") for e in dseen),
             "a folder view's events name only its folder's entries")
    finally:
        hk.close()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
