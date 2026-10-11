import { Agent } from "../../lib.ts";
import type { Handler, Json, Kernel, Payload } from "../../lib.ts";

// THEMES AND THEIR LAYOUTS (owner 2026-10-11: "if theme switch — locations
// change and layouts, but agents stay"). A cell is a black box; WHERE it sits is
// not on its record but in its view's LAYOUT for the theme in use — one agent per
// theme, a child of the view:
//
//   view_root                     theme: auto | canvas | mobile
//   ├── layout_canvas  layout.ts  rects: {<cell id>: {x, y, width, height}}
//   └── layout_mobile  layout.ts  rects: {…}
//
// Two themes: `canvas` (the free 2D canvas) and `mobile` (ONE column). `auto`
// (the default) picks per tab by its width: under MOBILE_MAX_WIDTH px, mobile.
// A cell with no rect in the theme in use is PLACED, once, by the theme: canvas
// — the nearest free space around the centre of what the tab shows; mobile —
// the bottom of the column. A removed cell's rects go with it.

export const LAYOUT = "layout.ts";
export const THEMES = ["canvas", "mobile"] as const;
export type Theme = (typeof THEMES)[number];
export type ThemeChoice = Theme | "auto";

/** A tab narrower than this is a phone: `auto` is `mobile` there (the common
 *  768px breakpoint — under it, one column). */
export const MOBILE_MAX_WIDTH = 768;
/** The mobile column, in view coordinates: the first cell's top at `top`, then
 *  `gap` between cells — room for each cell's head (32px) and bezel above it. */
export const COLUMN = { x: 0, width: 400, top: 40, gap: 48 } as const;
/** The space kept between cells the canvas theme places. */
export const GAP = 24;

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export const layoutId = (theme: Theme): string => `layout_${theme}`;

/** The theme a choice means in a tab `width` px wide. */
export function resolveTheme(choice: unknown, width: number): Theme {
  if (choice === "canvas" || choice === "mobile") return choice;
  return width < MOBILE_MAX_WIDTH ? "mobile" : "canvas";
}

const overlaps = (a: Rect, b: Rect, gap: number): boolean =>
  a.x < b.x + b.width + gap && b.x < a.x + a.width + gap && a.y < b.y + b.height + gap && b.y < a.y + a.height + gap;

/** CANVAS: the free spot nearest `center` for a `width`×`height` cell — rings of
 *  candidates around the centre, the closest that touches no occupied rect. */
export function freeSpot(occupied: Rect[], width: number, height: number, center: { x: number; y: number }): Rect {
  const at = (x: number, y: number): Rect => ({ x: Math.round(x - width / 2), y: Math.round(y - height / 2), width, height });
  const first = at(center.x, center.y);
  if (!occupied.some((o) => overlaps(first, o, GAP))) return first;
  const step = Math.max(40, Math.min(width, height) / 4);
  for (let ring = 1; ring <= 200; ring++) {
    const r = ring * step;
    const n = Math.max(8, Math.round((2 * Math.PI * r) / step));
    let best: Rect | null = null;
    let bestD = Infinity;
    for (let i = 0; i < n; i++) {
      const a = (2 * Math.PI * i) / n;
      const c = at(center.x + r * Math.cos(a), center.y + r * Math.sin(a));
      if (occupied.some((o) => overlaps(c, o, GAP))) continue;
      const d = Math.hypot(c.x + width / 2 - center.x, c.y + height / 2 - center.y);
      if (d < bestD) {
        bestD = d;
        best = c;
      }
    }
    if (best !== null) return best;
  }
  return first;
}

/** MOBILE: the bottom of the column, full column width, the cell's own height. */
export function columnSpot(occupied: Rect[], height: number): Rect {
  const bottom = occupied.reduce<number>((m, o) => Math.max(m, o.y + o.height + COLUMN.gap), COLUMN.top);
  return { x: COLUMN.x, y: Math.round(bottom), width: COLUMN.width, height };
}

/** MOBILE: the cells as ONE column — in their order top to bottom, each at the
 *  column's x and full width, `gap` apart; heights kept. */
export function stack(rects: Record<string, Rect>): Record<string, Rect> {
  const out: Record<string, Rect> = {};
  let y: number = COLUMN.top;
  for (const [id, r] of Object.entries(rects).sort((a, b) => a[1].y - b[1].y)) {
    out[id] = { x: COLUMN.x, y, width: COLUMN.width, height: r.height };
    y += r.height + COLUMN.gap;
  }
  return out;
}

/** Where `theme` puts a cell it has no rect for. */
export function assign(theme: Theme, occupied: Rect[], size: { width: number; height: number }, center: { x: number; y: number }): Rect {
  return theme === "mobile" ? columnSpot(occupied, size.height) : freeSpot(occupied, size.width, size.height, center);
}

const isRect = (v: unknown): v is Rect => {
  const r = v as Record<string, unknown> | null;
  return r !== null && typeof r === "object" && ["x", "y", "width", "height"].every((k) => typeof r[k] === "number" && Number.isFinite(r[k]));
};

