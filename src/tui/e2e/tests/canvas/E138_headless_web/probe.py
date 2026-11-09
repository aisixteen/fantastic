"""E138 headless_web — #560: a recorded `web` host listens in every mode.

After one `canvas` run (which records `web`, the canvas page and its socket leg),
`fantastic --headless` in the same folder serves them: the canvas page, a host call
through the socket, and a second agent's route (`static_files`) on the one host."""

from __future__ import annotations

import asyncio
import subprocess
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, http_get, verdict  # noqa: E402
from helpers import oslayer  # noqa: E402


def main() -> bool:
    from canvas import host_call

    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        # the first `canvas` records `web` and the canvas agents
        rig.start()
        rig.stop()
        nonce = f"headless-{uuid.uuid4().hex}"
        (rig.proj / "note.txt").write_text(nonce)
        rig.patch_host_agent("static_files", {"allow": ["note.txt"]})
        with open(rig.log, "a") as f:
            f.write("\n=== headless\n")
        rig.host = subprocess.Popen([str(rig.binary), "--headless"], cwd=rig.proj, env=rig.env(), stdout=open(rig.log, "a"),
                                    stderr=subprocess.STDOUT, **oslayer.group_kwargs())
        status, body = 0, ""
        for _ in range(150):
            if rig.host.poll() is not None:
                raise SystemExit(f"headless exited {rig.host.returncode}: {rig.log.read_text()[-800:]}")
            try:
                status, body = http_get(f"http://127.0.0.1:{rig.port}/canvas_page/file/index.html")
            except Exception:
                status = 0
            if status == 200:
                break
            time.sleep(0.2)
        info["page"] = [status, body[:80]]
        r["a1 headless serves the recorded web on its port"] = status == 200 and "<html" in body.lower()
        refl = asyncio.run(host_call(rig.port, "kernel", {"type": "reflect"}))
        info["reflect_id"] = refl.get("id") if isinstance(refl, dict) else refl
        r["a2 a declared route works: the canvas_ws socket"] = isinstance(refl, dict) and "error" not in refl and bool(refl.get("id"))
        try:
            got = http_get(f"http://127.0.0.1:{rig.port}/static_files/file/note.txt")
        except Exception as e:
            got = (0, str(e))
        info["note"] = list(got)
        r["b1 a second agent's route on the same host"] = got == (200, nonce)
        after = rig.log.read_text(errors="replace").split("=== headless")[-1]
        r["b2 the process is headless"] = "fantastic: the canvas:" not in after
    finally:
        rig.stop()
    ok = all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "all checks pass" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
