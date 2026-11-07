import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { registerCanvas } from "../src/bundles/canvas/canvas_agent.ts";
import { registerHtmlAgent } from "../src/bundles/html_agent/html_agent.ts";
import { setCanvasNode } from "../src/bundles/canvas/host_views.ts";
import {
  COLUMN, GAP, MOBILE_MAX_WIDTH, assign, framesToRemove, columnSpot, stack, freeSpot, layoutId, pinnedOf, placeMissing, prune, rectsOf, registerLayout, layoutFor, setPinned, setRect,
} from "../src/bundles/canvas/layout.ts";
import type { Rect } from "../src/bundles/canvas/layout.ts";

// LAYOUTS: a cell's rect lives in its view's layout (the free canvas, or the
// phone's one column), never on the cell; a cell new to a layout is placed by it.

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

test("the layout is by the tab's width alone: the column under the 768px breakpoint, the free canvas at and over it", () => {
  assert.equal(layoutFor(MOBILE_MAX_WIDTH - 1), "column");
  assert.equal(layoutFor(MOBILE_MAX_WIDTH), "free");
  assert.equal(layoutFor(500), "column");
  assert.equal(layoutFor(1200), "free");
  assert.equal(layoutFor(Infinity), "free", "no window counts as wide");
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

test("column: the bottom of the column, full column width, its own height", () => {
  assert.deepEqual(columnSpot([], 300), { x: COLUMN.x, y: COLUMN.top, width: COLUMN.width, height: 300 });
  const a = columnSpot([], 300);
  const b = columnSpot([a], 200);
  assert.equal(b.y, a.y + a.height + COLUMN.gap);
  assert.deepEqual(assign("column", [a, b], { width: 900, height: 120 }, { x: 5000, y: 5000 }).y, b.y + b.height + COLUMN.gap, "the centre is the free canvas's, not the column's");
});

test("the view's place / move / update write the layout in use — never the cell", async () => {
  const k = page();
  await call(k, { type: "place", kind: "html", id: "a", x: 10, y: 20, width: 300, height: 200, bytes: "<p>a</p>" });
  for (const f of ["x", "y", "width", "height"]) assert.equal(k.get("a")?.meta[f], undefined, `${f} is not on the cell`);
  assert.deepEqual(rectsOf(k, "view_root", "free")["a"], { x: 10, y: 20, width: 300, height: 200 });
  assert.equal(k.get(layoutId("free"))?.parentId, "view_root", "the layout is an agent of the view");
  await call(k, { type: "move", id: "a", y: 70 });
  await call(k, { type: "update", id: "a", fields: { width: 350, display_name: "A" } });
  assert.deepEqual(rectsOf(k, "view_root", "free")["a"], { x: 10, y: 70, width: 350, height: 200 });
  assert.equal(k.get("a")?.meta["display_name"], "A");
  assert.equal(k.get("a")?.meta["width"], undefined);
});

test("the view has no `theme` verb; a 500px tab writes the column's layout (layout_mobile), a wide one layout_canvas", async () => {
  const k = page();
  const u = await call(k, { type: "theme", name: "mobile" });
  assert.equal(u["reason"], "unhandled");
  assert.ok(!String(u["hint"]).includes("theme"), "the verbs it offers have no theme");
  await call(k, { type: "place", kind: "html", id: "a", x: 10, y: 20, bytes: "<p>a</p>" });
  assert.equal(k.get("view_root")?.meta["theme"], undefined);
  const g = globalThis as { innerWidth?: number };
  g.innerWidth = 500;
  try {
    await call(k, { type: "move", id: "a", y: 500 });
    assert.equal(rectsOf(k, "view_root", "column")["a"]?.y, 500, "a phone-wide tab moves the cell in the column's layout");
    assert.equal(k.get("layout_mobile")?.parentId, "view_root");
    g.innerWidth = 1200;
    await call(k, { type: "move", id: "a", y: 30 });
    assert.equal(rectsOf(k, "view_root", "free")["a"]?.y, 30, "a wide tab moves it in the free canvas's layout");
    assert.equal(rectsOf(k, "view_root", "column")["a"]?.y, 500, "the column's layout is untouched");
  } finally {
    delete g.innerWidth;
  }
});

test("a stored `theme` on the view record is ignored: the wide tab's cells sit in layout_canvas", async () => {
  const k = page();
  k.updateMeta("view_root", { theme: "mobile", user_themes: [{ name: "x" }] } as unknown as Payload);
  await call(k, { type: "place", kind: "html", id: "a", x: 10, y: 20, bytes: "<p>a</p>" });
  assert.deepEqual(rectsOf(k, "view_root", "free")["a"], { x: 10, y: 20, width: 480, height: 320 });
  assert.equal(k.get("layout_mobile"), undefined);
});

test("a cell made in one layout is placed by the other: the column appends it to its bottom, the free canvas finds free space", () => {
  const k = page();
  setRect(k, "view_root", "column", "top", { x: 0, y: 16, width: 400, height: 300 });
  setRect(k, "view_root", "free", "made_on_canvas", { x: 2000, y: 900, width: 480, height: 320 });
  const m = placeMissing(k, "view_root", "column", [{ id: "top", size: { width: 480, height: 320 } }, { id: "made_on_canvas", size: { width: 480, height: 320 } }], { x: 0, y: 0 });
  assert.deepEqual(m["made_on_canvas"], { x: COLUMN.x, y: 16 + 300 + COLUMN.gap, width: COLUMN.width, height: 320 });
  assert.deepEqual(m["top"], { x: 0, y: 16, width: 400, height: 300 }, "a cell already placed stays");
  setRect(k, "view_root", "column", "made_on_phone", { x: 0, y: 700, width: 400, height: 200 });
  const c = placeMissing(k, "view_root", "free", [{ id: "made_on_canvas", size: { width: 480, height: 320 } }, { id: "made_on_phone", size: { width: 400, height: 200 } }], { x: 2240, y: 1060 });
  assert.ok(clear(c["made_on_phone"]!, c["made_on_canvas"]!), JSON.stringify(c));
  assert.equal(c["made_on_phone"]!.width, 400, "its size from the layout it was made in");
});

test("a removed cell's rects go from both layouts", () => {
  const k = page();
  setRect(k, "view_root", "free", "a", { x: 1, y: 1 });
  setRect(k, "view_root", "column", "a", { x: 0, y: 16 });
  setRect(k, "view_root", "free", "b", { x: 600, y: 1 });
  prune(k, "view_root", new Set(["b"]));
  assert.deepEqual(Object.keys(rectsOf(k, "view_root", "free")), ["b"]);
  assert.deepEqual(Object.keys(rectsOf(k, "view_root", "column")), []);
});

test("prune removes a layout a stored user theme left (layout_<name>) and keeps layout_canvas and layout_mobile", () => {
  const k = page();
  setRect(k, "view_root", "free", "a", { x: 1, y: 1 });
  setRect(k, "view_root", "column", "a", { x: 0, y: 16 });
  k.register(new Agent({ id: "layout_wide", parentId: "view_root", handlerModule: "layout.ts", meta: { theme: "wide", rects: { a: { x: 5, y: 5, width: 9, height: 9 } } } as unknown as Payload }));
  prune(k, "view_root", new Set(["a"]));
  assert.equal(k.get("layout_wide"), undefined, "the user theme's layout is pruned");
  assert.ok(k.get("layout_canvas") !== undefined && k.get("layout_mobile") !== undefined, "the two layouts stay");
  assert.deepEqual(rectsOf(k, "view_root", "column")["a"], { x: 0, y: 16, width: 480, height: 320 });
});

test("column pin: one cell per layout, a new pin replaces it, a removed cell is unpinned", () => {
  const k = page();
  assert.equal(pinnedOf(k, "view_root", "column"), null);
  setRect(k, "view_root", "column", "chat", { x: 0, y: 16 });
  setPinned(k, "view_root", "column", "chat");
  assert.equal(pinnedOf(k, "view_root", "column"), "chat");
  setPinned(k, "view_root", "column", "term");
  assert.equal(pinnedOf(k, "view_root", "column"), "term", "one pinned: the new one replaces it");
  assert.equal(pinnedOf(k, "view_root", "free"), null, "the free canvas pins nothing");
  setRect(k, "view_root", "column", "term", { x: 0, y: 400 });
  prune(k, "view_root", new Set(["chat"]));
  assert.equal(pinnedOf(k, "view_root", "column"), null, "its cell gone, the pin goes");
});

test("column stack: one full-width column in the cells' order, gap apart, heights kept", () => {
  const s = stack({ b: { x: 300, y: 900, width: 200, height: 100 }, a: { x: 0, y: 16, width: 400, height: 300 } });
  assert.deepEqual(s["a"], { x: COLUMN.x, y: COLUMN.top, width: COLUMN.width, height: 300 });
  assert.deepEqual(s["b"], { x: COLUMN.x, y: COLUMN.top + 300 + COLUMN.gap, width: COLUMN.width, height: 100 });
});

test("minimized: a cell is its bar alone — the column's next cell follows its head; per layout; pruned", async () => {
  const { minimizedOf, setMinimized } = await import("../src/bundles/canvas/layout.ts");
  const s = stack({ a: { x: 0, y: 0, width: 400, height: 300 }, b: { x: 0, y: 500, width: 400, height: 100 } }, new Set(["a"]));
  assert.equal(s["b"]!.y, COLUMN.top + COLUMN.gap, "b sits right under a's bar");
  assert.equal(s["a"]!.height, 300, "a keeps its size for when it comes back");
  const k = page();
  setMinimized(k, "view_root", "column", "a", true);
  assert.deepEqual([...minimizedOf(k, "view_root", "column")], ["a"]);
  assert.equal(minimizedOf(k, "view_root", "free").size, 0, "per layout");
  setMinimized(k, "view_root", "column", "a", false);
  assert.equal(minimizedOf(k, "view_root", "column").size, 0);
});

test("column reorder: a dropped cell takes the slot its centre is over; the column restacks", async () => {
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
