import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { registerCanvas } from "../src/bundles/canvas/canvas_agent.ts";
import { registerHtmlAgent } from "../src/bundles/html_agent/html_agent.ts";
import { setCanvasNode } from "../src/bundles/canvas/host_views.ts";
import {
  COLUMN, GAP, MOBILE_MAX_WIDTH, assign, framesToRemove, columnSpot, stack, freeSpot, layoutId, pinnedOf, placeMissing, prune, rectsOf, registerLayout, resolveTheme, setPinned, setRect,
} from "../src/bundles/canvas/layout.ts";
import type { Rect } from "../src/bundles/canvas/layout.ts";

// THEMES: a cell's rect lives in its view's layout per theme (canvas | mobile),
// never on the cell; a cell new to a theme is placed by it.

function page(): Kernel {
  setCanvasNode("canvas_root");
  const k = new Kernel();
  registerCanvas(k);
  registerHtmlAgent(k);
  registerLayout(k);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k.register(new Agent({ id: "view_root", parentId: "canvas", handlerModule: "view.ts" }));
  return k;
}
const call = async (k: Kernel, payload: Record<string, unknown>): Promise<Payload> =>
  (await k.send([{ target: "view_root", payload: payload as Payload }]))[0]!.result as Payload;
const clear = (a: Rect, b: Rect): boolean =>
  a.x + a.width + GAP <= b.x || b.x + b.width + GAP <= a.x || a.y + a.height + GAP <= b.y || b.y + b.height + GAP <= a.y;

test("auto is mobile under the 768px breakpoint, canvas at and over it; a named theme is itself", () => {
  assert.equal(resolveTheme("auto", MOBILE_MAX_WIDTH - 1), "mobile");
  assert.equal(resolveTheme("auto", MOBILE_MAX_WIDTH), "canvas");
  assert.equal(resolveTheme(undefined, 390), "mobile", "no theme set = auto");
  assert.equal(resolveTheme("canvas", 390), "canvas");
  assert.equal(resolveTheme("mobile", 1920), "mobile");
});

test("canvas: an empty canvas places the cell centred on the centre", () => {
  assert.deepEqual(freeSpot([], 400, 200, { x: 1000, y: 500 }), { x: 800, y: 400, width: 400, height: 200 });
});

test("canvas: a busy centre places the cell in the nearest free space, clear of every cell", () => {
  const occ: Rect[] = [{ x: 800, y: 400, width: 400, height: 200 }];
  const r = freeSpot(occ, 400, 200, { x: 1000, y: 500 });
  assert.ok(occ.every((o) => clear(r, o)), JSON.stringify(r));
  // near: within a cell's size of the centre, not flung away
  assert.ok(Math.hypot(r.x + 200 - 1000, r.y + 100 - 500) < 700, JSON.stringify(r));
  // a ring of cells: still clear of all of them
  const ring: Rect[] = [];
  for (let i = -1; i <= 1; i++) for (let j = -1; j <= 1; j++) ring.push({ x: 800 + i * 460, y: 400 + j * 260, width: 400, height: 200 });
  const r2 = freeSpot(ring, 400, 200, { x: 1000, y: 500 });
  assert.ok(ring.every((o) => clear(r2, o)), JSON.stringify(r2));
});

test("mobile: the bottom of the column, full column width, its own height", () => {
  assert.deepEqual(columnSpot([], 300), { x: COLUMN.x, y: COLUMN.top, width: COLUMN.width, height: 300 });
  const a = columnSpot([], 300);
  const b = columnSpot([a], 200);
  assert.equal(b.y, a.y + a.height + COLUMN.gap);
  assert.deepEqual(assign("column", [a, b], { width: 900, height: 120 }, { x: 5000, y: 5000 }).y, b.y + b.height + COLUMN.gap, "the centre is the canvas theme's, not mobile's");
});

test("the view's place / move / update write the layout of the theme in use — never the cell", async () => {
  const k = page();
  await call(k, { type: "place", kind: "html", id: "a", x: 10, y: 20, width: 300, height: 200, bytes: "<p>a</p>" });
  for (const f of ["x", "y", "width", "height"]) assert.equal(k.get("a")?.meta[f], undefined, `${f} is not on the cell`);
  assert.deepEqual(rectsOf(k, "view_root", "canvas")["a"], { x: 10, y: 20, width: 300, height: 200 });
  assert.equal(k.get(layoutId("canvas"))?.parentId, "view_root", "the layout is an agent of the view");
  await call(k, { type: "move", id: "a", y: 70 });
  await call(k, { type: "update", id: "a", fields: { width: 350, display_name: "A" } });
  assert.deepEqual(rectsOf(k, "view_root", "canvas")["a"], { x: 10, y: 70, width: 350, height: 200 });
  assert.equal(k.get("a")?.meta["display_name"], "A");
  assert.equal(k.get("a")?.meta["width"], undefined);
});

