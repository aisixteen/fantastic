import { Agent } from "../../lib.ts";
import type { Handler, Json, Kernel, Payload } from "../../lib.ts";

// A VIEW'S LAYOUTS. A cell is a black box; WHERE it sits is not on its record but
// in its view's LAYOUT — one agent per layout, a child of the view:
//
//   view_root
//   ├── layout_canvas  layout.ts  rects: {<cell id>: {x, y, width, height}}   (free)
//   └── layout_mobile  layout.ts  rects: {…}, pinned, minimized               (column)
//
// The view picks its layout by the tab's width, a fixed rule with no choice: a tab
// narrower than MOBILE_MAX_WIDTH px draws ONE column, any other the free 2D canvas.
// (`layout_mobile` is only where the column's state is stored.) A cell with no rect
// in the layout in use is PLACED, once: free — the nearest free space around the
// centre of what the tab shows; column — the column's bottom. A removed cell's rects
// go with it.

export const LAYOUT = "layout.ts";
/** How the view lays its cells out: the free canvas, or one column. */
export type Layout = "free" | "column";
const LAYOUTS: ReadonlyArray<Layout> = ["free", "column"];

/** A tab narrower than this draws the column (the common 768px breakpoint). */
export const MOBILE_MAX_WIDTH = 768;
/** The mobile column, in view coordinates: the first cell's top at `top`, then
 *  `gap` between cells — exactly each cell's head (32px): flat, no holes. */
export const COLUMN = { x: 0, width: 400, top: 32, gap: 32 } as const;
/** The space kept between cells the free canvas places. */
export const GAP = 24;

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** The layout agent that stores a layout's state (the ids existing views already store under). */
export const layoutId = (layout: Layout): string => (layout === "column" ? "layout_mobile" : "layout_canvas");

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

/** COLUMN: the bottom of the column, full column width, the cell's own height. */
export function columnSpot(occupied: Rect[], height: number): Rect {
  const bottom = occupied.reduce<number>((m, o) => Math.max(m, o.y + o.height + COLUMN.gap), COLUMN.top);
  return { x: COLUMN.x, y: Math.round(bottom), width: COLUMN.width, height };
}

/** COLUMN: the cells as ONE column — in their order top to bottom, each at the
 *  column's x and full width, `gap` apart; heights kept. A `minimized` cell is
 *  its head alone: the next one follows its head. */
export function stack(rects: Record<string, Rect>, minimized: ReadonlySet<string> = new Set()): Record<string, Rect> {
  const out: Record<string, Rect> = {};
  let y: number = COLUMN.top;
  for (const [id, r] of Object.entries(rects).sort((a, b) => a[1].y - b[1].y)) {
    out[id] = { x: COLUMN.x, y, width: COLUMN.width, height: r.height };
    y += (minimized.has(id) ? 0 : r.height) + COLUMN.gap;
  }
  return out;
}

/** COLUMN REORDER: the `y` that puts cell `id`, dropped with its top at `top`,
 *  in the slot its centre is over — just above the first cell whose centre is
 *  below it, else after the last; its own `y` when that changes nothing. The
 *  column restacks by `y` (`stack`). */
export function reorderY(rects: Record<string, Rect>, id: string, top: number): number {
  const me = rects[id];
  if (me === undefined) return top;
  const mid = top + me.height / 2;
  const others = Object.entries(rects).filter(([k]) => k !== id).sort((a, b) => a[1].y - b[1].y);
  const before = others.findIndex(([, r]) => mid < r.y + r.height / 2);
  // the slot it already holds: nothing moves
  const now = others.findIndex(([, r]) => r.y > me.y);
  if (before === now) return me.y;
  if (before >= 0) return others[before]![1].y - 0.5;
  const last = others[others.length - 1];
  return last !== undefined ? last[1].y + 0.5 : me.y;
}

/** Where `kind` puts a cell it has no rect for. */
export function assign(kind: Layout, occupied: Rect[], size: { width: number; height: number }, center: { x: number; y: number }): Rect {
  return kind === "column" ? columnSpot(occupied, size.height) : freeSpot(occupied, size.width, size.height, center);
}

const isRect = (v: unknown): v is Rect => {
  const r = v as Record<string, unknown> | null;
  return r !== null && typeof r === "object" && ["x", "y", "width", "height"].every((k) => typeof r[k] === "number" && Number.isFinite(r[k]));
};

/** Every rect `layout` holds for `view`'s cells. */
export function rectsOf(kernel: Kernel, view: string, layout: Layout): Record<string, Rect> {
  const id = layoutId(layout);
  const a = kernel.get(id);
  if (a === undefined || a.parentId !== view) return {};
  const raw = a.meta["rects"];
  const out: Record<string, Rect> = {};
  if (raw !== null && typeof raw === "object" && !Array.isArray(raw)) {
    for (const [k, v] of Object.entries(raw as Record<string, unknown>)) if (isRect(v)) out[k] = v;
  }
  return out;
}

