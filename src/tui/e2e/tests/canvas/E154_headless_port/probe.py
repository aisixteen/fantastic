"""E154 headless_port — #580: `--port` and `--host` with `--headless`; a port-less `web` picks one.

One `canvas` run records `web` (and the canvas page it serves). Then, in the same folder:
a) `--headless --port N --host 127.0.0.1` (N another port): the recorded `web` serves the page on N and its
   record names N.
b) `--headless` again, no flags: it serves on N again (the record is reused).
c) the recorded `web` loses its `port` (stopped): `--headless` picks a free port, records it and serves on it;
   the next `--headless` serves on the same one."""

from __future__ import annotations

import subprocess
import sys
import time
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, http_get, verdict  # noqa: E402
from canvas import free_port  # noqa: E402
from helpers import oslayer  # noqa: E402

PAGE = "/canvas_page/file/index.html"


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    web = rig.proj / ".fantastic" / "agents" / "user" / "agents" / "web" / "agent.yaml"
    r: dict = {}
    info: dict = {}

    def recorded() -> int | None:
        try:
            return (yaml.safe_load(oslayer.read_text(web)) or {}).get("port")
        except (OSError, ValueError):
            return None

    def headless(*flags: str, want: int | None) -> tuple[int | None, bool]:
        """Run `--headless` and wait for the page: on `want`, or on whatever the record then names.
        Returns (the port that served, whether it served)."""
        with open(rig.log, "a") as f:
            f.write(f"\n=== headless {' '.join(flags)}\n")
        rig.host = subprocess.Popen([str(rig.binary), "--headless", *flags], cwd=rig.proj, env=rig.env(), stdout=open(rig.log, "a"),
                                    stderr=subprocess.STDOUT, **oslayer.group_kwargs())
        try:
            for _ in range(150):
                if rig.host.poll() is not None:
                    raise SystemExit(f"headless exited {rig.host.returncode}: {rig.log.read_text()[-800:]}")
                port = want or recorded()
                if port:
                    try:
                        status, body = http_get(f"http://127.0.0.1:{port}{PAGE}")
                    except Exception:
                        status, body = 0, ""
                    if status == 200 and "<html" in body.lower():
                        return port, True
                time.sleep(0.2)
            return want or recorded(), False
        finally:
            rig.stop()

    try:
        # the first `canvas` records `web` and the canvas page
        rig.start()
        rig.stop()
        first = rig.port
        n = free_port()
        info["canvas_port"], info["N"] = first, n
        # a) the flags move the recorded web onto N
        got, served = headless("--port", str(n), "--host", "127.0.0.1", want=n)
        r["a1 --headless --port N --host 127.0.0.1 serves the recorded web's page on N"] = served and got == n
        r["a2 the record names N"] = recorded() == n
        # b) a restart without flags reuses N
        got, served = headless(want=None)
        r["b1 a restart with no flags serves on N again"] = served and got == n
        r["b2 the record still names N"] = recorded() == n
        # c) a web with no port picks a free one, once
        rec = yaml.safe_load(oslayer.read_text(web)) or {}
        rec.pop("port", None)
        web.write_text(yaml.safe_dump(rec, sort_keys=False))
        got, served = headless(want=None)
        picked = recorded()
        info["picked"] = picked
        r["c1 a port-less web picks a port, records it and serves on it"] = served and bool(picked) and got == picked
        got2, served2 = headless(want=None)
        r["c2 the next start reuses the picked port"] = served2 and got2 == picked and recorded() == picked
    finally:
        rig.stop()
    ok = all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "all checks pass" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