test("theme: auto by default; named, the moves go to THAT layout, the other keeps its rects", async () => {
  const k = page();
  assert.deepEqual(await call(k, { type: "theme" }), { theme: "auto", in_use: "canvas", themes: ["auto", "canvas", "mobile"] });
  await call(k, { type: "place", kind: "html", id: "a", x: 10, y: 20, bytes: "<p>a</p>" });
  assert.equal((await call(k, { type: "theme", name: "mobile" }))["in_use"], "mobile");
  assert.equal(k.get("view_root")?.meta["theme"], "mobile");
  await call(k, { type: "move", id: "a", y: 500 });
  assert.equal(rectsOf(k, "view_root", "mobile")["a"]?.y, 500);
  assert.equal(rectsOf(k, "view_root", "canvas")["a"]?.y, 20, "the canvas layout is untouched");
  assert.equal((await call(k, { type: "theme", name: "tablet" }))["arg"], "name");
});

test("a cell made in one theme is placed by the other on switch: mobile appends it to the column's bottom, canvas finds free space", () => {
  const k = page();
  setRect(k, "view_root", "mobile", "top", { x: 0, y: 16, width: 400, height: 300 });
  setRect(k, "view_root", "canvas", "made_on_canvas", { x: 2000, y: 900, width: 480, height: 320 });
  const m = placeMissing(k, "view_root", "mobile", [{ id: "top", size: { width: 480, height: 320 } }, { id: "made_on_canvas", size: { width: 480, height: 320 } }], { x: 0, y: 0 });
  assert.deepEqual(m["made_on_canvas"], { x: COLUMN.x, y: 16 + 300 + COLUMN.gap, width: COLUMN.width, height: 320 });
  assert.deepEqual(m["top"], { x: 0, y: 16, width: 400, height: 300 }, "a cell already placed stays");
  setRect(k, "view_root", "mobile", "made_on_phone", { x: 0, y: 700, width: 400, height: 200 });
  const c = placeMissing(k, "view_root", "canvas", [{ id: "made_on_canvas", size: { width: 480, height: 320 } }, { id: "made_on_phone", size: { width: 400, height: 200 } }], { x: 2240, y: 1060 });
  assert.ok(clear(c["made_on_phone"]!, c["made_on_canvas"]!), JSON.stringify(c));
  assert.equal(c["made_on_phone"]!.width, 400, "its size from the theme it was made in");
});

test("a removed cell's rects go from every theme", () => {
  const k = page();
  setRect(k, "view_root", "canvas", "a", { x: 1, y: 1 });
  setRect(k, "view_root", "mobile", "a", { x: 0, y: 16 });
  setRect(k, "view_root", "canvas", "b", { x: 600, y: 1 });
  prune(k, "view_root", new Set(["b"]));
  assert.deepEqual(Object.keys(rectsOf(k, "view_root", "canvas")), ["b"]);
  assert.deepEqual(Object.keys(rectsOf(k, "view_root", "mobile")), []);
});

test("mobile pin: one cell per layout, a new pin replaces it, a removed cell is unpinned", () => {
  const k = page();
  assert.equal(pinnedOf(k, "view_root", "mobile"), null);
  setRect(k, "view_root", "mobile", "chat", { x: 0, y: 16 });
  setPinned(k, "view_root", "mobile", "chat");
  assert.equal(pinnedOf(k, "view_root", "mobile"), "chat");
  setPinned(k, "view_root", "mobile", "term");
  assert.equal(pinnedOf(k, "view_root", "mobile"), "term", "one pinned: the new one replaces it");
  assert.equal(pinnedOf(k, "view_root", "canvas"), null, "the canvas theme pins nothing");
  setRect(k, "view_root", "mobile", "term", { x: 0, y: 400 });
  prune(k, "view_root", new Set(["chat"]));
  assert.equal(pinnedOf(k, "view_root", "mobile"), null, "its cell gone, the pin goes");
});

test("mobile stack: one full-width column in the cells' order, gap apart, heights kept", () => {
  const s = stack({ b: { x: 300, y: 900, width: 200, height: 100 }, a: { x: 0, y: 16, width: 400, height: 300 } });
  assert.deepEqual(s["a"], { x: COLUMN.x, y: COLUMN.top, width: COLUMN.width, height: 300 });
  assert.deepEqual(s["b"], { x: COLUMN.x, y: COLUMN.top + 300 + COLUMN.gap, width: COLUMN.width, height: 100 });
});

