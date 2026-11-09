"""E142 bounded_stop — #562: a signal stop is bounded.

A stub model never answers, so a call is in flight at the stop.
a) SIGTERM: the kernel's clean stop runs (waits the quiet period), exits 0 inside
   the 15 s grace, the lock and the run file gone.
b) SIGTERM, then SIGTERM again: the second exits 1 at once, lock and run file gone."""

from __future__ import annotations

import json
import re
import socket
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict  # noqa: E402
from helpers import oslayer  # noqa: E402

E2E = Path(__file__).resolve().parents[6] / "e2e"


class _Rec:
    def __init__(self, root: Path) -> None:
        self.root = root

    def raw(self, rel: str, data: bytes) -> None:
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)

    def json(self, rel: str, obj) -> None:
        p = self.root / f"{rel}.json"
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(json.dumps(obj, default=str))


def main() -> bool:
    sys.path.insert(0, str(E2E / "parity"))
    from stubs.openai_chat import Handler, OpenAIChatStub  # noqa: E402

    rig = Rig(__file__, live=False)
    rig.build("vllm")
    rig.make_home(None)
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        stub_port = s.getsockname()[1]
    stub = OpenAIChatStub(_Rec(rig.art / "stub"), "stub", stub_port)
    posts: list[float] = []
    stub.responder = lambda req, raw: (posts.append(time.time()), {"stall": 120})[1]
    stub.start(Handler)
    r: dict = {}
    info: dict = {}
    senders: list[subprocess.Popen] = []
    lock = rig.proj / ".fantastic" / "lock.yaml"
    run = rig.proj / ".fantastic" / "cli" / "canvas.yaml"

    def in_flight(n: int) -> subprocess.Popen:
        p = subprocess.Popen([str(rig.binary), "send", json.dumps({"target": "ai", "payload": {"type": "complete", "rid": "r562", "model": "stub", "messages": [{"role": "user", "content": "hello"}]}})],
                             cwd=rig.proj, env=rig.env(), stdout=open(rig.art / "send.log", "a"), stderr=subprocess.STDOUT,
                             **oslayer.group_kwargs())
        senders.append(p)
        for _ in range(600):
            if len(posts) > n:
                break
            time.sleep(0.1)
        return p

    try:
        # first boot creates the root AI
        rig.host = subprocess.Popen([str(rig.binary), "canvas", "--port", str(rig.port), "ai", "vllm", "-p", str(stub_port)], cwd=rig.proj,
                                    env=rig.env(), stdout=open(rig.log, "a"), stderr=subprocess.STDOUT, **oslayer.group_kwargs())
        for _ in range(600):
            if rig.host.poll() is not None:
                raise SystemExit(f"the host exited {rig.host.returncode}: {rig.log.read_text()[-800:]}")
            if re.search(r"^fantastic: the canvas:", rig.log.read_text(), re.M):
                break
            time.sleep(0.2)
        else:
            raise SystemExit("the host never announced the canvas")
        rig.stop()
        rig.patch_host_agent("ai", {"model": "stub", "context_window": 32768, "max_tokens": 4096, "kv_slots": 4, "kv_overflow": "evict"})

        # a) a clean stop with a call in flight
        rig.start()
        r["a0 the lock and the run file exist while it runs"] = lock.exists() and run.exists()
        n = len(posts)
        s1 = in_flight(n)
        r["a1 the model call is in flight (a POST arrived)"] = len(posts) > n
        t0 = time.monotonic()
        oslayer.stop(rig.host)
        rc = rig.host.wait(30)
        dt = time.monotonic() - t0
        rig.host = None
        info["a"] = {"rc": rc, "dt": round(dt, 2)}
        r["a2 a clean stop exits 0"] = rc == 0
        r["a3 it waited the quiet period"] = dt >= 9.0
        r["a4 under the grace"] = dt < 15.0
        r["a5 the lock is released"] = not lock.exists()
        r["a6 the run file is removed"] = not run.exists()
        try:
            s1.wait(10)
        except subprocess.TimeoutExpired:
            s1.kill()

        # b) a second signal
        rig.start()
        n = len(posts)
        s2 = in_flight(n)
        r["b0 the model call is in flight (a POST arrived)"] = len(posts) > n
        oslayer.stop(rig.host)
        time.sleep(1.0)
        t1 = time.monotonic()
        oslayer.stop(rig.host)
        rc2 = rig.host.wait(30)
        dt2 = time.monotonic() - t1
        rig.host = None
        info["b"] = {"rc": rc2, "dt2": round(dt2, 2)}
        r["b1 a second signal exits 1"] = rc2 == 1
        r["b2 at once"] = dt2 < 3.0
        r["b3 the lock is released"] = not lock.exists()
        r["b4 the run file is removed"] = not run.exists()
        try:
            s2.wait(10)
        except subprocess.TimeoutExpired:
            s2.kill()
    finally:
        rig.stop()
        for p in senders:
            if p.poll() is None:
                p.kill()
        stub.stop()
    ok = all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "all checks pass" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
