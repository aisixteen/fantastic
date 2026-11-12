"""E153 launcher_seed — #555: the LAUNCHER seeds a fresh state folder, then the kernel starts.

a) each kernel-starting mode (`--headless`, `canvas`, `mcp`) started in a fresh folder: the stock tree
   is there and the kernel holds the lock.
b) a one-shot command (no argument and no terminal, `send`, `import`, a refused command line)
   writes nothing: no `.fantastic` in the folder.
c) the test-only embedder (`examples/embedder.rs`: the launcher plus ONE extra blueprint, public API
   only), `--headless --statedir <far> --workdir <work>` run from another folder: the stock tree and the
   embedder's record are in the state folder and in the kernel's live `list_agents`; nothing is
   written in the cwd or the work folder (the check fails when the blueprint pass is skipped or the seed
   goes to the cwd)."""

from __future__ import annotations

import json
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict  # noqa: E402
from canvas import free_port  # noqa: E402
from helpers import oslayer  # noqa: E402

#: The stock tree's records, under the state folder.
STOCK = [
    "agents/user/agent.yaml",
    "agents/user/agents/exec/agent.yaml",
    "agents/user/agents/files/agent.yaml",
    "agents/user/agents/store/agent.yaml",
    "agents/user/agents/memory/agent.yaml",
]
EXTRA = "agents/user/agents/embedder_store/agent.yaml"


def fresh(rig: Rig, name: str) -> Path:
    d = rig.art / name
    d.mkdir()
    return d


def has_all(state: Path, rels: list[str]) -> bool:
    return all((state / r).is_file() for r in rels)


def start(rig: Rig, binary: Path, argv: list[str], cwd: Path, log: Path) -> subprocess.Popen:
    return subprocess.Popen([str(binary), *argv], cwd=cwd, env=rig.env(), stdin=subprocess.PIPE, stdout=open(log, "a"),
                            stderr=subprocess.STDOUT, **oslayer.group_kwargs())


def until(pred, secs: float = 60.0, proc: subprocess.Popen | None = None) -> bool:
    end = time.time() + secs
    while time.time() < end:
        if pred():
            return True
        if proc is not None and proc.poll() is not None:
            return pred()
        time.sleep(0.2)
    return False


def end(proc: subprocess.Popen) -> None:
    oslayer.stop(proc)
    try:
        proc.wait(30)
    except subprocess.TimeoutExpired:
        proc.kill()


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build(example="embedder")
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    # a) each kernel-starting mode seeds a fresh folder
    for kind, argv in [("headless", ["--headless"]), ("canvas", ["canvas", "--port", str(free_port())]), ("mcp", ["mcp"])]:
        cwd = fresh(rig, f"mode_{kind}")
        proc = start(rig, rig.binary, argv, cwd, rig.art / f"mode_{kind}.log")
        try:
            seeded = until(lambda: has_all(cwd / ".fantastic", STOCK) and (cwd / ".fantastic" / "lock.yaml").is_file(), 60, proc)
        finally:
            end(proc)
        r[f"a {kind}: a fresh folder gets the stock tree, written by the launcher, and the kernel runs on it"] = seeded
    # b) a one-shot writes nothing
    one_shots = {
        "no argument and no terminal": ([], 0),
        "send": (["send", json.dumps({"target": "kernel", "payload": {"type": "reflect"}})], 1),
        "import": (["import", "p", "--parent", "user"], None),
        "a refused mcp line": (["mcp", "--can-call", "files"], 2),
        "an unknown command": (["bogus"], 2),
    }
    for name, (argv, code) in one_shots.items():
        cwd = fresh(rig, "one_" + name.replace(" ", "_"))
        p = subprocess.run([str(rig.binary), *argv], cwd=cwd, env=rig.env(), capture_output=True, text=True, timeout=120, stdin=subprocess.DEVNULL)
        info[f"one-shot {name}"] = p.returncode
        ok = not (cwd / ".fantastic").exists() and (code is None or p.returncode == code)
        if name == "no argument and no terminal":
            ok = ok and bool(p.stdout.strip())
        r[f"b {name}: writes nothing to a fresh folder"] = ok
    # c) the embedder: its blueprint, the far state folder, the work folder
    cwd, work, state = fresh(rig, "emb_cwd"), fresh(rig, "emb_work"), rig.art / "emb_far" / "state"
    state.parent.mkdir()
    proc = start(rig, rig.example, ["--headless", "--statedir", str(state), "--workdir", str(work)], cwd, rig.art / "embedder.log")
    try:
        up = until(lambda: has_all(state, [*STOCK, EXTRA]) and (state / "lock.yaml").is_file() and (state / "cli" / "mcp.yaml").is_file(), 60, proc)
        r["c1 the stock tree and the embedder's record are in the state folder; the kernel runs on it"] = up
        listed: list = []
        if up:
            q = subprocess.run([str(rig.example), "send", "--statedir", str(state),
                                json.dumps({"target": "kernel_state", "payload": {"type": "list_agents"}})],
                               cwd=cwd, env=rig.env(), capture_output=True, text=True, timeout=60)
            try:
                listed = sorted(a["id"] for a in json.loads(q.stdout)["result"]["agents"])
            except (ValueError, KeyError, TypeError):
                info["send"] = (q.stdout + q.stderr)[-400:]
        info["agents"] = listed
        r["c2 the live list_agents holds the stock agents and the embedder's"] = {"files", "exec", "store", "memory", "embedder_store"} <= set(listed)
    finally:
        end(proc)
    r["c3 no .fantastic in the cwd or the work folder"] = not (cwd / ".fantastic").exists() and not (work / ".fantastic").exists()
    ok = all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "all checks pass" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