/** Replace `layout`'s rects for `view` — its layout agent made on first use. */
export function writeRects(kernel: Kernel, view: string, layout: Layout, rects: Record<string, Rect>): void {
  const id = layoutId(layout);
  if (kernel.get(id) === undefined) {
    kernel.register(new Agent({ id, parentId: view, handlerModule: LAYOUT, meta: { rects } as unknown as Payload }));
    return;
  }
  kernel.updateMeta(id, { rects } as unknown as Payload);
}

/** This tab's width (no window — a test, a worker — counts as wide). */
export const tabWidth = (): number => (typeof innerWidth === "number" ? innerWidth : Infinity);

/** The layout a tab `width` px wide draws: the column under MOBILE_MAX_WIDTH, else the free canvas. */
export const layoutFor = (width: number): Layout => (width < MOBILE_MAX_WIDTH ? "column" : "free");

/** Set (merge) a cell's rect in `layout`: partial fields over what it had. */
export function setRect(kernel: Kernel, view: string, layout: Layout, cell: string, patch: Partial<Rect>, size: { width: number; height: number } = { width: 480, height: 320 }): Rect {
  const rects = rectsOf(kernel, view, layout);
  const had = rects[cell] ?? { x: 0, y: 0, ...size };
  const next: Rect = { ...had };
  for (const k of ["x", "y", "width", "height"] as const) {
    const v = patch[k];
    if (typeof v === "number" && Number.isFinite(v)) next[k] = Math.round(v);
  }
  writeRects(kernel, view, layout, { ...rects, [cell]: next });
  return next;
}

/** The cell PINNED in `layout` (mobile: held on the screen's bottom while the
 *  column scrolls), or null. One per layout. */
export function pinnedOf(kernel: Kernel, view: string, layout: Layout): string | null {
  const a = kernel.get(layoutId(layout));
  const p = a !== undefined && a.parentId === view ? a.meta["pinned"] : null;
  return typeof p === "string" && p !== "" ? p : null;
}

/** Pin `cell` in `layout` (null: unpin) — it replaces the one pinned before. */
export function setPinned(kernel: Kernel, view: string, layout: Layout, cell: string | null): void {
  if (kernel.get(layoutId(layout)) === undefined) writeRects(kernel, view, layout, {});
  kernel.updateMeta(layoutId(layout), { pinned: cell } as unknown as Payload);
}

/** The cells MINIMIZED in `layout` — shown as their head bar alone (a double
 *  click on the head toggles it). */
export function minimizedOf(kernel: Kernel, view: string, layout: Layout): Set<string> {
  const a = kernel.get(layoutId(layout));
  const m = a !== undefined && a.parentId === view ? a.meta["minimized"] : null;
  return new Set(Array.isArray(m) ? m.filter((x): x is string => typeof x === "string") : []);
}

/** Minimize `cell` in `layout` (`on` false: back to its full size). */
export function setMinimized(kernel: Kernel, view: string, layout: Layout, cell: string, on: boolean): void {
  if (kernel.get(layoutId(layout)) === undefined) writeRects(kernel, view, layout, {});
  const m = minimizedOf(kernel, view, layout);
  if (on) m.add(cell);
  else m.delete(cell);
  kernel.updateMeta(layoutId(layout), { minimized: [...m] } as unknown as Payload);
}

/** The cells of `view` HIDDEN (#133): out of the view, their agents running. Per
 *  view, not per layout — it is on the view's own record. */
export function hiddenOf(kernel: Kernel, view: string): Set<string> {
  const m = kernel.get(view)?.meta["hidden"];
  return new Set(Array.isArray(m) ? m.filter((x): x is string => typeof x === "string") : []);
}

/** Hide `cell` in `view` (`on` false: show it again — its rect kept). */
export function setHidden(kernel: Kernel, view: string, cell: string, on: boolean): void {
  const h = hiddenOf(kernel, view);
  if (on) h.add(cell);
  else h.delete(cell);
  kernel.updateMeta(view, { hidden: [...h] } as unknown as Payload);
}

/** Show `cell` again if its view hides it (#133): a pick of a hidden cell (the
 *  file tree's) brings it back where it was. True when it was hidden. */
export function showHidden(kernel: Kernel, cell: string): boolean {
  const view = kernel.get(cell)?.parentId ?? null;
  if (view === null || !hiddenOf(kernel, view).has(cell)) return false;
  setHidden(kernel, view, cell, false);
  return true;
}

