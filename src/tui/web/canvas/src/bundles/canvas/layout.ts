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
// THEMES ARE DATA (owner 2026-10-12: "themes like backgrounds — edit, restore;
// a trigger to auto-switch"): a theme is {name, layout: free | column, trigger}.
// Two STOCK themes — `canvas` (the free 2D canvas) and `mobile` (ONE column,
// triggered under MOBILE_MAX_WIDTH px) — and the user's own, kept on the view's
// CANVAS THEME agent (`backgrounds.ts`: `user_themes`, `hidden_themes`). `auto`
// (the default) takes the first theme whose trigger matches the tab's width
// (yours first), else the first without a trigger. A cell with no rect in the
// theme in use is PLACED, once, by its layout: free — the nearest free space
// around the centre of what the tab shows; column — the column's bottom. A
// removed cell's rects go with it.

export const LAYOUT = "layout.ts";
/** A theme's name (its layout agent is `layout_<name>`). */
export type Theme = string;
export type ThemeChoice = Theme | "auto";
/** How a theme lays its cells out. */
export type ThemeLayout = "free" | "column";
/** When `auto` switches to a theme: the tab's width within these bounds (px). */
export interface ThemeTrigger {
  max_width?: number;
  min_width?: number;
}
export interface ThemeDef {
  name: string;
  layout: ThemeLayout;
  trigger: ThemeTrigger | null;
  note?: string;
}
/** The canvas theme agent's handler (the view's child that keeps the user's themes). */
export const CANVAS_THEME_AGENT = "backgrounds.ts";

/** A tab narrower than this is a phone: `auto` is `mobile` there (the common
 *  768px breakpoint — under it, one column). */
export const MOBILE_MAX_WIDTH = 768;
/** The mobile column, in view coordinates: the first cell's top at `top`, then
 *  `gap` between cells — exactly each cell's head (32px): flat, no holes. */
export const COLUMN = { x: 0, width: 400, top: 32, gap: 32 } as const;
/** The space kept between cells the canvas theme places. */
export const GAP = 24;

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export const layoutId = (theme: Theme): string => `layout_${theme}`;

/** The stock themes, in their order. */
export const STOCK_THEMES: ReadonlyArray<ThemeDef> = [
  { name: "canvas", layout: "free", trigger: null, note: "the free 2D canvas" },
  { name: "mobile", layout: "column", trigger: { max_width: MOBILE_MAX_WIDTH - 1 }, note: "one column — a tab under 768px wide" },
];

/** The default theme: the free canvas, never deleted (owner 2026-10-12). */
export const DEFAULT_THEME: Theme = "canvas";

/** A name a theme may take (it names its layout agent). */
export const themeNameOk = (n: string): boolean => /^[A-Za-z0-9_-]{1,40}$/.test(n);

/** Whether `trigger` holds for a tab `width` px wide. */
export function triggered(trigger: ThemeTrigger | null, width: number): boolean {
  if (trigger === null) return false;
  if (trigger.max_width !== undefined && width > trigger.max_width) return false;
  if (trigger.min_width !== undefined && width < trigger.min_width) return false;
  return trigger.max_width !== undefined || trigger.min_width !== undefined;
}

/** The theme a choice means in a tab `width` px wide, among `defs`. */
export function resolveTheme(choice: unknown, width: number, defs: ReadonlyArray<ThemeDef> = STOCK_THEMES): Theme {
  if (typeof choice === "string" && choice !== "auto" && defs.some((d) => d.name === choice)) return choice;
  const hit = defs.find((d) => triggered(d.trigger, width));
  if (hit !== undefined) return hit.name;
  return (defs.find((d) => d.trigger === null) ?? defs[0] ?? STOCK_THEMES[0]!).name;
}

const isTrigger = (v: unknown): v is ThemeTrigger => {
  if (v === null || typeof v !== "object" || Array.isArray(v)) return false;
  const t = v as Record<string, unknown>;
  return ["max_width", "min_width"].every((k) => t[k] === undefined || (typeof t[k] === "number" && Number.isFinite(t[k])));
};
/** A stored theme, checked — or null. */
export function themeDefOf(v: unknown): ThemeDef | null {
  const r = v as Record<string, unknown> | null;
  if (r === null || typeof r !== "object") return null;
  const name = typeof r["name"] === "string" ? r["name"] : "";
  if (!themeNameOk(name)) return null;
  const layout = r["layout"] === "column" ? "column" : r["layout"] === "free" ? "free" : null;
  if (layout === null) return null;
  const trigger = r["trigger"] === null || r["trigger"] === undefined ? null : isTrigger(r["trigger"]) ? (r["trigger"] as ThemeTrigger) : null;
  return { name, layout, trigger, ...(typeof r["note"] === "string" ? { note: r["note"] } : {}) };
}

