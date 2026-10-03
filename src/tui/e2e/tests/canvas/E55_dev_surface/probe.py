"""E55 dev_surface — the AI builds the reflective surface; E56's check scores it."""

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

    from _lib.canvas_live import free_port
    from _lib.dev_surface import exec_record, reactive_check, write_project
    port = free_port()
    write_project(rig.proj)
    rig.init(host_agents=[(FOLDER, exec_record(port))])
    ask_text = (f"Set up a workspace for this project using the exec agent `dev` — it may run exactly `sh build.sh` and "
                f"`python3 -m http.server {port} --directory dist`. I want: a controls panel with Build, Run and Stop buttons, "
                "a build log panel, a run log panel, a panel showing the running app, and a status panel that says "
                "built / running / stopped — all wired so they update by themselves as things happen.")

    async def drive():
        async with rig.page(1900, 1100) as pg:
            done, said = await ask(pg, 250, 300, ask_text, rig.limits["total_seconds"], rig.limits["quiet_seconds"])
            await asyncio.sleep(3)
            made = [c["id"] for c in rig.cells() if c.get("handler_module") in ("html_agent.ts", "terminal_view.ts")]
            checks = await reactive_check(pg, rig.proj, port)
            return said, made, checks
    try:
        said, made, checks = asyncio.run(drive())
    finally:
        rig.stop()
    rig.summary.update({"answer": said, "made": made, "checks": checks})
    print(f"made {len(made)} cells: {made}")
    print(f"reactive checks: {checks}")
    ok = len(made) >= 5 and all(checks.values())
    return verdict(*rig.finish(ok, f"{len(made)} cells · " + " ".join(f"{k}={'✓' if v else '✗'}" for k, v in checks.items())))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
