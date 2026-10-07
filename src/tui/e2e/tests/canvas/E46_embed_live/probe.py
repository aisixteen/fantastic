"""E46 embed_live — a panel framing a live URL, as dara's :3001/:8000/:3002."""

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

    import subprocess
    from _lib.canvas_live import free_port
    (rig.art / "srv").mkdir()
    (rig.art / "srv" / "index.html").write_text("<h1>E46 service</h1>")
    port = free_port()
    srv = subprocess.Popen([sys.executable, "-m", "http.server", str(port), "--directory", str(rig.art / "srv")],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    rig.init()
    url = f"http://127.0.0.1:{port}/"

    async def drive():
        from _lib.dev_surface import frame_urls
        async with rig.page() as pg:
            done, said = await ask(pg, 250, 300, f"Show the page at {url} in a panel next to you.",
                                   rig.budget(rig.limits["total_seconds"]), rig.limits["quiet_seconds"])
            await asyncio.sleep(3)
            return said, await frame_urls(pg)
    try:
        said, urls = asyncio.run(drive())
    finally:
        rig.stop()
        srv.terminate()
    # THE RECORD is the evidence (a sandboxed panel's inner frame can be hidden):
    # a cell whose html frames the URL; the live frame tree is reported too
    srcs = [m for c in rig.cells() if isinstance(c.get("html"), str)
            for m in re.findall(r"""src=["']([^"']+)["']""", c["html"]) if m.startswith(url.rstrip("/"))]
    live = any(u.startswith(url.rstrip("/")) for u in urls)
    rig.summary.update({"answer": said, "frames": urls, "srcs": srcs})
    print(f"panel srcs: {srcs} · live frames: {urls}")
    return verdict(*rig.finish(bool(srcs), f"a panel frames {url}: {bool(srcs)} (live in the frame tree: {live})"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