/** Every rect `theme` holds for `view`'s cells. */
export function rectsOf(kernel: Kernel, view: string, theme: Theme): Record<string, Rect> {
  const id = layoutId(theme);
  const a = kernel.get(id);
  if (a === undefined || a.parentId !== view) return {};
  const raw = a.meta["rects"];
  const out: Record<string, Rect> = {};
  if (raw !== null && typeof raw === "object" && !Array.isArray(raw)) {
    for (const [k, v] of Object.entries(raw as Record<string, unknown>)) if (isRect(v)) out[k] = v;
  }
  return out;
}

/** Replace `theme`'s rects for `view` — its layout agent made on first use. */
export function writeRects(kernel: Kernel, view: string, theme: Theme, rects: Record<string, Rect>): void {
  const id = layoutId(theme);
  if (kernel.get(id) === undefined) {
    kernel.register(new Agent({ id, parentId: view, handlerModule: LAYOUT, meta: { theme, rects } as unknown as Payload }));
    return;
  }
  kernel.updateMeta(id, { rects } as unknown as Payload);
}

/** This tab's width (no window — a test, a worker — counts as wide). */
export const tabWidth = (): number => (typeof innerWidth === "number" ? innerWidth : Infinity);

/** The theme `view` is in, for a tab `width` px wide. */
export function themeOf(kernel: Kernel, view: string, width: number): Theme {
  return resolveTheme(kernel.get(view)?.meta["theme"], width);
}

/** Set (merge) a cell's rect in `theme`: partial fields over what it had. */
export function setRect(kernel: Kernel, view: string, theme: Theme, cell: string, patch: Partial<Rect>, size: { width: number; height: number } = { width: 480, height: 320 }): Rect {
  const rects = rectsOf(kernel, view, theme);
  const had = rects[cell] ?? { x: 0, y: 0, ...size };
  const next: Rect = { ...had };
  for (const k of ["x", "y", "width", "height"] as const) {
    const v = patch[k];
    if (typeof v === "number" && Number.isFinite(v)) next[k] = Math.round(v);
  }
  writeRects(kernel, view, theme, { ...rects, [cell]: next });
  return next;
}

/** The cell PINNED in `theme` (mobile: held on the screen's bottom while the
 *  column scrolls), or null. One per layout. */
export function pinnedOf(kernel: Kernel, view: string, theme: Theme): string | null {
  const a = kernel.get(layoutId(theme));
  const p = a !== undefined && a.parentId === view ? a.meta["pinned"] : null;
  return typeof p === "string" && p !== "" ? p : null;
}

/** Pin `cell` in `theme` (null: unpin) — it replaces the one pinned before. */
export function setPinned(kernel: Kernel, view: string, theme: Theme, cell: string | null): void {
  if (kernel.get(layoutId(theme)) === undefined) writeRects(kernel, view, theme, {});
  kernel.updateMeta(layoutId(theme), { pinned: cell } as unknown as Payload);
}

/** Drop every rect of cells no longer in `view` (all themes). */
export function prune(kernel: Kernel, view: string, live: Set<string>): void {
  for (const t of THEMES) {
    const p = pinnedOf(kernel, view, t);
    if (p !== null && !live.has(p)) setPinned(kernel, view, t, null);
    const rects = rectsOf(kernel, view, t);
    const gone = Object.keys(rects).filter((k) => !live.has(k));
    if (gone.length === 0) continue;
    for (const k of gone) delete rects[k];
    // a layout left with no cell goes too (made again on the next rect)
    if (Object.keys(rects).length === 0) kernel.remove(layoutId(t));
    else writeRects(kernel, view, t, rects);
  }
}

/** Give every cell in `cells` without a rect in `theme` one, by the theme's rule
 *  (in order) — one write; returns the full map. */
export function placeMissing(
  kernel: Kernel,
  view: string,
  theme: Theme,
  cells: Array<{ id: string; size: { width: number; height: number } }>,
  center: { x: number; y: number },
): Record<string, Rect> {
  const rects = rectsOf(kernel, view, theme);
  let added = false;
  for (const c of cells) {
    if (rects[c.id] !== undefined) continue;
    rects[c.id] = assign(theme, Object.values(rects), c.size, center);
    added = true;
  }
  if (added) writeRects(kernel, view, theme, rects);
  return rects;
}

export const LAYOUT_README =
  "A view's LAYOUT for one theme (canvas | mobile): `rects: {<cell id>: {x, y, width, height}}` — where each cell sits in that theme. " +
  "The cells are untouched by it; the view's `theme` verb picks which layout is in use (auto: mobile under 768px wide). " +
  "Move cells with the view's `move`; a cell with no rect in a theme is placed by it (canvas: free space near the centre; mobile: the column's bottom). " +
  "`pinned: <cell id>` (mobile): that cell is held on the screen's bottom while the column scrolls (the 📌 on its head).";

/** A layout holds state only: it answers reflect (the kernel's) and nothing else. */
export const layoutAgent: Handler = (_id, payload): Json => {
  const t = (payload as Record<string, unknown>)["type"];
  if (t === "boot" || t === "shutdown") return null;
  return { error: `layout: unknown verb '${String(t)}' — a layout is moved through its view: move {id, x, y, width, height}; theme {name}`, reason: "unhandled", verb: String(t) };
};

export function registerLayout(kernel: { registerBundle(hm: string, h: Handler): void; setBundleReadme(hm: string, readme: string): void }): void {
  kernel.registerBundle(LAYOUT, layoutAgent);
  kernel.setBundleReadme(LAYOUT, LAYOUT_README);
}
