"""E47 pdf_panel — served documents through a deny-all file leg."""

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

    pdf = '%PDF-1.1\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 144]>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n'
    for rel in ("docs/prd.pdf", "secret/plan.pdf"):
        (rig.proj / rel).parent.mkdir(parents=True, exist_ok=True)
        (rig.proj / rel).write_text(pdf)
    files = {"id": "files", "handler_module": "file_bridge.tools", "parent_id": "web", "root": ".", "readonly": True,
             "allow": ["docs/"], "ingress_rule": "allow_all_ingress"}
    rig.init(host_agents=[("agents/web", files)])
    base = rig.url.split("/canvas_page/")[0]

    async def drive():
        from _lib.dev_surface import frame_urls
        async with rig.page() as pg:
            done, said = await ask(pg, 250, 300,
                "Open the PRD in a panel next to you — it is docs/prd.pdf, served by the host's `files` agent.",
                rig.limits["total_seconds"], rig.limits["quiet_seconds"])
            await asyncio.sleep(3)
            embeds = await pg.js("[...document.querySelectorAll('.agent-frame iframe')].flatMap(f=>{try{return [...f.contentDocument.querySelectorAll('iframe,embed,object')].map(e=>e.src||e.data||'')}catch(e){return []}})")
            # WHILE the host runs: does each framed URL really serve the PDF?
            srcs = [m for c in rig.cells() if isinstance(c.get("html"), str)
                    for m in re.findall(r"""(?:src|data)=["']([^"']*prd\.pdf[^"']*)""", c["html"])]
            served = [u for u in srcs if http_get(u if u.startswith("http") else base + u)[1].startswith("%PDF")]
            # or the PDF's own bytes, embedded (`data:application/pdf;base64,…`) —
            # read through the file leg, so the leg still decided what was served
            import base64
            pdf = (rig.proj / "docs" / "prd.pdf").read_bytes()
            for c in rig.cells():
                for d in re.findall(r"data:application/pdf;base64,([A-Za-z0-9+/=]+)", str(c.get("html") or "")):
                    if base64.b64decode(d) == pdf:
                        served.append(f"data:application/pdf (the bytes of docs/prd.pdf, in {c['id']})")
            return said, await frame_urls(pg), embeds or [], srcs, served
    try:
        said, urls, embeds, srcs, served = asyncio.run(drive())
    finally:
        rig.stop()
    # THE RECORD is the evidence: a cell whose html frames / embeds the PDF, at a
    # URL that really serves it (a sandboxed panel's inner frame is not visible
    # to the page or the frame tree)
    painted = any("prd.pdf" in u for u in [*urls, *embeds])
    trusted = [c["id"] for c in rig.cells() if c.get("trusted") and "prd.pdf" in str(c.get("html"))]
    rig.summary.update({"answer": said, "frames": urls, "embeds": embeds, "srcs": srcs, "served": served, "trusted": trusted})
    print(f"panel srcs: {srcs} · served as PDF: {served} · visible in the frame tree: {painted} · trusted: {trusted}")
    return verdict(*rig.finish(bool(served), f"a panel frames docs/prd.pdf at a URL serving it: {bool(served)} (trusted, so the PDF paints: {bool(trusted)})"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
