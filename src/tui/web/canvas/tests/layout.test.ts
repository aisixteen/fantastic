import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { registerCanvas } from "../src/bundles/canvas/canvas_agent.ts";
import { registerHtmlAgent } from "../src/bundles/html_agent/html_agent.ts";
import { setCanvasNode } from "../src/bundles/canvas/host_views.ts";
import {
  COLUMN, GAP, MOBILE_MAX_WIDTH, assign, columnSpot, stack, freeSpot, layoutId, pinnedOf, placeMissing, prune, rectsOf, registerLayout, resolveTheme, setPinned, setRect,
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
  assert.deepEqual(assign("mobile", [a, b], { width: 900, height: 120 }, { x: 5000, y: 5000 }).y, b.y + b.height + COLUMN.gap, "the centre is the canvas theme's, not mobile's");
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
