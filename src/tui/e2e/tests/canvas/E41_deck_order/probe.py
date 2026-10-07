"""E41 deck_order — read a many-panel deck (UA + EN twins), find its order and the gap."""

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

    import random
    random.seed(41)
    ua = {1: 'Титр і Земля', 2: 'Болванка', 3: 'Коілган', 4: 'Постріл', 6: 'Енергія', 7: 'Удар', 8: 'Підсумок'}
    en = {1: 'Title and Earth', 2: 'Penetrator', 3: 'Coilgun', 4: 'Fire', 6: 'Energy', 7: 'Impact', 8: 'Summary'}
    cells = []
    spots = [(700 + 330 * (i % 4), 60 + 260 * (i // 4)) for i in range(14)]
    random.shuffle(spots)
    for i, (n, t) in enumerate(sorted(ua.items())):
        cells.append(html_cell(f"ua_{n}", f"{n}. {t}", *spots[i], f"<h1>{n}. {t}</h1><p>слайд {n}</p>"))
    for i, (n, t) in enumerate(sorted(en.items())):
        cells.append(html_cell(f"en_{n}", f"EN:{n}. {t}", *spots[7 + i], f"<h1>{n}. {t}</h1><p>slide {n}</p>"))
    rig.init(cells=cells)

    async def drive():
        async with rig.page(1800, 1000) as pg:
            return await ask(pg, 250, 300, "What is this deck? In what order do the slides go, and is anything missing?",
                             rig.budget(rig.limits["total_seconds"]), rig.limits["quiet_seconds"])
    try:
        done, said = asyncio.run(drive())
    finally:
        rig.stop()
    looks = rig.looks_since_spawn()
    whole = any(x.startswith("web_loader call load_tree") for x in looks)  # every record at once
    looked = {c.split(" ")[0] for c in looks if c.endswith(" call reflect") or c.endswith(" call get_html")}
    rig.summary.update({"answer": said, "looks": looks})
    print(f"looks: {looks[:20]} · answer: {said[:500]}")
    gap = bool(re.search(r"(?<!\d)5(?!\d)", said))
    langs = bool(re.search(r"(?i)english|\ben\b", said)) and bool(re.search(r"(?i)ukrain|україн|\bua\b", said))
    ok = (len(looked) >= 8 or whole) and gap and langs
    return verdict(*rig.finish(ok, f"looked: {len(looked)} cells{' + the whole tree (load_tree)' if whole else ''} · names the missing 5: {gap} · both languages: {langs}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
