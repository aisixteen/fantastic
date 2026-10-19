"""Run the home scenario (E24) — the home kernel, in-process in its test host,
managing real `fantastic canvas` processes. No model, no browser.

    cd src/lib/python && uv run python -u ../../../e2e/runners/home.py home

The host is `fantastic-home-host --fantastic <bin> --port P` (test-only: the
desktop app is the home's only real host). Every fantastic of the run — the ones
the home starts, and the one this runner starts itself — shares ONE home dir,
so they all write the registry the home reads. It is `HOME`, a level under the
throwaway dir importing `runners/canvas.py` makes, that does not exist yet: the
home kernel creates it private itself (`ensure_private_dir`), so the strict
check it then makes judges a dir the kernel made, never one the harness did.

Every step is checked through the home's own page socket (`user.forward` to
`home` over `/user/ws`), on the state stream (`/home/ws`), and on disk.
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

RUNNERS = Path(__file__).resolve().parent
sys.path.insert(0, str(RUNNERS))

import canvas  # noqa: E402 — sets FANTASTIC_HOME to its private throwaway dir
from canvas import _HOME, build_bin, build_home_host, free_port, host_call  # noqa: E402
from helpers import oslayer  # noqa: E402
from helpers.verdict import verdict  # noqa: E402

#: The run's home dir (see the module docs). A Windows dir the harness makes
#: private with `icacls` was read as "readable by others" by the kernel's check
#: on the CI runner; one the kernel creates carries the owner and DACL it judges.
HOME = _HOME / "home"
os.environ["FANTASTIC_HOME"] = str(HOME)
#: The home kernel's folder: its workdir and its state dir (no `.fantastic` inside).
W = HOME / "app"
ANNOUNCE = "fantastic-home-host: the home: "
CANVAS_ANNOUNCE = "fantastic: the canvas: "


def canonical(p: Path) -> str:
    """A folder as a kernel running there names it: links resolved on POSIX; on
    Windows absolute, as its `current_dir()` reports it."""
    return os.path.abspath(p) if oslayer.WINDOWS else os.path.realpath(p)


def yaml_of(p: Path) -> dict:
    import yaml

    return yaml.safe_load(oslayer.read_text(p)) or {}


def lock_pid(folder: Path) -> int | None:
    """The live pid in `folder`'s lock file (a clean stop removes the file)."""
    f = folder / ".fantastic" / "lock.yaml"
    try:
        pid = yaml_of(f).get("pid")
    except (FileNotFoundError, OSError):
        return None
    return pid if isinstance(pid, int) and oslayer.pid_alive(pid) else None


def registry_files() -> list[Path]:
    return sorted((HOME / "fantastics").glob("*/fantastic.yaml"))


def entry_of(path: str) -> dict | None:
    for f in registry_files():
        doc = yaml_of(f)
        if doc.get("path") == path:
            return {**doc, "_file": f}
    return None


def http_status(url: str) -> int:
    try:
        with urllib.request.urlopen(url, timeout=5) as r:
            return r.status
    except Exception as e:  # noqa: BLE001 — the status is the answer
        return getattr(e, "code", 0)