test("minimized: a cell is its bar alone — the column's next cell follows its head; per theme; pruned", async () => {
  const { minimizedOf, setMinimized } = await import("../src/bundles/canvas/layout.ts");
  const s = stack({ a: { x: 0, y: 0, width: 400, height: 300 }, b: { x: 0, y: 500, width: 400, height: 100 } }, new Set(["a"]));
  assert.equal(s["b"]!.y, COLUMN.top + COLUMN.gap, "b sits right under a's bar");
  assert.equal(s["a"]!.height, 300, "a keeps its size for when it comes back");
  const k = page();
  setMinimized(k, "view_root", "mobile", "a", true);
  assert.deepEqual([...minimizedOf(k, "view_root", "mobile")], ["a"]);
  assert.equal(minimizedOf(k, "view_root", "canvas").size, 0, "per theme");
  setMinimized(k, "view_root", "mobile", "a", false);
  assert.equal(minimizedOf(k, "view_root", "mobile").size, 0);
});

test("mobile reorder: a dropped cell takes the slot its centre is over; the column restacks", async () => {
  const { reorderY } = await import("../src/bundles/canvas/layout.ts");
  const col = stack({ a: { x: 0, y: 0, width: 400, height: 100 }, b: { x: 0, y: 200, width: 400, height: 100 }, c: { x: 0, y: 400, width: 400, height: 100 } });
  const order = (r: Record<string, Rect>): string[] => Object.entries(stack(r)).sort((x, y) => x[1].y - y[1].y).map(([k]) => k);
  // c dragged to the top
  const up = { ...col, c: { ...col["c"]!, y: reorderY(col, "c", col["a"]!.y - 10) } };
  assert.deepEqual(order(up), ["c", "a", "b"]);
  // a dragged past b's centre
  const down = { ...col, a: { ...col["a"]!, y: reorderY(col, "a", col["b"]!.y + 10) } };
  assert.deepEqual(order(down), ["b", "a", "c"]);
  // a dragged to the bottom
  const last = { ...col, a: { ...col["a"]!, y: reorderY(col, "a", col["c"]!.y + 300) } };
  assert.deepEqual(order(last), ["b", "c", "a"]);
  // dropped back over its own slot: its own y, nothing moves
  assert.equal(reorderY(col, "b", col["b"]!.y + 5), col["b"]!.y);
  assert.equal(reorderY(col, "c", col["c"]!.y + 40), col["c"]!.y);
});

test("themes are data: stock canvas (free) + mobile (column, under 768px); the user's first; auto by the triggers", async () => {
  const { STOCK_THEMES, resolveTheme, themeDefOf, triggered } = await import("../src/bundles/canvas/layout.ts");
  assert.deepEqual(STOCK_THEMES.map((d) => [d.name, d.layout]), [["canvas", "free"], ["mobile", "column"]]);
  assert.ok(triggered({ max_width: 767 }, 767) && !triggered({ max_width: 767 }, 768) && !triggered(null, 100));
  const wide = themeDefOf({ name: "wide", layout: "column", trigger: { min_width: 2000 } })!;
  const defs = [wide, ...STOCK_THEMES];
  assert.equal(resolveTheme("auto", 2560, defs), "wide", "a user trigger wins");
  assert.equal(resolveTheme("auto", 1400, defs), "canvas", "none triggered: the first without a trigger");
  assert.equal(resolveTheme("auto", 500, defs), "mobile");
  assert.equal(resolveTheme("wide", 500, defs), "wide", "a named theme holds whatever the width");
  assert.equal(resolveTheme("gone", 1400, defs), "canvas", "an unknown name falls to auto");
  assert.equal(themeDefOf({ name: "bad name", layout: "free" }), null);
  assert.equal(themeDefOf({ name: "x", layout: "grid" }), null);
});

// A DROP OF TWO CELLS (#134): the first cell's mount is still awaiting when the second is shown,
// so the first refresh ends after the second mounted its frame. Its member list (A alone) is
// older: it must not take the second cell's frame down.
test("a refresh a newer one overtook takes no frame down", () => {
  const exists = (): boolean => true;
  // the newest refresh drew A and B: nothing to remove
  assert.deepEqual(framesToRemove(["a", "b"], new Set(["a", "b"]), exists, true), []);
  // the newest refresh sweeps what it did not draw, and what is gone
  assert.deepEqual(framesToRemove(["a", "b", "c"], new Set(["a"]), (id) => id !== "a", true), ["a", "b", "c"]);
  // the stale one drew A alone; B's frame (mounted by the newer) stays
  assert.deepEqual(framesToRemove(["a", "b"], new Set(["a"]), exists, false), []);
});
