"""E139 ws_route — #559: a `web_ws` leg may name a literal `route`.

After one `canvas` run (which records `web`, `canvas_ws` — the one catch-all leg,
`/user/ws` — and `user`), a second `web_ws` leg `page_ws` with `route: /page_ws/ws`
is seeded under the same `web`. `fantastic --headless` mounts both on one host:
the canvas's socket still connects and calls, and the literal route answers as
its own leg."""

from __future__ import annotations

import asyncio
import json
import secrets
import subprocess
import sys
import time
from pathlib import Path

import websockets
import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, http_get, verdict  # noqa: E402
from helpers import oslayer  # noqa: E402


async def leg_call(port: int, path: str, target: str, payload: dict) -> dict:
    """A call on the socket at `path`, the answer to THIS frame's id."""
    frame = {"type": "call", "target": target, "payload": payload, "id": secrets.token_hex(6)}
    async with websockets.connect(f"ws://127.0.0.1:{port}{path}", origin=f"http://127.0.0.1:{port}") as ws:
        await ws.send(json.dumps(frame))
        while True:
            m = json.loads(await asyncio.wait_for(ws.recv(), 10))
            if m.get("type") in ("reply", "error") and m.get("id") == frame["id"]:
                return m.get("data", m)


def main() -> bool:
    from canvas import host_call

    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        # the first `canvas` records `web`, `canvas_ws` and `user`
        rig.start()
        rig.stop()
        base = rig.proj / ".fantastic"
        web_yaml = next(
            f for f in base.rglob("web/agent.yaml") if (yaml.safe_load(oslayer.read_text(f)) or {}).get("handler_module") == "web.tools"
        )
        parent_path = web_yaml.parent.relative_to(base)
        rig.seed_host_agent(
            str(parent_path),
            {
                "id": "page_ws",
                "parent_id": "web",
                "handler_module": "web_ws.tools",
                "route": "/page_ws/ws",
                "ingress_rule": "allow_all_ingress",
                "callable_by": {"web": ["get_routes"]},
            },
        )
        cc = dict(rig.host_record("web").get("can_call") or {})
        cc["page_ws"] = ["get_routes"]
        rig.patch_host_agent("web", {"can_call": cc})
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
        r["a1 web boots with two web_ws legs"] = status == 200
        refl = asyncio.run(host_call(rig.port, "kernel", {"type": "reflect"}))
        info["reflect_id"] = refl.get("id") if isinstance(refl, dict) else refl
        r["a2 the canvas's catch-all socket works"] = isinstance(refl, dict) and "error" not in refl and bool(refl.get("id"))
        try:
            data = asyncio.run(leg_call(rig.port, "/page_ws/ws", "page_ws", {"type": "reflect"}))
        except Exception as e:
            data = {"error": repr(e)}
        info["page_ws"] = data
        r["b1 the literal route answers as its own leg"] = (
            isinstance(data, dict) and data.get("id") == "page_ws" and data.get("path_pattern") == "/page_ws/ws"
        )
        after = rig.log.read_text(errors="replace").split("=== headless")[-1]
        r["b2 no catch-all refusal in the log"] = "a second catch-all" not in after
    finally:
        rig.stop()
    ok = all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "all checks pass" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