# ── the home host ─────────────────────────────────────────────────────────────
class HomeHost:
    def __init__(self, binary: Path, fantastic: Path, port: int, log: Path):
        self.binary, self.fantastic, self.port, self.log = binary, fantastic, port, log
        self.proc: subprocess.Popen | None = None
        self.url: str | None = None

    def start(self) -> None:
        with open(self.log, "a") as f:
            f.write("\n=== start\n")
        out = open(self.log, "a")
        self.proc = subprocess.Popen(
            [str(self.binary), "--fantastic", str(self.fantastic), "--port", str(self.port)],
            stdout=out, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, **oslayer.group_kwargs(),
        )
        deadline = time.time() + 60
        while time.time() < deadline:
            if self.proc.poll() is not None:
                # a refused home names its dir's access: print how the OS sees it
                access = "\n".join(oslayer.describe_access(p) for p in (_HOME, HOME, W) if p.exists())
                raise SystemExit(
                    f"the home host exited {self.proc.returncode}:\n{self.log.read_text()[-2000:]}\n"
                    f"── access of the home's levels:\n{access}"
                )
            for line in self.log.read_text().split("=== start")[-1].splitlines():
                if line.startswith(ANNOUNCE):
                    self.url = line[len(ANNOUNCE):].strip()
                    return
            time.sleep(0.1)
        raise SystemExit(f"the home host never announced its url:\n{self.log.read_text()[-2000:]}")

    def stop(self) -> int:
        assert self.proc is not None
        oslayer.stop(self.proc)
        try:
            code = self.proc.wait(30)
        except subprocess.TimeoutExpired:
            oslayer.kill_group(self.proc.pid)
            raise SystemExit("the home host did not stop when asked")
        self.proc = None
        return code


class Outside:
    """A fantastic this runner starts itself (`fantastic canvas`, in its console)."""

    def __init__(self, binary: Path, folder: Path, port: int, log: Path):
        self.binary, self.folder, self.port, self.log = binary, folder, port, log
        self.proc: subprocess.Popen | None = None

    def start(self) -> None:
        first = not (self.folder / ".fantastic").exists()
        with open(self.log, "a") as f:
            f.write("\n=== start\n")
        out = open(self.log, "a")
        self.proc = subprocess.Popen(
            [str(self.binary), "canvas", *(["--port", str(self.port)] if first else [])],
            cwd=self.folder, stdout=out, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, **oslayer.group_kwargs(),
        )
        deadline = time.time() + 60
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise SystemExit(f"fantastic canvas in {self.folder} exited {self.proc.returncode}:\n{self.log.read_text()[-2000:]}")
            if any(line.startswith(CANVAS_ANNOUNCE) for line in self.log.read_text().split("=== start")[-1].splitlines()):
                return
            time.sleep(0.1)
        raise SystemExit(f"fantastic canvas in {self.folder} never announced its canvas:\n{self.log.read_text()[-2000:]}")


# ── the state stream ──────────────────────────────────────────────────────────
class Stream:
    """Every `state` payload on `home`, from `ws://127.0.0.1:<port>/home/ws`."""

    def __init__(self, port: int):
        self.port, self.states, self.task = port, [], None

    async def open(self) -> None:
        import websockets

        ws = await websockets.connect(f"ws://127.0.0.1:{self.port}/home/ws")

        async def read():
            try:
                async for m in ws:
                    frame = json.loads(m)
                    p = frame.get("payload") or {}
                    if frame.get("type") == "event" and p.get("type") == "state":
                        self.states.append(p)
            except Exception:  # noqa: BLE001 — the host stopping closes it
                pass

        self.ws = ws
        self.task = asyncio.create_task(read())

    async def close(self) -> None:
        await self.ws.close()
        if self.task:
            await self.task

    def statuses(self, row_path: str) -> list[str]:
        out = []
        for s in self.states:
            for r in s.get("rows", []):
                if r.get("path") == row_path and (not out or out[-1] != r["status"]):
                    out.append(r["status"])
        return out


