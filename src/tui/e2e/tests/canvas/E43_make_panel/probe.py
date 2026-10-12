"""E43 make_panel — author a new html cell that computes."""

from __future__ import annotations

import asyncio
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict, answer, ask, html_cell, http_get, in_frame, say, spot, wait_quiet  # noqa: E402,F401

def main() -> bool:
    rig = Rig(__file__)
    why = rig.plug_or_refusal()
    if why:
        return verdict(*rig.finish(False, why))
    rig.build()
    rig.make_home(rig.plug.record())

    rig.init()

    async def drive():
        async with rig.page() as pg:
            done, said = await ask(pg, 250, 300,
                "Make me a small calculator panel next to you: kinetic energy in joules from mass (kg) and speed (m/s).",
                rig.budget(rig.limits["total_seconds"]), rig.limits["quiet_seconds"])
            await asyncio.sleep(2)
            made = [c["id"] for c in rig.cells() if c.get("handler_module") == "html_agent.ts"]
            # drive it in WHATEVER frame holds it — through CDP (an AI need not mark
            # its panel trusted, and a sandboxed panel is closed to the page's DOM)
            from _lib.dev_surface import frame_eval
            drove = await frame_eval(pg, """(()=>{const i=[...document.querySelectorAll('input')]; if(i.length<2) return false;
                const set=(e,v)=>{e.value=v; e.dispatchEvent(new Event('input',{bubbles:true})); e.dispatchEvent(new Event('change',{bubbles:true}))};
                set(i[0],'2'); set(i[1],'3'); const b=document.querySelector('button'); if(b) b.click(); return true})()""")
            got = None
            if any(drove):
                await asyncio.sleep(1)
                texts = [t for t in await frame_eval(pg, "document.querySelectorAll('input').length>=2 ? document.body.innerText : null") if t]
                got = texts[0] if texts else None
            return said, made, got
    try:
        said, made, got = asyncio.run(drive())
    finally:
        rig.stop()
    rig.summary.update({"answer": said, "made": made, "panel_text_after_2kg_3ms": got})
    print(f"made {made} · panel after m=2, v=3: {str(got)[:200]!r}")
    ok = bool(made) and got is not None and bool(re.search(r"(?<![\d.])9(?:\.0+)?(?![\d])", got or ""))
    return verdict(*rig.finish(ok, f"{len(made)} panel(s) made · 2 kg at 3 m/s shows 9 J: {ok}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
