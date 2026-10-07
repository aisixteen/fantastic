"""E45 run_service — start and stop a process through an exec agent."""

from __future__ import annotations

import asyncio
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, verdict, answer, ask, html_cell, http_get, in_frame, say, spot, wait_quiet  # noqa: E402,F401

def main() -> bool:
    rig = Rig(__file__)
    why = rig.plug_or_refusal()
    if why:
        return verdict(*rig.finish(False, why))
    rig.build()
    rig.make_home(rig.plug.record())

    (rig.proj / "site").mkdir()
    (rig.proj / "site" / "index.html").write_text("<p>E45-OK</p>")
    svc = {"id": "svc", "handler_module": "exec_bridge.tools", "parent_id": "canvas_root",
           "allowed": [r"^python3 -m http\.server [0-9]+ --directory site$"], "cwd": ".", "inherit_env": ["*"],
           "ingress_rule": "allow_all_ingress"}
    rig.init(host_agents=[(FOLDER, svc)])

    async def drive():
        async with rig.page() as pg:
            done, said = await ask(pg, 250, 300,
                "Using the exec agent `svc`, start a web server for the `site` directory on a free port, and tell me the port.",
                rig.budget(rig.limits["total_seconds"]), rig.limits["quiet_seconds"])
            ports = [int(p) for p in re.findall(r"(?<!\d)(\d{4,5})(?!\d)", said)]
            up = next((p for p in ports if "E45-OK" in http_get(f"http://127.0.0.1:{p}/")[1]), None)
            await say(pg, "Now stop that server.")
            await wait_quiet(pg, rig.budget(rig.limits["total_seconds"]), rig.limits["quiet_seconds"])
            await asyncio.sleep(2)
            down = up is not None and http_get(f"http://127.0.0.1:{up}/", 3)[0] == 0
            return said, up, down
    try:
        said, up, down = asyncio.run(drive())
    finally:
        rig.stop()
    rig.summary.update({"answer": said, "port_served": up, "stopped": down})
    print(f"served on {up} · stopped after 'stop it': {down}")
    return verdict(*rig.finish(up is not None and down, f"the reported port served the site: {up is not None} · stopped: {down}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