# ── the scenario ──────────────────────────────────────────────────────────────
class Run:
    def __init__(self, tmp: Path):
        self.tmp = tmp
        self.bin = build_bin(tmp)
        self.host_bin = build_home_host(tmp)
        self.port = free_port()
        self.host = HomeHost(self.host_bin, self.bin, self.port, tmp / "home-host.log")
        self.a = tmp / "A"
        self.b = tmp / "B"
        self.a.mkdir()
        self.b.mkdir()
        self.outside = Outside(self.bin, self.b, free_port(), tmp / "B.log")
        self.stream: Stream | None = None
        self.a_id: str | None = None
        self.b_id: str | None = None

    async def call(self, payload: dict) -> dict:
        return await host_call(self.port, "home", payload)

    async def until(self, what: str, ok, timeout: float) -> dict:
        deadline = time.time() + timeout
        state: dict = {}
        while time.time() < deadline:
            state = await self.call({"type": "list"})
            if state.get("type") == "state" and ok(state):
                return state
            await asyncio.sleep(0.25)
        raise AssertionError(f"{what}: not within {timeout:.0f} s — last state {json.dumps(state)[:1500]}")

    def row(self, state: dict, path: str) -> dict | None:
        return next((r for r in state.get("rows", []) if r.get("path") == path), None)

    async def open_stream(self) -> None:
        self.stream = Stream(self.port)
        await self.stream.open()

    # ── the runs ──
    async def boot(self, bad: list[str]) -> None:
        self.host.start()
        await self.open_stream()
        if (code := http_status(self.host.url)) != 200:
            bad.append(f"GET {self.host.url} answered {code}")
        state = await self.until("listed", lambda s: s["listed"], 15)
        if state["rows"] != []:
            bad.append(f"rows: {state['rows']}")
        run = yaml_of(W / "home.yaml")
        want = {"pid": self.host.proc.pid, "port": self.port, "url": self.host.url}
        if run != want:
            bad.append(f"home.yaml {run} != {want}")
        if not (W / "home.db").is_file():
            bad.append("no app/home.db")
        if (W / ".fantastic").exists():
            bad.append("a .fantastic is nested in app/")

    async def second_host_refused(self, bad: list[str]) -> None:
        r = subprocess.run([str(self.host_bin), "--fantastic", str(self.bin)], capture_output=True, text=True, timeout=60)
        if r.returncode != 2:
            bad.append(f"a second host exited {r.returncode}, not 2: {r.stderr[-500:]}")
        if f"already running (pid {self.host.proc.pid})" not in r.stderr:
            bad.append(f"a second host did not name the first's pid: {r.stderr[-500:]}")

    async def open_adds(self, bad: list[str]) -> None:
        a = canonical(self.a)
        reply = await self.call({"type": "open", "paths": [str(self.a)]})
        if reply.get("type") != "state":
            bad.append(f"open refused: {reply}")
            return
        state = await self.until("A running", lambda s: (r := self.row(s, a)) is not None and r["status"] == "running" and r["guid"], 30)
        row = self.row(state, a)
        self.a_id = row["id"]
        holder = lock_pid(self.a)
        if row["pid"] != holder:
            bad.append(f"row pid {row['pid']} != A's lock holder {holder}")
        if not row.get("canvas"):
            bad.append(f"no canvas on {row}")
        entry = entry_of(a)
        if entry is None:
            bad.append("no registry entry names A")
        else:
            if entry.get("pid") != holder or entry.get("canvas") != (row.get("canvas") or {}).get("url"):
                bad.append(f"the registry entry {entry} does not name pid {holder} and canvas {row.get('canvas')}")
            if entry["_file"].parent.name != row["guid"]:
                bad.append(f"the registry folder {entry['_file'].parent.name} is not the guid {row['guid']}")
        if not row.get("log") or not Path(row["log"]).is_file():
            bad.append(f"no launch log: {row.get('log')}")
        elif Path(row["log"]).parent != HOME / "logs":
            bad.append(f"the launch log is not in <home>/logs: {row['log']}")
        if state["pages"] != [self.a_id] or state["selection"] != self.a_id:
            bad.append(f"pages {state['pages']} selection {state['selection']}")

    async def stop(self, bad: list[str]) -> None:
        a = canonical(self.a)
        reply = await self.call({"type": "stop", "id": self.a_id})
        if reply.get("type") != "state":
            bad.append(f"stop refused: {reply}")
            return
        await self.until("A stopped", lambda s: (self.row(s, a) or {}).get("status") == "stopped", 20)
        seen = self.stream.statuses(a)
        if "stopping" not in seen or seen[-1] != "stopped" or seen.index("stopping") > len(seen) - 2:
            bad.append(f"A's statuses on the stream: {seen} (stopping, then stopped)")
        entry = entry_of(a) or {}
        if entry.get("pid") is not None or entry.get("canvas") is not None:
            bad.append(f"after a clean stop the entry names pid {entry.get('pid')} canvas {entry.get('canvas')}")
        if lock_pid(self.a) is not None or (self.a / ".fantastic" / "lock.yaml").exists():
            bad.append("A's lock is still held")
        if (self.a / ".fantastic" / "cli" / "canvas.yaml").exists():
            bad.append("A's run file is still there")

    async def start(self, bad: list[str]) -> None:
        a = canonical(self.a)
        reply = await self.call({"type": "start", "id": self.a_id})
        if reply.get("type") != "state":
            bad.append(f"start refused: {reply}")
            return
        await self.until("A running again", lambda s: (self.row(s, a) or {}).get("status") == "running", 30)

    async def outside_run(self, bad: list[str]) -> None:
        b = canonical(self.b)
        self.outside.start()
        r = await host_call(self.outside.port, "user_store", {"type": "register_home_folder", "value": True})
        if r.get("error"):
            bad.append(f"B's switch: {r}")
            return
        state = await self.until("B running", lambda s: (self.row(s, b) or {}).get("status") == "running", 30)
        self.b_id = self.row(state, b)["id"]
        reply = await self.call({"type": "stop", "id": self.b_id})
        if reply.get("type") != "state":
            bad.append(f"stop B refused: {reply}")
            return
        await self.until("B stopped", lambda s: (self.row(s, b) or {}).get("status") == "stopped", 20)
        try:
            self.outside.proc.wait(20)
        except subprocess.TimeoutExpired:
            bad.append("B's process did not exit")
        entry = entry_of(b) or {}
        if not entry or entry.get("pid") is not None or entry.get("canvas") is not None:
            bad.append(f"B's entry after its stop: {entry}")

    async def remove(self, bad: list[str]) -> None:
        b = canonical(self.b)
        self.outside.start()
        await self.until("B running again", lambda s: (self.row(s, b) or {}).get("status") == "running", 30)
        folder = (entry_of(b) or {}).get("_file")
        reply = await self.call({"type": "remove", "id": self.b_id})
        if reply.get("type") != "state":
            bad.append(f"remove refused: {reply}")
            return
        await self.until("B gone from the list", lambda s: self.row(s, b) is None, 20)
        if folder is not None and folder.parent.exists():
            bad.append(f"B's registry folder {folder.parent} is still there")
        if self.outside.proc.poll() is not None:
            bad.append(f"B stopped running ({self.outside.proc.returncode}) when it was removed")
        oslayer.stop_fantastic(self.bin, self.outside.proc.pid)
        try:
            self.outside.proc.wait(20)
        except subprocess.TimeoutExpired:
            bad.append("B did not stop when asked")

    async def own_folder(self, bad: list[str]) -> None:
        # the home's own folder is <home> itself, the parent of app/
        r = await self.call({"type": "open", "paths": [str(HOME)]})
        if r.get("reason") != "bad_arg":
            bad.append(f"opening the home's own folder: {r}")
        own = canonical(HOME)
        named = [f for f in registry_files() if yaml_of(f).get("path") == own]
        if named:
            bad.append(f"the registry names the home's own folder: {named}")

    async def restart_keeps_pages(self, bad: list[str]) -> None:
        a = canonical(self.a)
        state = await self.call({"type": "select", "id": self.a_id})
        if state.get("selection") != self.a_id:
            bad.append(f"select: {state}")
        await self.stream.close()
        code = self.host.stop()
        if code != 0:
            bad.append(f"the host exited {code}")
        if (W / "home.yaml").exists():
            bad.append("the run file is still there")
        if (W / "lock.yaml").exists():
            bad.append("the home's lock is still held")
        if lock_pid(self.a) is None:
            bad.append("A stopped with the host")
        self.host.start()
        await self.open_stream()
        state = await self.until("listed again", lambda s: s["listed"] and (self.row(s, a) or {}).get("status") == "running", 30)
        if state["pages"] != [self.a_id] or state["selection"] != self.a_id:
            bad.append(f"after a restart: pages {state['pages']} selection {state['selection']}")

    async def host_leaves_them_running(self, bad: list[str]) -> None:
        a = canonical(self.a)
        state = await self.call({"type": "list"})
        url = ((self.row(state, a) or {}).get("canvas") or {}).get("url")
        await self.stream.close()
        code = self.host.stop()
        if code != 0:
            bad.append(f"the host exited {code}")
        if lock_pid(self.a) is None:
            bad.append("A's lock is free after the host stopped")
        if not url or http_status(url) != 200:
            bad.append(f"A's canvas {url} does not answer")

    def teardown(self) -> None:
        if self.host.proc is not None:
            self.host.stop()
        if self.outside.proc is not None and self.outside.proc.poll() is None:
            oslayer.stop_fantastic(self.bin, self.outside.proc.pid)
        pids = [p for p in (lock_pid(self.a), lock_pid(self.b)) if p is not None]
        for pid in pids:
            oslayer.stop_fantastic(self.bin, pid)
        deadline = time.time() + 20
        while time.time() < deadline and any(oslayer.pid_alive(p) for p in pids):
            time.sleep(0.2)
        for pid in pids:
            if oslayer.pid_alive(pid):
                oslayer.kill_group(pid)


