"""E66 render_fidelity — the shared-WebGL terminal renderer, CHECKED FRAME BY
FRAME (owner 2026-10-09: "catch atlas + what rendered to confirm each frame").

K terminals (FIDELITY_K, default 12) on the patched addon (one WebGL context
for the page, one shared glyph atlas). Each round every terminal runs a SEEDED
glyph/colour sweep (`glyphs.py`: ASCII, box drawing, blocks, braille, CJK,
Hangul, emoji, combining marks, arrows; random 256-colour and true colour; every
SGR style) — distinct (glyph, colour, style) keys that grow the atlas, merge its
pages and evict. Checked:

- EVERY FRAME, every terminal: no placeholder-red pixel (the addon's 1×1 red
  texture shows when a renderer draws with an unbound/unuploaded page — the
  sweep never uses red);
- after every round, every terminal: its pixels EQUAL its fresh redraw — the
  probe clears ONE terminal's atlas (the shared one: every sibling's texture
  coordinates go stale unless the layout version reaches them), lets all redraw,
  and compares each to its own pixels from before; then it clears each
  terminal's atlas in turn and compares again;
- a LOST CONTEXT (WEBGL_lose_context): every terminal comes back, non-blank and
  equal to its pixels from before the loss.

It reports the atlas: page textures live, released, uploads, contexts made."""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, py_typed, verdict  # noqa: E402

K = int(os.environ.get("FIDELITY_K", "12"))
# pixels a terminal may change on a redraw: a glyph that overhangs its cell can
# land a texel column apart (the GPU's own rounding); a STALE glyph — what this
# catches — is a whole cell wrong, hundreds of pixels
TOL = 64
ROUNDS = int(os.environ.get("FIDELITY_ROUNDS", "6"))
VERBS = ["watch", "status", "start", "write", "ack", "resize", "output", "paste_image"]
HERE = Path(__file__).resolve().parent


