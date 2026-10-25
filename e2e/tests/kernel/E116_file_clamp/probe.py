"""kernel/file_clamp — file_bridge's clamp and allow list over REST. NO MODEL.

The kernel's own `file_bridge` on the PACKAGED host of `RUNTIME` (python |
rust), every call through `web_rest`, its `changed` events over the `ws` face. The
bridge serves `box/` (root) and exposes `pub` and `bin` (allow):

  1. escapes refused, nothing leaked: `..` out of the root, `..` to an unexposed
     sibling, absolute paths (outside and inside the running dir);
  2. symlinks: a link inside `pub` pointing OUT of the root, or to an unexposed file
     inside it, serves nothing (read, read_stream, list hides it); a link staying
     inside the exposed area serves;
  3. writes never follow a link out: write / append / write_stream through a link
     to outside the root or to an unexposed file are refused and the target is
     untouched — also after a regular file was SWAPPED for such a link;
  4. an atomic write leaves no temp file behind;
  5. `allow` is read LIVE: update_agent narrows it, the next call obeys; widened
     again, the next call serves;
  6. watch_path honours `allow` live: a watched dir dropped from `allow` reports
     nothing more while a still-exposed one keeps reporting; an unexposed path
     cannot be watched;
  7. binary: write_stream / read_stream carry raw bytes (all 256 values, several
     chunks) and the disk holds exactly them; `read` refuses a binary file.
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
OUTSIDE = "outside-the-root-but-in-the-running-dir"
FOREIGN = "foreign-file-outside-the-running-dir"
BLOB = bytes(range(256)) * 600  # 153600 bytes, three 64 KiB chunks


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


def poll(fn, what: str, timeout: float = 15.0, step: float = 0.05):
    """fn() until truthy, bounded; returns the last value (falsy on timeout)."""
    end = time.monotonic() + timeout
    while True:
        v = fn()
        if v or time.monotonic() > end:
            if not v:
                print(f"  .. timed out waiting for {what}", flush=True)
            return v
        time.sleep(step)


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-file-clamp-"))
    foreign_dir = Path(tempfile.mkdtemp(prefix="e2e-file-clamp-foreign-"))
    foreign = foreign_dir / "foreign.txt"
    foreign.write_text(FOREIGN)
    box = work / "box"
    (box / "pub").mkdir(parents=True)
    (box / "bin").mkdir()
    (box / "pub/a.txt").write_text("alpha\nbeta\n")
    (box / "bin/b.txt").write_text("bee\n")
    (box / "secret.txt").write_text(SECRET)
    (work / "outside.txt").write_text(OUTSIDE)
    os.symlink(foreign, box / "pub/out_link")  # out of the root (and the running dir)
    os.symlink("../secret.txt", box / "pub/in_link")  # inside the root, unexposed
    os.symlink("a.txt", box / "pub/ok_link")  # stays inside the exposed area
    hk = HostKernel(spec, work, reach=["fb"]).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def fb(payload: dict) -> dict:
        r = hk.send("fb", payload, timeout=60)
        return r if isinstance(r, dict) else {"error": f"non-dict reply {r!r}"}

    def refused(r: dict, *leaks: str) -> bool:
        """A refusal (a `reason`), and none of `leaks` anywhere in the reply."""
        txt = json.dumps(r, default=lambda b: b.decode("latin-1"))
        return bool(r.get("reason")) and not any(s in txt for s in leaks)

    def allow(paths: list[str]) -> dict:
        return hk.send(root, {"type": "update_agent", "id": "fb", "allow": paths})

    try:
        rec = hk.create(parent_id="rest", handler_module="file_bridge.tools", id="fb",
                        root="box", allow=["pub", "bin"],
                        ingress_rule="allow_all_ingress", callable_by=GRANTS)
        need(rec.get("id") == "fb", f"create fb: {str(rec)[:200]}")
        r = fb({"type": "read", "path": "pub/a.txt"})
        need([x.get("text") for x in r.get("lines") or []] == ["alpha", "beta"],
             f"an exposed file reads: {str(r)[:160]}")

        # 1 · escapes
        for p in ("../outside.txt", "pub/../../outside.txt", "pub/../../../../etc/hosts"):
            r = fb({"type": "read", "path": p})
            need(refused(r, OUTSIDE, "localhost"), f"`..` out of the root refused: {p} → {str(r)[:140]}")
        r = fb({"type": "read", "path": "pub/../secret.txt"})
        need(refused(r, SECRET) and r.get("reason") == "not_found",
             f"`..` to an unexposed sibling is not_found: {str(r)[:140]}")
        for p in ("/etc/hosts", str(work / "outside.txt"), str(box / "pub/a.txt")):
            r = fb({"type": "read", "path": p})
            need(refused(r, OUTSIDE, "localhost", "alpha"),
                 f"an absolute path is refused: {p} → {str(r)[:140]}")
        r = fb({"type": "read_stream", "path": "../outside.txt"})
        need(refused(r, OUTSIDE), f"read_stream `..` refused: {str(r)[:140]}")
        r = fb({"type": "write", "path": "../escaped.txt", "bytes": "x"})
        need(refused(r) and not (work / "escaped.txt").exists(),
             f"write `..` refused, nothing written: {str(r)[:140]}")
        r = fb({"type": "write", "path": str(work / "abs.txt"), "bytes": "x"})
        need(refused(r) and not (work / "abs.txt").exists(),
             f"write to an absolute path refused, nothing written: {str(r)[:140]}")

        # 2 · symlinks on read
        for p, leak in (("pub/out_link", FOREIGN), ("pub/in_link", SECRET)):
            r = fb({"type": "read", "path": p})
            need(refused(r, leak), f"read through {p} serves nothing: {str(r)[:140]}")
            r = fb({"type": "read_stream", "path": p})
            need(refused(r, leak), f"read_stream through {p} serves nothing: {str(r)[:140]}")
        r = fb({"type": "read", "path": "pub/ok_link"})
        need([x.get("text") for x in r.get("lines") or []] == ["alpha", "beta"],
             f"a link inside the exposed area serves: {str(r)[:140]}")
        r = fb({"type": "list", "path": "pub"})
        names = sorted(e.get("name", "") for e in r.get("files") or [])
        need(any("ok_link" in n for n in names) and not any("out_link" in n or "in_link" in n
                                                             for n in names),
             f"list hides links that land outside the list: {names}")

        # 3 · writes never follow a link out
        for p, target, before in (("pub/out_link", foreign, FOREIGN),
                                  ("pub/in_link", box / "secret.txt", SECRET)):
            for verb, pl in (("write", {"bytes": "PWNED"}), ("append", {"bytes": "PWNED"}),
                             ("write_stream", {"bytes": b"PWNED", "truncate": True})):
                r = fb({"type": verb, "path": p, **pl})
                need(refused(r) and target.read_text() == before,
                     f"{verb} through {p} refused, target untouched: {str(r)[:120]}")
        r = fb({"type": "write", "path": "pub/swap.txt", "bytes": "v1"})
        need(r.get("size") == 2, f"a regular write: {str(r)[:120]}")
        (box / "pub/swap.txt").unlink()
        os.symlink(foreign, box / "pub/swap.txt")
        r = fb({"type": "write", "path": "pub/swap.txt", "bytes": "v2"})
        need(refused(r) and foreign.read_text() == FOREIGN
             and (box / "pub/swap.txt").is_symlink(),
             f"a write after the file was swapped for an outward link is refused: {str(r)[:120]}")
        (box / "pub/swap.txt").unlink()
        os.symlink("../secret.txt", box / "pub/swap.txt")
        r = fb({"type": "write", "path": "pub/swap.txt", "bytes": "v3"})
        need(refused(r) and (box / "secret.txt").read_text() == SECRET,
             f"…and for a link to an unexposed file: {str(r)[:120]}")
        (box / "pub/swap.txt").unlink()

        # 4 · atomic writes leave nothing behind
        for i in range(5):
            fb({"type": "write", "path": "pub/doc.txt", "bytes": f"version {i}\n" * 2000})
        r = fb({"type": "write", "path": "pub/deep/new/doc.txt", "bytes": "nested\n"})
        need(r.get("size") == 7, f"a write creates its parent dirs: {str(r)[:120]}")
        leftovers = [str(p.relative_to(box)) for p in box.rglob("*")
                     if ".tmp" in p.name or p.name.startswith(".")]
        need(not leftovers and (box / "pub/doc.txt").read_text() == "version 4\n" * 2000,
             f"atomic writes leave no temp file: {leftovers}")

        # 5 · `allow` is read live
        r = allow(["pub"])
        need(r.get("updated") is True, f"update_agent allow=[pub]: {str(r)[:120]}")
        r = fb({"type": "read", "path": "bin/b.txt"})
        need(r.get("reason") == "not_found" and "bee" not in json.dumps(r),
             f"the next call obeys the narrowed list: {str(r)[:140]}")
        r = fb({"type": "write", "path": "bin/new.txt", "bytes": "x"})
        need(refused(r) and not (box / "bin/new.txt").exists(),
             f"…writes too: {str(r)[:140]}")
        r = allow(["pub", "bin"])
        r = fb({"type": "read", "path": "bin/b.txt"})
        need([x.get("text") for x in r.get("lines") or []] == ["bee"],
             f"widened again, the next call serves: {str(r)[:140]}")
        r = hk.send(root, {"type": "update_agent", "id": "fb", "allow": ["../x"]})
        need(bool(r.get("error")) and r.get("updated") is not True,
             f"update_agent refuses an allow entry that climbs out: {str(r)[:140]}")

        # 6 · watch_path honours `allow` live
        seen = watch(hk.port, "fb")
        r = fb({"type": "watch_path", "path": "secret.txt"})
        need(r.get("reason") == "not_found", f"an unexposed path cannot be watched: {str(r)[:140]}")
        for p in ("pub", "bin"):
            r = fb({"type": "watch_path", "path": p})
            need(r.get("watching") == p, f"watch_path {p}: {str(r)[:140]}")

        def changed(path: str) -> bool:
            return any(e.get("type") == "changed" and e.get("path") == path for e in list(seen))

        # The ws watch and the disk watcher come up async: write until one lands.
        got = poll(lambda: ((box / "pub/w1.txt").write_text(str(time.time())) and False)
                   or changed("pub/w1.txt"), "a changed event for pub/w1.txt", step=0.3)
        need(bool(got), "a watched exposed dir reports a change")
        ev = next((e for e in list(seen) if e.get("path") == "pub/w1.txt"), {})
        need(ev.get("kind") in ("added", "modified"), f"changed event shape: {ev}")
        r = allow(["bin"])
        need(r.get("updated") is True, f"update_agent allow=[bin]: {str(r)[:120]}")
        (box / "pub/w2.txt").write_text("hidden now")
        got = poll(lambda: ((box / "bin/marker.txt").write_text(str(time.time())) and False)
                   or changed("bin/marker.txt"), "a changed event for bin/marker.txt", step=0.3)
        need(bool(got), "the still-exposed watched dir keeps reporting")
        need(not changed("pub/w2.txt"),
             "a watched dir dropped from `allow` reports nothing more")
        allow(["pub", "bin"])

        # 7 · binary round trip, raw bytes
        chunk = 65536
        for i in range(0, len(BLOB), chunk):
            r = fb({"type": "write_stream", "path": "bin/blob.bin",
                    "bytes": BLOB[i:i + chunk], "truncate": i == 0})
            need(r.get("written") == len(BLOB[i:i + chunk]) and r.get("offset") == i,
                 f"write_stream chunk @{i}: {str(r)[:120]}")
        need((box / "bin/blob.bin").read_bytes() == BLOB, "the disk holds exactly the bytes sent")
        got, off, eof, n = b"", 0, False, 0
        while not eof and n < 20:
            r = fb({"type": "read_stream", "path": "bin/blob.bin", "offset": off})
            b = r.get("bytes")
            if not isinstance(b, (bytes, bytearray)):
                need(False, f"read_stream returns raw bytes: {str(r)[:160]}")
                break
            got += bytes(b)
            off, eof, n = r.get("next_offset"), r.get("eof"), n + 1
            if r.get("size") != len(BLOB):
                need(False, f"read_stream size: {r.get('size')}")
                break
        need(got == BLOB and n == 3, f"read_stream round trip: {len(got)} bytes in {n} chunks")
        r = fb({"type": "read_stream", "path": "bin/blob.bin", "offset": 300, "length": 10})
        need(r.get("bytes") == BLOB[300:310] and r.get("next_offset") == 310
             and r.get("eof") is False, f"read_stream at an offset: {str(r)[:160]}")
        r = fb({"type": "read", "path": "bin/blob.bin"})
        need(r.get("reason") == "bad_arg", f"`read` refuses a binary file: {str(r)[:140]}")
    finally:
        hk.close()
        shutil.rmtree(work, ignore_errors=True)
        shutil.rmtree(foreign_dir, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
