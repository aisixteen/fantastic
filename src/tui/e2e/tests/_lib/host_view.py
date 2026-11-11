"""Read the page's inline read-only views (store, scheduler): their rows as text.

The views mount straight into a canvas cell (not an iframe) as `.ro-view.<name> .body`,
one `div` per row; the header carries the `⟳ refresh` button."""

from __future__ import annotations

import asyncio
import json


async def lines(pg, name: str) -> list[str]:
    """The text of every row of the `name` view (`store`, `scheduler`), whitespace trimmed."""
    got = await pg.js(f"[...document.querySelectorAll('.ro-view.{name} .body > div')].map(d => d.textContent.trim())")
    return got if isinstance(got, list) else []


async def until_line(pg, name: str, want, secs: float = 20) -> bool:
    """Whether some row of the view satisfies `want` (a function of the row's text) within `secs`."""
    for _ in range(int(secs * 5)):
        if any(want(t) for t in await lines(pg, name)):
            return True
        await asyncio.sleep(0.2)
    return False


async def refresh(pg, name: str) -> bool:
    """Press the view's `⟳ refresh`."""
    return bool(await pg.js(f"(() => {{ const b = document.querySelector('.ro-view.{name} button.refresh'); if (!b) return false; b.click(); return true; }})()"))


def shown(info: dict, name: str, rows: list[str]) -> None:
    """Keep what the view showed in the summary."""
    info[name] = json.loads(json.dumps(rows))[:20]
