"""E54 media — a served video and an AudioContext, as gaus_arta's final cuts and dawee's instruments."""

from __future__ import annotations

import asyncio
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict, answer, ask, html_cell, http_get, in_frame, say, spot, wait_quiet  # noqa: E402,F401

def main() -> bool:
    import subprocess
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    (rig.proj / "media").mkdir()
    r = subprocess.run(["ffmpeg", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=duration=4:size=320x180:rate=15",
                        "-c:v", "libvpx-vp9", "-b:v", "200k", str(rig.proj / "media" / "clip.webm")], capture_output=True)
    if r.returncode != 0:
        return verdict(*rig.finish(False, f"ffmpeg could not make the clip: {r.stderr[-300:]!r}"))
    files = {"id": "media_files", "handler_module": "file_bridge.tools", "parent_id": "web", "root": ".", "readonly": True,
             "allow": ["media/"], "ingress_rule": "allow_all_ingress"}
    rig.init(host_agents=[("agents/web", files)])
    base = rig.url.split("/canvas_page/")[0]
    rig.stop()
    rig.seed_cells([
        html_cell("video", "Video", 700, 60, f"<video id='v' src='{base}/media_files/file/media/clip.webm' muted autoplay loop playsinline style='width:100%'></video>", 360, 240),
        html_cell("audio", "Audio", 1100, 60, "<p id='s'>?</p><script>const a=new AudioContext();const o=a.createOscillator();const g=a.createGain();g.gain.value=0;o.connect(g).connect(a.destination);o.start();setInterval(()=>document.getElementById('s').textContent=a.state,300)</script>", 240, 160),
    ])
    rig.start()

    async def drive():
        async with rig.page(1600, 900) as pg:
            out = []
            for _ in range(2):
                await pg.until("document.querySelectorAll('.agent-frame iframe').length >= 2", 20)
                v = None
                for _ in range(rig.limits["seconds"] * 2):
                    v = await in_frame(pg, "video", "(()=>{const v=doc.getElementById('v');return v?{t:v.currentTime,w:v.videoWidth,err:v.error&&v.error.code}:null})()")
                    if v and v["t"] > 0.5 and v["w"] > 0:
                        break
                    await asyncio.sleep(0.5)
                s = await in_frame(pg, "audio", "doc.getElementById('s').textContent")
                out.append({"video": v, "audio": s})
                await pg.open(rig.url)
            return out
    try:
        seen = asyncio.run(drive())
    finally:
        rig.stop()
    rig.summary.update({"seen": seen})
    print(f"before / after reload: {seen}")
    ok = all(x["video"] and x["video"]["t"] > 0.5 and x["video"]["w"] > 0 and x["audio"] == "running" for x in seen)
    return verdict(*rig.finish(ok, f"video advancing with frames + AudioContext running, before and after a reload: {ok}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
