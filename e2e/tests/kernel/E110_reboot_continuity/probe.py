"""kernel/reboot_continuity — the tree a kernel built comes back whole on the
next boot of the same directory. NO MODEL.

The PACKAGED rust host, every call over `web_rest`:

  1. BUILD: a three-level chain of agents under the leg (`top` → `mid` → `leaf`),
     records carrying extra fields (str, int, float, bool, list, nested map,
     unicode), a `can` line on the caller (`top` → `ghost`, written after
     `ghost` exists; the target keeps no list); a
     `files` file_bridge with sixteen text files, a nested one and a binary one
     (raw bytes over the octet leg); an `update_agent` of a field; an agent and
     a file DELETED before the stop;
  2. AGENT REBOOT in one process (what coding/E75 does to its AI agent):
     rows planted in the memory store under `ghost`, `ghost` deleted and
     re-created on the same id — its rows are still in the store; `files`
     deleted and re-created on the same root — every file still lists;
  3. STOP (SIGTERM) and BOOT the same directory: every id with its parent,
     every field, the `can` line, the update, both deletes, the guid (read
     from the state folder: root fields are operator only); every file
     lists and reads back byte for byte; the agents answer; the id counter
     never repeats an id;
  4. A SECOND stop + boot with nothing changed in between: the same tree again.
"""

from __future__ import annotations

import os
import secrets
import shutil
import sys
import tempfile
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

import asyncio

import yaml

from helpers import memory
from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.host.wire import WireRoot
from helpers.lines import call, is_unknown
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
N = 16
BIN = bytes(range(256)) * 3 + b"\x00tail\xff"
#: Every id the probe creates. All live under `rest`, which reaches them by
#: position, so the seed gives the legs no line on any of them.
IDS = ["top", "mid", "leaf", "files", "ghost", "gone"]

#: The fields `top` carries, every JSON type the record format has to round-trip.
EXTRA = {
    "note": "first",
    "count": 42,
    "ratio": 0.25,
    "flag": True,
    "tags": ["a", "b", "c"],
    "nested": {"k": [1, {"deep": "yes"}], "empty": {}},
    "label": "Київ — ✓",
}
#: `top`'s own line: two verbs on its sibling `ghost`. The target keeps no list.
TOP_CAN = {"ghost": call("work", "started")}