RUNS = {
    "boot": Run.boot,
    "second-host-refused": Run.second_host_refused,
    "open-adds": Run.open_adds,
    "stop": Run.stop,
    "start": Run.start,
    "outside": Run.outside_run,
    "remove": Run.remove,
    "own-folder": Run.own_folder,
    "restart-keeps-pages": Run.restart_keeps_pages,
    "host-leaves-them-running": Run.host_leaves_them_running,
}


async def run_all(sc: dict, tmp: Path) -> bool:
    r = Run(tmp)
    failures = 0
    try:
        for spec in sc["runs"]:
            name = spec["name"]
            bad: list[str] = []
            try:
                await RUNS[name](r, bad)
            except AssertionError as e:
                bad.append(str(e))
            print(f"  {'✓' if not bad else '✗'} {name}", flush=True)
            for b in bad:
                print(f"      {b}", flush=True)
            if bad:
                failures += 1
                break  # each run builds on the one before it
    finally:
        r.teardown()
        if failures:
            for log in (r.host.log, r.outside.log):
                if log.exists():
                    print(f"  ── {log.name} (tail)\n{log.read_text()[-3000:]}", flush=True)
    return failures == 0


def main(argv: list[str]) -> int:
    import yaml

    if not argv or argv[0].startswith("-"):
        raise SystemExit("usage: home.py <scenario-id>")
    sc = yaml.safe_load(canvas._scenario_path(argv[0]).read_text())
    print(f"E24_{sc['id']} · {sc['title']}", flush=True)
    unknown = [s["name"] for s in sc["runs"] if s["name"] not in RUNS]
    if unknown:
        raise SystemExit(f"the scenario names runs this runner does not know: {unknown}")
    tmp = Path(tempfile.mkdtemp(prefix="e2e-home-"))
    try:
        ok = asyncio.run(run_all(sc, tmp))
    finally:
        if not os.environ.get("E2E_KEEP"):
            shutil.rmtree(tmp, ignore_errors=True)
            shutil.rmtree(_HOME, ignore_errors=True)
        else:
            print(f"  kept {tmp} and {_HOME}")
    verdict(ok, "all runs green" if ok else "failures above")
    return 0 if ok else 1


if __name__ == "__main__":
    oslayer.ensure_utf8()
    sys.exit(main(sys.argv[1:]))
