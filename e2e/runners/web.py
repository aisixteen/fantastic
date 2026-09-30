"""Run a browser-View scenario against the `fantastic` binary — a SCRIPTED client.

`fantastic web` serves the page over the kernel's own `web` host and every
browser frame runs as the `web_ui` agent. This runner is the browser: it
fetches the page, opens the socket with the host's own Origin, `watch`es the
`ui` agent, sends intents (`resize`, `key`) and rebuilds the screen from the
frames the ONE ViewModel publishes. No model, no browser engine.

    uv run python e2e/runners/web.py web                   # build src/tui, use it
    uv run python e2e/runners/web.py web --bin /path/fantastic

A run with `tui: true` also drives the terminal View through the shell's PTY rig
(`src/tui/e2e/screenshot.rs`), at the same time, on the same directory — the
check that a key in either View moves both.
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers import verdict  # noqa: E402

# The binary's user-home template store (`user_home_store`) reads FANTASTIC_HOME,
# else ~/.config/fantastic. A test must never read or write the owner's real
# home: every binary this runner launches inherits a throwaway one.
os.environ["FANTASTIC_HOME"] = tempfile.mkdtemp(prefix="fc-e2e-home-")

REPO = E2E.parent
TUI = REPO / "src" / "tui"
TARGET = REPO / "src" / "target" / "debug"


def _scenario_path(scenario_id: str) -> Path:
    for d in sorted((E2E / "tests" / "scenario").iterdir()):
        if d.is_dir() and d.name.split("_", 1)[-1] == scenario_id:
            return d / "scenario.yaml"
    raise SystemExit(f"no scenario {scenario_id!r}")


def build(what: list[str]) -> None:
    r = subprocess.run(["cargo", "build", "--manifest-path", str(TUI / "Cargo.toml"), *what], capture_output=True, text=True)
    if r.returncode != 0:
        print(r.stderr[-2000:])
        raise SystemExit(f"cargo build {' '.join(what)} failed")


def resolve(argv: list[str], into: Path) -> tuple[Path, Path]:
    import shutil

    if "--bin" in argv:
        b = Path(argv[argv.index("--bin") + 1]).expanduser().resolve()
    elif os.environ.get("FANTASTIC_BIN"):
        b = Path(os.environ["FANTASTIC_BIN"]).expanduser().resolve()
    else:
        build([])
        b = TARGET / "fantastic"
    build(["--example", "screenshot"])
    out = []
    for p in (b, TARGET / "examples" / "screenshot"):
        dst = into / p.name
        shutil.copy2(p, dst)
        out.append(dst)
    return out[0], out[1]


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Browser:
    """The page's protocol, scripted: watch `ui`, send intents, rebuild frames."""

    def __init__(self, port: int):
        self.port, self.rows, self.seq, self.frames, self.n = port, {}, 0, 0, 0
        self.replies: list[dict] = []
        self.closed: str | None = None
        import secrets

        self.sid = secrets.token_hex(8)

    async def open(self):
        import websockets

        self.ws = await websockets.connect(f"ws://127.0.0.1:{self.port}/web_ui/ws", origin=f"http://127.0.0.1:{self.port}")
        await self.ws.send(json.dumps({"type": "watch", "src": "ui"}))

    async def call(self, payload: dict):
        # Ids are unique per session, as the page mints them: the frame id is
        # the execution id, and a repeated one returns the recorded reply.
        self.n += 1
        await self.ws.send(json.dumps({"type": "call", "target": "ui", "payload": payload, "id": f"{self.sid}:{self.n}"}))

    async def pump(self, secs: float):
        loop = asyncio.get_event_loop()
        end = loop.time() + secs
        while (left := end - loop.time()) > 0:
            try:
                m = json.loads(await asyncio.wait_for(self.ws.recv(), left))
            except asyncio.TimeoutError:
                return
            except Exception as e:  # the socket went away: say so, do not crash
                self.closed = f"{type(e).__name__}: {e}"
                return
            f = m.get("payload") if m.get("type") == "event" else m.get("data") if m.get("type") == "reply" else None
            if isinstance(f, dict) and f.get("type") == "frame":
                if m.get("type") == "event" and not f["full"] and (self.seq == 0 or f["seq"] != self.seq + 1):
                    await self.call({"type": "state"})  # a gap: resync, as the page does
                    continue
                if f["full"]:
                    self.rows = {}
                for y, runs in f["rows"].items():
                    self.rows[int(y)] = "".join(r["t"] for r in runs)
                self.seq, self.frames = f["seq"], self.frames + 1
            elif m.get("type") == "reply":
                self.replies.append(m.get("data") or {})

    def text(self) -> str:
        return "\n".join(self.rows.get(y, "") for y in range(max(self.rows, default=-1) + 1))


def check(label: str, text: str, want: dict) -> list[str]:
    bad = [f"{label}: lacks {w!r}" for w in want.get("contains", []) if w not in text]
    bad += [f"{label}: shows {w!r}" for w in want.get("lacks", []) if w in text]
    return bad