/** The HIDDEN cell (#133) among `cells` that shows agent `backendId`, if any: a
 *  kept dialog reopened comes back to its hidden cell, not a second one beside it. */
export function hiddenCellFor(kernel: Kernel, backendId: string, cells: string[]): string | undefined {
  return cells.find((id) => {
    const a = kernel.get(id);
    const view = a?.parentId;
    return a !== undefined && typeof view === "string" && a.record()["backend_id"] === backendId && hiddenOf(kernel, view).has(id);
  });
}

/** The frames a finished refresh takes down: those it did not draw, or whose agent is gone. A
 *  refresh a newer one has overtaken (`current` false) takes down NONE: its member list is a
 *  snapshot from before the newer one's changes (a cell shown meanwhile is not in it), and the
 *  newer refresh sweeps with the right list. */
export function framesToRemove(frames: Iterable<string>, drawn: Set<string>, exists: (id: string) => boolean, current: boolean): string[] {
  if (!current) return [];
  return [...frames].filter((id) => !drawn.has(id) || !exists(id));
}

/** Drop every rect of cells no longer in `view` (both layouts), and any other
 *  layout child an older view stored (`layout_<name>`). */
export function prune(kernel: Kernel, view: string, live: Set<string>): void {
  for (const h of hiddenOf(kernel, view)) {
    // a chat card's hide (`embed:<chat cell>:<agent>`, #339) lives while its chat cell does
    const chat = /^embed:([^:]+):/.exec(h)?.[1];
    if (!live.has(chat ?? h)) setHidden(kernel, view, h, false);
  }
  const kept = new Set<string>(LAYOUTS.map(layoutId));
  for (const a of [...(kernel.get(view)?.children.values() ?? [])]) {
    if (a.record().handler_module === LAYOUT && !kept.has(a.id)) kernel.remove(a.id);
  }
  for (const t of LAYOUTS) {
    const p = pinnedOf(kernel, view, t);
    if (p !== null && !live.has(p)) setPinned(kernel, view, t, null);
    for (const m of minimizedOf(kernel, view, t)) if (!live.has(m)) setMinimized(kernel, view, t, m, false);
    const rects = rectsOf(kernel, view, t);
    const gone = Object.keys(rects).filter((k) => !live.has(k));
    if (gone.length === 0) continue;
    for (const k of gone) delete rects[k];
    // a layout left with no cell goes too (made again on the next rect)
    if (Object.keys(rects).length === 0) kernel.remove(layoutId(t));
    else writeRects(kernel, view, t, rects);
  }
}

/** Give every cell in `cells` without a rect in `layout` one, by the layout's rule
 *  (in order) — one write; returns the full map. */
export function placeMissing(
  kernel: Kernel,
  view: string,
  layout: Layout,
  cells: Array<{ id: string; size: { width: number; height: number } }>,
  center: { x: number; y: number },
): Record<string, Rect> {
  const rects = rectsOf(kernel, view, layout);
  let added = false;
  for (const c of cells) {
    if (rects[c.id] !== undefined) continue;
    rects[c.id] = assign(layout, Object.values(rects), c.size, center);
    added = true;
  }
  if (added) writeRects(kernel, view, layout, rects);
  return rects;
}

export const LAYOUT_README =
  "A view's LAYOUT: `rects: {<cell id>: {x, y, width, height}}` — where each cell sits. `layout_canvas` is the free canvas; `layout_mobile` is where the one-column phone layout (a tab under 768px wide) keeps its state. " +
  "The view uses one by the tab's width; the cells are untouched by it. " +
  "Move cells with the view's `move`; a cell with no rect is placed by its layout (free: free space near the centre; column: the column's bottom). " +
  "`minimized: [<cell id>…]`: those cells show their head bar alone (a double click on the head toggles it). " +
  "`pinned: <cell id>` (the column): that cell is held on the screen's bottom while the column scrolls (the 📌 on its head). " +
  "The view's `hidden: [<cell id>…]` are the cells not shown: their agents run on and their rects are kept, so `show` puts them back where they were.";

/** A layout holds state only: it answers reflect (the kernel's) and nothing else. */
export const layoutAgent: Handler = (_id, payload): Json => {
  const t = (payload as Record<string, unknown>)["type"];
  if (t === "boot" || t === "shutdown") return null;
  return { error: `layout: unknown verb '${String(t)}' — a layout is moved through its view: move {id, x, y, width, height}`, reason: "unhandled", verb: String(t) };
};

export function registerLayout(kernel: { registerBundle(hm: string, h: Handler): void; setBundleReadme(hm: string, readme: string): void }): void {
  kernel.registerBundle(LAYOUT, layoutAgent);
  kernel.setBundleReadme(LAYOUT, LAYOUT_README);
}