def seeds(k: int) -> tuple[list[dict], list[tuple[str, dict]]]:
    cells, hosts = [], []
    for i in range(k):
        cid, bid = f"cell_fid{i:03d}", f"exec_bridge_fid{i:03d}"
        cells.append({"id": cid, "handler_module": "terminal_view.ts", "backend_id": bid, "owns_backend": False,
                      "can_call": {"$self": "*", bid: VERBS}, "callable_by": {"$self": "*"},
                      "x": 10 + (i % 4) * 390, "y": 10 + (i // 4) * 300, "width": 380, "height": 290})
        hosts.append((FOLDER, {"id": bid, "handler_module": "exec_bridge.tools", "cwd": ".", "inherit_env": ["*"], "shell": "login",
                               "ingress_rule": "allow_all_ingress", "cell_id": cid, "can_call": {"$self": "*"},
                               "callable_by": {"$self": "*", "mcp": ["*"], "user": ["*"], "canvas_root": ["*"], "canvas_ws": ["watch"]}}))
    return cells, hosts


# page-side helpers: a terminal's pixels (its canvas drawn into a 2D canvas),
# their hash, and a per-frame watch for placeholder red over every terminal
PAGE = r"""
window.__fid = (() => {
  const frames = () => [...document.querySelectorAll('.agent-frame')].filter(f => f.querySelector('.frame-body')?.xterm);
  const canvasOf = f => f.querySelector('.xterm-screen canvas:not(.xterm-link-layer)');
  const scratch = document.createElement('canvas'); const sx = scratch.getContext('2d', { willReadFrequently: true });
  const pixels = f => { const c = canvasOf(f); if (!c || !c.width) return null;
    scratch.width = c.width; scratch.height = c.height; sx.clearRect(0, 0, c.width, c.height); sx.drawImage(c, 0, 0);
    return sx.getImageData(0, 0, c.width, c.height).data; };
  const hash = d => { let h = 2166136261 >>> 0; for (let i = 0; i < d.length; i += 4) { h ^= d[i] | (d[i+1] << 8) | (d[i+2] << 16) | (d[i+3] << 24); h = Math.imul(h, 16777619) >>> 0; } return h; };
  const reds = d => { let n = 0; for (let i = 0; i < d.length; i += 4) if (d[i] > 235 && d[i+1] < 25 && d[i+2] < 25 && d[i+3] > 235) n++; return n; };
  const lit = d => { let n = 0; for (let i = 3; i < d.length; i += 4) if (d[i] > 0) n++; return n; };
  const state = { frames: 0, redFrames: 0, redMax: 0, redWhere: [] , watching: false };
  const watch = () => { if (!state.watching) return; state.frames++;
    for (const f of frames()) { const d = pixels(f); if (!d) continue; const r = reds(d);
      if (r > 0) { state.redFrames++; state.redMax = Math.max(state.redMax, r); if (state.redWhere.length < 20) state.redWhere.push(`${f.dataset.id}:${r}`); } }
    requestAnimationFrame(watch); };
  return {
    ids: () => frames().map(f => f.dataset.id),
    snap: () => Object.fromEntries(frames().map(f => { const d = pixels(f); return [f.dataset.id, d ? { h: hash(d), lit: lit(d), red: reds(d) } : null]; })),
    text: id => { const f = frames().find(x => x.dataset.id === id); const t = f?.querySelector('.frame-body').xterm; if (!t) return '';
      const b = t.buffer.active; let s = ''; for (let i = 0; i < t.rows; i++) { const l = b.getLine(b.viewportY + i); if (l) s += l.translateToString(true) + '\n'; } return s; },
    quiet: () => { for (const f of frames()) { const t = f.querySelector('.frame-body').xterm; t.options.cursorBlink = false; t.options.cursorInactiveStyle = 'none'; t.blur?.(); } },
    clearAtlas: id => { const f = frames().find(x => x.dataset.id === id); f?.querySelector('.frame-body').webgl?.clearTextureAtlas(); },
    watchStart: () => { state.watching = true; requestAnimationFrame(watch); },
    watchStop: () => { state.watching = false; return state; },
    gl: () => ({ ...(globalThis.__fantasticWebgl || {}) }),
    // the pixels of every terminal kept, then how many changed since (any channel)
    keep: () => { window.__kept = Object.fromEntries(frames().map(f => [f.dataset.id, pixels(f)?.slice() ?? null])); return true; },
    changed: () => Object.fromEntries(frames().map(f => { const a = (window.__kept || {})[f.dataset.id], b = pixels(f);
      if (!a || !b || a.length !== b.length) return [f.dataset.id, -1];
      let n = 0; for (let i = 0; i < a.length; i += 4) if (a[i] !== b[i] || a[i+1] !== b[i+1] || a[i+2] !== b[i+2] || a[i+3] !== b[i+3]) n++; return [f.dataset.id, n]; })),
  };
})();
"""


async def settle(pg, rounds: int = 12) -> dict:
    """Snapshot until two in a row agree (every terminal drawn and still)."""
    last = None
    for _ in range(rounds):
        await asyncio.sleep(0.35)
        s = await pg.js("window.__fid.snap()")
        if s == last:
            return s
        last = s
    return last or {}


async def run(rig: Rig, r: dict, info: dict) -> None:
    from canvas import host_call
    async with rig.page(1600, 1000) as pg:
        await pg.js(PAGE)
        ids = [c["id"] for c in seeds(K)[0]]
        await pg.until(f"window.__fid.ids().length >= {K} && window.__fid.ids().every(id => window.__fid.text(id).trim().length > 0)", 60)
        await pg.js("window.__fid.quiet()")
        info["renderer"] = await pg.js("(()=>document.querySelector('.agent-frame .xterm-screen canvas:not(.xterm-link-layer)') ? 'canvas' : 'dom')()")
        jobs: dict[str, int] = {}
        for i in range(K):
            bid = f"exec_bridge_fid{i:03d}"
            # its login shell: running a moment after its cell drew its first text
            for _ in range(150):
                st = await host_call(rig.port, bid, {"type": "status"})
                job = next((j["job_id"] for j in st.get("jobs", []) if j.get("pty") and j.get("status") == "running"), None)
                if job is not None:
                    jobs[bid] = job
                    break
                await asyncio.sleep(0.2)
            else:
                raise SystemExit(f"{bid}: no running shell in 30 s ({st})")
        await pg.js("window.__fid.watchStart()")
        mismatches: list[str] = []
        worst = 0
        blank: list[str] = []
        for rnd in range(ROUNDS):
            mk = f"DONE{rnd:02d}"
            # the sweep, then a marker line the probe waits for
            await asyncio.gather(*(host_call(rig.port, f"exec_bridge_fid{i:03d}", {"type": "write", "job_id": jobs[f"exec_bridge_fid{i:03d}"],
                                   "data": py_typed("glyphs.py", str(i), str(rnd), mk) + "\r"}) for i in range(K)))
            await pg.until(" && ".join(f"window.__fid.text({json.dumps(c)}).includes({json.dumps(mk)})" for c in ids), 30)
            await pg.js("window.__fid.quiet()")
            before = await settle(pg)
            blank += [f"r{rnd}:{c}" for c, v in before.items() if not v or v["lit"] == 0]
            # ONE terminal clears the SHARED atlas: every sibling must redraw right
            victim = ids[rnd % K]
            await pg.js("window.__fid.keep()")
            await pg.js(f"window.__fid.clearAtlas({json.dumps(victim)})")
            await settle(pg)
            ch = await pg.js("window.__fid.changed()")
            worst = max([worst, *[n for n in ch.values() if n >= 0]])
            mismatches += [f"r{rnd} sibling-clear by {victim}: {c} ({ch.get(c)} px)" for c in ids if ch.get(c, -1) < 0 or ch[c] > TOL]
            # each terminal against its own fresh redraw
            for c in ids[rnd % 3:: 3]:
                await pg.js("window.__fid.keep()")
                await pg.js(f"window.__fid.clearAtlas({json.dumps(c)})")
                await settle(pg)
                n = (await pg.js("window.__fid.changed()")).get(c, -1)
                worst = max(worst, n)
                if n < 0 or n > TOL:
                    mismatches.append(f"r{rnd} fresh redraw: {c} ({n} px)")
        # a LOST context: every terminal back, as it was
        await settle(pg)
        await pg.js("window.__fid.keep()")
        await pg.js("window.__fantasticWebgl.loseContext()")
        await asyncio.sleep(4.5)  # the addon waits 3 s for a restore, then rebuilds
        post = await settle(pg, 20)
        ch = await pg.js("window.__fid.changed()")
        info["context_loss_changed_px"] = ch
        lost = [c for c in ids if ch.get(c, -1) < 0 or ch[c] > TOL or not post.get(c) or post[c]["lit"] == 0]
        watch = await pg.js("window.__fid.watchStop()")
        gl = await pg.js("window.__fid.gl()")
        info["frames_checked"] = watch["frames"]
        info["red"] = {"frames": watch["redFrames"], "max_px": watch["redMax"], "where": watch["redWhere"]}
        info["mismatches"] = mismatches[:30]
        info["redraw_worst_px"] = worst
        info["blank"] = blank[:20]
        info["after_context_loss_differs"] = lost
        info["atlas"] = {k: gl.get(k) for k in ("pages", "released", "uploads", "contexts", "renders")}
        info["upload_mb"] = round((gl.get("uploadPx") or 0) * 4 / 1e6, 1)
        r["renderer_webgl"] = info["renderer"] == "canvas"
        r["no_red_in_any_frame"] = watch["frames"] > 0 and watch["redFrames"] == 0
        r["every_terminal_equals_its_fresh_redraw"] = not mismatches
        r["none_blank"] = not blank
        r["context_loss_recovers"] = not lost and (gl.get("contexts") or 0) >= 2


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.proj.mkdir(parents=True, exist_ok=True)
    shutil.copy2(HERE / "glyphs.py", rig.proj / "glyphs.py")
    cells, hosts = seeds(K)
    r: dict = {}
    info: dict = {"k": K, "rounds": ROUNDS}
    try:
        rig.init(cells=cells, host_agents=hosts)
        asyncio.run(run(rig, r, info))
    finally:
        rig.stop()
    print(json.dumps(info, indent=1, ensure_ascii=False))
    ok = all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "all checks pass" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