/** The themes `view` has: the user's (its canvas theme agent's), then the stock
 *  ones not hidden — a user theme of a stock name takes its place. */
export function themesOf(kernel: Kernel, view: string): ThemeDef[] {
  const owner = [...(kernel.get(view)?.children.values() ?? [])].find((a) => a.record().handler_module === CANVAS_THEME_AGENT);
  const meta = owner?.meta ?? {};
  const user = (Array.isArray(meta["user_themes"]) ? (meta["user_themes"] as unknown[]) : []).map(themeDefOf).filter((d): d is ThemeDef => d !== null);
  const hidden = new Set(Array.isArray(meta["hidden_themes"]) ? (meta["hidden_themes"] as unknown[]).filter((x): x is string => typeof x === "string") : []);
  const stock = STOCK_THEMES.filter((d) => !hidden.has(d.name) && !user.some((u) => u.name === d.name));
  return [...user, ...stock];
}

/** How `theme` lays out `view`'s cells (an unknown theme: free). */
export const layoutOf = (kernel: Kernel, view: string, theme: Theme): ThemeLayout =>
  themesOf(kernel, view).find((d) => d.name === theme)?.layout ?? "free";

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

/** MOBILE REORDER: the `y` that puts cell `id`, dropped with its top at `top`,
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

/** Where a theme of layout `kind` puts a cell it has no rect for. */
export function assign(kind: ThemeLayout, occupied: Rect[], size: { width: number; height: number }, center: { x: number; y: number }): Rect {
  return kind === "column" ? columnSpot(occupied, size.height) : freeSpot(occupied, size.width, size.height, center);
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
  return resolveTheme(kernel.get(view)?.meta["theme"], width, themesOf(kernel, view));
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

/** The cells MINIMIZED in `theme` — shown as their head bar alone (a double
 *  click on the head toggles it). */
export function minimizedOf(kernel: Kernel, view: string, theme: Theme): Set<string> {
  const a = kernel.get(layoutId(theme));
  const m = a !== undefined && a.parentId === view ? a.meta["minimized"] : null;
  return new Set(Array.isArray(m) ? m.filter((x): x is string => typeof x === "string") : []);
}

/** Minimize `cell` in `theme` (`on` false: back to its full size). */
export function setMinimized(kernel: Kernel, view: string, theme: Theme, cell: string, on: boolean): void {
  if (kernel.get(layoutId(theme)) === undefined) writeRects(kernel, view, theme, {});
  const m = minimizedOf(kernel, view, theme);
  if (on) m.add(cell);
  else m.delete(cell);
  kernel.updateMeta(layoutId(theme), { minimized: [...m] } as unknown as Payload);
}

/** The cells of `view` HIDDEN (#133): out of the view, their agents running. Per
 *  view, not per theme — it is on the view's own record. */
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

/** Drop every rect of cells no longer in `view` (all themes). */
export function prune(kernel: Kernel, view: string, live: Set<string>): void {
  for (const h of hiddenOf(kernel, view)) {
    // a chat card's hide (`embed:<chat cell>:<agent>`, #339) lives while its chat cell does
    const chat = /^embed:([^:]+):/.exec(h)?.[1];
    if (!live.has(chat ?? h)) setHidden(kernel, view, h, false);
  }
  // every layout the view holds (a deleted theme's too)
  const themes = [...(kernel.get(view)?.children.values() ?? [])]
    .filter((a) => a.record().handler_module === LAYOUT)
    .map((a) => (typeof a.meta["theme"] === "string" ? (a.meta["theme"] as string) : a.id.replace(/^layout_/, "")));
  for (const t of themes) {
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
    rects[c.id] = assign(layoutOf(kernel, view, theme), Object.values(rects), c.size, center);
    added = true;
  }
  if (added) writeRects(kernel, view, theme, rects);
  return rects;
}

export const LAYOUT_README =
  "A view's LAYOUT for one theme: `rects: {<cell id>: {x, y, width, height}}` — where each cell sits in that theme. " +
  "The cells are untouched by it; the view's `theme` verb picks which theme is in use (auto: the first whose width trigger matches the tab — stock `mobile` under 768px wide). " +
  "Themes are listed, added and edited on the canvas theme agent (`themes`, `add_theme`…). " +
  "Move cells with the view's `move`; a cell with no rect in a theme is placed by its layout (free: free space near the centre; column: the column's bottom). " +
  "`minimized: [<cell id>…]`: those cells show their head bar alone (a double click on the head toggles it). " +
  "`pinned: <cell id>` (a column theme): that cell is held on the screen's bottom while the column scrolls (the 📌 on its head). " +
  "The view's `hidden: [<cell id>…]` (on the view, all themes) are the cells not shown: their agents run on and their rects are kept, so `show` puts them back where they were.";

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