def start(binary: Path, cwd: Path, args: list[str]) -> tuple[subprocess.Popen, str]:
    p = subprocess.Popen([str(binary), *args], cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    line = p.stdout.readline().strip()  # the announce line: the URL, or why not
    return p, line


def stop(p: subprocess.Popen, cwd: Path) -> list[str]:
    bad = []
    if p.poll() is None:
        p.send_signal(signal.SIGTERM)
        try:
            p.wait(10)
        except subprocess.TimeoutExpired:
            p.kill()
            bad.append("did not exit within 10 s of SIGTERM")
    if p.returncode not in (0, None):
        bad.append(f"exit {p.returncode}")
    if (cwd / ".fantastic" / "lock.yaml").exists():
        bad.append("lock.yaml left behind")
    return bad


async def headless(run: dict, binary: Path, cwd: Path, port: int) -> list[str]:
    want = run.get("port")
    args = ["--headless", "web", *(["--port", str(port + 1 if want == "other" else port)] if want else [])]
    p, line = start(binary, cwd, args)
    bad = []
    if exp := run.get("refused"):
        p.wait(10)
        if p.returncode == 0 or exp not in line:
            bad.append(f"expected a refusal naming {exp!r}; got exit {p.returncode}: {line!r}")
        return bad
    if f"http://127.0.0.1:{port}/" not in line:
        return [f"no URL announced: {line!r}"] + stop(p, cwd)
    page = urllib.request.urlopen(line.split("View: ", 1)[1]).read().decode()
    if "web_ui/ws" not in page:
        bad.append("the page does not open the web_ui socket")
    b = Browser(port)
    await b.open()
    await b.call({"type": "reflect"})
    await b.call({"type": "resize", "w": 110, "h": 32})
    await b.pump(1.0)
    await b.call({"type": "state"})  # as the page does: the whole frame, then diffs
    await b.pump(1.5)
    if not any(r.get("sized_by") == "web" for r in b.replies):
        bad.append("ui does not say the browser sizes it")
    bad += check("first", b.text(), run.get("first", {}))
    for k in run.get("keys", []):
        await b.call({"type": "key", "code": "Char", "ch": k} if len(k) == 1 else {"type": "key", "code": k})
        await b.pump(0.4)
    await b.pump(1.0)
    bad += check("after keys", b.text(), run.get("after", {}))
    await b.ws.close()
    return bad + stop(p, cwd)


async def both(run: dict, binary: Path, rig: Path, cwd: Path, port: int, tmp: Path) -> list[str]:
    # The terminal View in the rig; the browser View here; one ViewModel.
    script = tmp / f"{run['name']}.script"
    script.write_text(run["script"])
    frames = tmp / f"frames-{run['name']}"
    env = {**os.environ, "FT_CWD": str(cwd), "FT_ROWS": "32", "FT_COLS": "110", "FT_ARGS": run.get("args", "web")}
    r = subprocess.Popen([str(rig), str(binary), str(script), str(frames)], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    bad = []
    for _ in range(100):
        try:
            socket.create_connection(("127.0.0.1", port), 0.2).close()
            break
        except OSError:
            time.sleep(0.1)
    b = Browser(port)
    await b.open()
    await b.call({"type": "reflect"})
    await b.call({"type": "state"})
    await b.pump(1.0)
    if not any(x.get("sized_by") == "tui" for x in b.replies):
        ps = subprocess.run(["pgrep", "-fl", "fantastic"], capture_output=True, text=True).stdout.strip()
        got = [(x.get("sized_by"), x.get("pid")) for x in b.replies if "sized_by" in x]
        bad.append(f"with a terminal attached, ui must say the terminal sizes it; got {got}; processes: {ps!r}")
    for step in run["browser"]:
        await b.pump(step.get("wait", 0) / 1000)
        if "key" in step:
            k = step["key"]
            await b.call({"type": "key", "code": "Char", "ch": k} if len(k) == 1 else {"type": "key", "code": k})
        if "expect" in step:
            await b.pump(0.6)
            bad += check(f"browser {step['expect'].get('label', '?')}", b.text(), step["expect"])
    if b.closed:
        bad.append(f"browser: the socket closed early ({b.closed})")
    await b.ws.close()
    out, _ = r.communicate(timeout=60)
    for label, want in (run.get("frames") or {}).items():
        hits = sorted(frames.glob(f"*_{label}.txt"))
        bad += [f"no terminal frame {label!r}"] if not hits else check(f"terminal {label}", hits[-1].read_text(), want)
    if "exit: 0" not in out:
        bad.append(f"terminal: {out.strip().splitlines()[-1:]}")
    if (cwd / ".fantastic" / "lock.yaml").exists():
        bad.append("lock.yaml left behind")
    return bad


async def stream(run: dict, binary: Path, cwd: Path) -> list[str]:
    """`fantastic --headless`: no screen, the kernel's message stream on stdout."""
    p = subprocess.Popen([str(binary), "--headless"], cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    await asyncio.sleep(run.get("secs", 3))
    p.send_signal(signal.SIGTERM)
    out, _ = p.communicate(timeout=15)
    bad = check("stream", out, run.get("stream", {}))
    if p.returncode != 0:
        bad.append(f"exit {p.returncode}")
    if (cwd / ".fantastic" / "lock.yaml").exists():
        bad.append("lock.yaml left behind")
    return bad


def main(argv: list[str]) -> int:
    import yaml

    sc = yaml.safe_load(_scenario_path(argv[0]).read_text())
    failures: list[str] = []
    with tempfile.TemporaryDirectory(prefix="e2e-web-") as t:
        tmp = Path(t)
        binary, rig = resolve(argv[1:], tmp)
        print(f"{sc['id']} · {sc.get('title')}\n  binary: {binary}", flush=True)
        cwd = tmp / "work"
        cwd.mkdir()
        port = free_port()
        for run in sc["runs"]:
            job = (both(run, binary, rig, cwd, port, tmp) if run.get("tui")
                   else stream(run, binary, cwd) if run.get("stream") else headless(run, binary, cwd, port))
            bad = asyncio.run(job)
            print(f"  {'✓' if not bad else '✕'} {run['name']}" + ("" if not bad else "  — " + " · ".join(bad)), flush=True)
            failures += [f"{run['name']}: {b}" for b in bad]
    ok = not failures
    verdict(ok, "all runs green" if ok else f"{len(failures)} failure(s)")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
