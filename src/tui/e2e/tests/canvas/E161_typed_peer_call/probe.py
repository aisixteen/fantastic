"""E161 typed_peer_call — a call to a page agent on another kernel is typed by THAT
kernel's schema (#719, #728).

The sender is `user.forward` (what a canvas tab's host call is), one of the senders that
type a call through `typing::payload`: for `<page guid>|view_root` it asks the page's
`docs {of: view_root, verb: place, part: schema}` first, then reads the text by it. The
evidence is what the page RECEIVED (its socket's frames, read over CDP), the layout record
on disk and the cell's frame on the page."""

from __future__ import annotations

import asyncio
import base64
import json
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict  # noqa: E402

CELL, FAR = "e161_typed", "e161_far"
BODY = "<p id='e161'>typed by the page's schema</p>"
RECT = {"x": 100, "y": 200, "width": 320, "height": 180}


def head_of(raw: str) -> object:
    """A received frame's JSON: a text frame's whole, or a binary frame's head (CDP
    gives it base64: `[4-byte big-endian head length | head | body]`); None when neither."""
    try:
        return json.loads(raw)
    except ValueError:
        pass
    try:
        b = base64.b64decode(raw, validate=True)
        return json.loads(b[4:4 + int.from_bytes(b[:4], "big")])
    except ValueError:
        return None


def calls_received(frames: list[str]) -> list[dict]:
    """Every CALL frame the page received (`{type: call, target, payload}`), in order."""
    out = []
    for raw in frames:
        f = head_of(raw)
        if isinstance(f, dict) and f.get("type") == "call" and isinstance(f.get("payload"), dict):
            out.append(f)
    return out


def is_docs_ask(f: dict) -> bool:
    p = f["payload"]
    return (f.get("target") == "kernel" and p.get("type") == "docs" and p.get("of") == "view_root"
            and p.get("verb") == "place" and p.get("part") == "schema")


def is_place(f: dict, cell: str) -> bool:
    p = f["payload"]
    return f.get("target") == "view_root" and p.get("type") == "place" and p.get("id") == cell


def number(v: object, want: int) -> bool:
    return isinstance(v, (int, float)) and not isinstance(v, bool) and v == want


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.init()
    r: dict = {}
    info: dict = {}

    async def run() -> None:
        from canvas import host_call

        guid = rig.page_guid()
        r["0 the page has a guid"] = bool(guid)
        # the page's socket frames are read over CDP: Network is enabled before the page opens
        os.environ["CANVAS_WS_LOG"] = "1"
        async with rig.page(1200, 800) as pg:
            pg.ws_received = []
            # every geometry value written as TEXT, as a model's `<x>100</x>` is
            text = {k: str(v) for k, v in RECT.items()}
            placed = await host_call(rig.port, f"{guid}|view_root", {"type": "place", "kind": "html", "id": CELL, "bytes": BODY, **text})
            info["placed"] = placed
            r["1 the place answers placed"] = placed.get("placed") == CELL
            await pg.js("1")  # read the frame events Chrome sent up to now
            calls = calls_received(pg.ws_received)
            info["calls_received"] = [(c.get("target"), c["payload"].get("type")) for c in calls]
            asks = [i for i, c in enumerate(calls) if is_docs_ask(c)]
            places = [i for i, c in enumerate(calls) if is_place(c, CELL)]
            r["2 the page was asked docs {of: view_root, verb: place, part: schema}"] = bool(asks)
            r["3 the page received the place once, after the docs ask"] = len(places) == 1 and bool(asks) and asks[0] < places[0]
            got = calls[places[0]]["payload"] if places else {}
            info["place_received"] = {k: got.get(k) for k in RECT}
            r["4 the page received numbers (x == 100, y == 200, width, height)"] = bool(got) and all(number(got.get(k), v) for k, v in RECT.items())
            r["5 the layout holds the cell at that rect"] = await until_rect(rig, CELL, RECT, 15)
            info["layout_rect"] = rig.rects().get(CELL)
            style = (f"(()=>{{const e=document.querySelector('.agent-frame[data-id={CELL}]');"
                     "return e ? [e.style.left, e.style.top, e.style.width, e.style.height] : null;})()")
            want = [f"{RECT[k]}px" for k in ("x", "y", "width", "height")]
            r["6 the cell's frame is drawn at that rect"] = await pg.until(f"JSON.stringify({style}) === {json.dumps(json.dumps(want, separators=(',', ':')))}", 15)
            info["frame_style"] = await pg.js(style)

            # ── negative: a value that is not a number is refused by its argument's name on
            #    the host; the place call never reaches the page
            pg.ws_received = []
            refused = await host_call(rig.port, f"{guid}|view_root", {"type": "place", "kind": "html", "id": FAR, "bytes": BODY, **text, "x": "far"})
            info["refused"] = refused
            r["7 x='far' is refused by argument name (bad_arg, arg x)"] = refused.get("reason") == "bad_arg" and refused.get("arg") == "x"
            await asyncio.sleep(1.0)
            await pg.js("1")
            calls = calls_received(pg.ws_received)
            info["calls_received_after_refusal"] = [(c.get("target"), c["payload"].get("type")) for c in calls]
            r["8 the refused place never reached the page"] = not any(is_place(c, FAR) for c in calls)
            r["9 no cell was made for it"] = FAR not in rig.rects() and not await pg.js(f"!!document.querySelector('.agent-frame[data-id={FAR}]')")
            info["page_errors"] = [x for x in pg.logs if "WebGL" not in x][:8]
            r["10 no page errors"] = not info["page_errors"]

    try:
        asyncio.run(run())
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:3000])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


async def until_rect(rig: Rig, cell: str, want: dict, secs: float) -> bool:
    """Whether the free-canvas layout on disk holds `cell` at `want` within `secs`."""
    for _ in range(int(secs * 5)):
        if rig.rects().get(cell) == want:
            return True
        await asyncio.sleep(0.2)
    return False


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
