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
    # the same surface in fewer panels (owner 2026-10-06: less filler, same
    # complexity): buttons drive `dev`, its events update the page by themselves
    ask_text = (f"Using the exec agent `dev` (it may run exactly `sh build.sh` and `python3 -m http.server {port} --directory dist`), "
                "build: a panel with Build, Run and Stop buttons, a live log and a status line (built / running / stopped), "
                "and a panel showing the running app — all updating by themselves.")

    async def drive():
        async with rig.page(1900, 1100) as pg:
            # the AI's time: its limit, less what the reactive checks need (≈130 s)
            done, said = await ask(pg, 250, 300, ask_text, rig.budget(rig.limits["total_seconds"], reserve=150), rig.limits["quiet_seconds"])
            await asyncio.sleep(3)
            made = [c["id"] for c in rig.cells() if c.get("handler_module") in ("html_agent.ts", "agent_view.ts", "terminal_view.ts")]
            checks = await reactive_check(pg, rig.proj, port)
            return said, made, checks
    try:
        said, made, checks = asyncio.run(drive())
    finally:
        rig.stop()
    rig.summary.update({"answer": said, "made": made, "checks": checks})
    print(f"made {len(made)} cells: {made}")
    print(f"reactive checks: {checks}")
    ok = len(made) >= 2 and all(checks.values())
    return verdict(*rig.finish(ok, f"{len(made)} cells · " + " ".join(f"{k}={'✓' if v else '✗'}" for k, v in checks.items())))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