def file_body(i: int) -> str:
    return f"file {i}\n" + "line\n" * i


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-reboot-"))
    bad: list[str] = []
    kernels: list[HostKernel] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def boot() -> HostKernel:
        hk = HostKernel(spec, work).boot()
        kernels.append(hk)
        return hk

    def stop(hk: HostKernel) -> None:
        hk.close()
        need(hk.proc.returncode is not None, f"the kernel exited (code {hk.proc.returncode})")

    def records(hk: HostKernel) -> dict[str, dict]:
        # The LIVE records: the root's `reflect tree=all detail=full`, whose nodes
        # carry each record whole inside `rest`'s tree (an agent's own reflect is
        # its bundle's reply, which leaves record fields out). A missing id is in
        # no node, and its own reflect gives the unknown answer.
        out: dict[str, dict] = {}
        rf = hk.send("kernel", {"type": "reflect", "tree": "all", "detail": "full"})
        todo = [rf.get("tree")] if isinstance(rf, dict) else []
        while todo:
            n = todo.pop()
            if not isinstance(n, dict):
                continue
            if n.get("id") in IDS:
                out[n["id"]] = {k: v for k, v in n.items() if k != "children"}
            todo.extend(n.get("children") or [])
        for aid in IDS:
            if aid not in out:
                r = hk.send(aid, {"type": "reflect"})
                need(is_unknown(r, aid), f"{aid}: in no tree node, and its reflect is the unknown answer: {str(r)[:160]}")
        return out

    def disk_guid() -> str | None:
        # The root's guid, from the state folder with the daemon down: root
        # fields are operator only, so `rest` never reads it.
        root = yaml.safe_load((work / ".fantastic" / "agent.yaml").read_text()) or {}
        g = root.get("guid")
        return g if isinstance(g, str) and g else None

    def listed(hk: HostKernel) -> set[str]:
        out: set[str] = set()
        for path in ("data", "data/sub"):
            r = hk.send("files", {"type": "list", "path": path})
            out |= {f.get("path") for f in r.get("files") or [] if f.get("type") != "dir"}
        return out

    def read_text(hk: HostKernel, path: str) -> str | None:
        r = hk.send("files", {"type": "read", "path": path, "limit": 1000})
        if not isinstance(r.get("lines"), list):
            return None
        return "".join(ln.get("text", "") + "\n" for ln in r["lines"])

    def read_bin(hk: HostKernel) -> bytes | None:
        r = hk.send("files", {"type": "read_stream", "path": "data/blob.bin", "length": 65536})
        b = r.get("bytes")
        return b if isinstance(b, bytes) else None

    def create(hk: HostKernel, **meta) -> dict:
        rec = hk.create(**meta)
        need(rec.get("id") == meta["id"], f"create {meta['id']}: {str(rec)[:200]}")
        return rec

    def create_files(hk: HostKernel) -> None:
        create(hk, parent_id="rest", handler_module="file_bridge.tools", id="files",
               root=".", allow=["data"])

    want_files = {f"data/f_{i:02d}.txt" for i in range(N) if i != 3} | {
        "data/sub/deep.txt", "data/blob.bin"}

    def check_tree(hk: HostKernel, tag: str, ids_before: int) -> None:
        recs = records(hk)
        for aid, parent in (("top", "rest"), ("mid", "top"), ("leaf", "mid"),
                            ("files", "rest"), ("ghost", "rest")):
            got = (recs.get(aid) or {}).get("parent_id")
            need(got == parent, f"{tag}: {aid} is back under {parent} (got {got!r})")
        need("gone" not in recs, f"{tag}: the agent deleted before the stop stays deleted")
        top = recs.get("top") or {}
        # THE LIVE FIELDS. No wire read shows a record's free fields whole (a tree
        # node carries the posture keys and `can`; top's reflect is its bundle's
        # reply), so the live record is made to show itself: one new field written
        # now, and the kernel persists the WHOLE record it holds in memory. The
        # stored record then carries every field only if the reboot hydrated it.
        touch = f"{tag}-{secrets.token_hex(3)}"
        r = hk.send(hk.root_id, {"type": "update_agent", "id": "top", "touch": touch})
        need(r.get("updated") is True, f"{tag}: update top's touch: {str(r)[:160]}")
        end = time.monotonic() + 10
        stored = hk.stored("top")
        while time.monotonic() < end and stored.get("touch") != touch:
            time.sleep(0.1)
            stored = hk.stored("top")
        want = {**EXTRA, "note": "second", "touch": touch}
        diff = {k: (stored.get(k), v) for k, v in want.items() if stored.get(k) != v}
        need(not diff, f"{tag}: every field of top round-trips, the update kept: {diff or 'all equal'}")
        need(top.get("can") == TOP_CAN, f"{tag}: top's can: {top.get('can')}")
        fb = recs.get("files") or {}
        need(fb.get("root") == "." and fb.get("allow") == ["data"],
             f"{tag}: the file_bridge record (root, allow): "
             f"{ {k: fb.get(k) for k in ('root', 'allow')} }")
        rf = hk.send("kernel", {"type": "reflect", "tree": "none"})
        need(isinstance(rf, dict) and rf.get("id") == hk.root_id and "guid" not in rf,
             f"{tag}: rest's root reflect carries no guid (operator only): {str(rf)[:200]}")
        got = listed(hk)
        need(got == want_files, f"{tag}: {len(got & want_files)}/{len(want_files)} files listed"
             + (f", missing {sorted(want_files - got)}, extra {sorted(got - want_files)}"
                if got != want_files else ""))
        wrong = [i for i in range(N) if i != 3
                 and read_text(hk, f"data/f_{i:02d}.txt") != file_body(i)]
        need(not wrong, f"{tag}: every text file reads back the same (wrong: {wrong})")
        need(read_text(hk, "data/sub/deep.txt") == "deep\n", f"{tag}: the nested file reads back")
        need(read_bin(hk) == BIN, f"{tag}: the binary file reads back byte for byte")
        w = hk.send("top", {"type": "work"})
        need(isinstance(w.get("runs"), int), f"{tag}: top answers a call: {w}")
        led = hk.send("kernel", {"type": "ledger"}).get("executions") or []
        ids = [e["id"] for e in led if isinstance(e.get("id"), int)]
        need(bool(ids) and min(ids) > ids_before,
             f"{tag}: every new execution id is above the last one before the stop "
             f"(min {min(ids) if ids else None} > {ids_before})")

    async def plant(hk: HostKernel) -> list[int]:
        rows = [{"role": "user" if i % 2 == 0 else "assistant", "content": f"message {i}"}
                for i in range(N)]
        return await memory.plant(WireRoot(hk), "ghost", rows)

    async def ghost_rows(hk: HostKernel) -> list[dict]:
        return await memory.rows_of(WireRoot(hk), "ghost")

    try:
        hk = boot()

        # 1 · build
        create(hk, parent_id="rest", handler_module="slow_probe.tools", id="top", **EXTRA)
        create(hk, parent_id="top", handler_module="slow_probe.tools", id="mid")
        create(hk, parent_id="mid", handler_module="slow_probe.tools", id="leaf")
        create(hk, parent_id="rest", handler_module="slow_probe.tools", id="gone")
        create(hk, parent_id="rest", handler_module="slow_probe.tools", id="ghost")
        # the line names `ghost`, so it is written once `ghost` exists
        r = hk.send(hk.root_id, {"type": "update_agent", "id": "top", "can": TOP_CAN})
        need(r.get("updated") is True, f"update_agent top can=TOP_CAN: {str(r)[:200]}")
        create_files(hk)
        for i in range(N):
            r = hk.send("files", {"type": "write", "path": f"data/f_{i:02d}.txt", "bytes": file_body(i)})
            if r.get("error"):
                need(False, f"write f_{i:02d}: {r}")
        r = hk.send("files", {"type": "write", "path": "data/sub/deep.txt", "bytes": "deep\n"})
        need(not r.get("error"), f"write into a new subdirectory: {str(r)[:200]}")
        r = hk.send("files", {"type": "write_stream", "path": "data/blob.bin", "bytes": BIN,
                              "truncate": True})
        need(not r.get("error"), f"write_stream raw bytes: {str(r)[:200]}")
        r = hk.send("files", {"type": "delete", "path": "data/f_03.txt"})
        need(r.get("deleted") is True, f"delete a file: {str(r)[:200]}")
        r = hk.send(hk.root_id, {"type": "update_agent", "id": "top", "note": "second"})
        need(not r.get("error"), f"update_agent top note=second: {str(r)[:200]}")
        r = hk.send(hk.root_id, {"type": "delete_agent", "id": "gone"})
        need(not r.get("error"), f"delete_agent gone: {str(r)[:200]}")
        lt = hk.send("top", {"type": "work"})
        need(isinstance(lt.get("runs"), int), f"top answers before the stop: {lt}")

        # 2 · an agent reboot in one process — the store keeps its rows, the disk its files
        planted = asyncio.run(plant(hk))
        before = asyncio.run(ghost_rows(hk))
        need(len(before) == N, f"{len(before)}/{N} rows planted under ghost")
        r = hk.send(hk.root_id, {"type": "delete_agent", "id": "ghost",
                                   "keep_memory": True})
        need(not r.get("error"), f"delete_agent ghost: {str(r)[:200]}")
        kept = asyncio.run(ghost_rows(hk))
        need([x.get("id") for x in kept] == planted,
             f"after delete_agent ghost: {len(kept)}/{N} rows still in the store")
        create(hk, parent_id="rest", handler_module="slow_probe.tools", id="ghost")
        again = asyncio.run(ghost_rows(hk))
        need([x.get("id") for x in again] == planted
             and [x.get("content") for x in again] == [x.get("content") for x in before],
             f"ghost re-created on the same id: {len(again)}/{N} rows, same ids and content")
        r = hk.send(hk.root_id, {"type": "delete_agent", "id": "files"})
        need(not r.get("error"), f"delete_agent files: {str(r)[:200]}")
        create_files(hk)
        got = listed(hk)
        need(got == want_files, f"files re-created on the same root: {len(got & want_files)}/"
             f"{len(want_files)} files listed")

        led = hk.send("kernel", {"type": "ledger"}).get("executions") or []
        ids_before = max((e["id"] for e in led if isinstance(e.get("id"), int)), default=0)
        need(ids_before > 0, f"the ledger names executions before the stop (max id {ids_before})")

        # 3 · stop, boot the same directory
        stop(hk)
        on_disk = {p.parent.name for p in (work / ".fantastic").rglob("agent.yaml")}
        need({"top", "mid", "leaf", "files", "ghost"} <= on_disk and "gone" not in on_disk,
             f"records on disk after the stop: {sorted(on_disk)}")
        guid = disk_guid()
        need(guid is not None, f"the root has a guid in its state folder: {guid!r}")
        hk = boot()
        check_tree(hk, "reboot 1", ids_before)

        # 4 · again, nothing changed in between
        led = hk.send("kernel", {"type": "ledger"}).get("executions") or []
        ids_before = max((e["id"] for e in led if isinstance(e.get("id"), int)), default=0)
        stop(hk)
        g = disk_guid()
        need(g == guid, f"reboot 1: the root's guid is the same ({g} vs {guid})")
        hk = boot()
        check_tree(hk, "reboot 2", ids_before)
        stop(hk)
        g = disk_guid()
        need(g == guid, f"reboot 2: the root's guid is the same ({g} vs {guid})")
    finally:
        for k in kernels:
            k.close()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
