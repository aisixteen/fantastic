import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { setCanvasNode } from "../src/bundles/canvas/host_views.ts";
import { registerCanvas } from "../src/bundles/canvas/canvas_agent.ts";
import { registerHtmlAgent } from "../src/bundles/html_agent/html_agent.ts";
import { registerLayout, rectsOf } from "../src/bundles/canvas/layout.ts";
import { FIT_MIN, fitSize, forgetSize, nextSize, reportSize, sizeOf } from "../src/bundles/canvas/fit.ts";

test("fitSize: the content plus the frame's chrome, clamped, overflow said", () => {
  assert.deepEqual(fitSize({ width: 300.2, height: 200 }, { width: 2, height: 2 }), { width: 303, height: 202, overflowed: false });
  assert.deepEqual(fitSize({ width: 10, height: 5 }, { width: 0, height: 0 }), { ...FIT_MIN, overflowed: false }, "never below the minimum");
  assert.deepEqual(fitSize({ width: 900, height: 900 }, { width: 0, height: 0 }, { width: 500, height: 400 }), { width: 500, height: 400, overflowed: true });
});

test("a panel's reports: the last kept, the next awaited, none in time is null", async () => {
  forgetSize("p");
  assert.equal(sizeOf("p"), undefined);
  const next = nextSize("p", 1000);
  reportSize("p", { mw: 120, h: 80 });
  assert.deepEqual(await next, { mw: 120, h: 80 });
  assert.deepEqual(sizeOf("p"), { mw: 120, h: 80 });
  reportSize("p", { mw: Number.NaN, h: 1 });
  assert.deepEqual(sizeOf("p"), { mw: 120, h: 80 }, "a bad report is dropped");
  assert.equal(await nextSize("p", 20), null);
  forgetSize("p");
  assert.equal(sizeOf("p"), undefined);
});

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

test("the view's fit: the width the content wants, then its height at that width", async () => {
  const k = page();
  await call(k, { type: "place", kind: "html", id: "f", x: 10, y: 20, width: 200, height: 100, html: "<p>long text</p>" });
  forgetSize("f");
  reportSize("f", { mw: 640, h: 300 }); // measured at 200 wide: the text wrapped
  // the panel measures again once it is 640 wide (as its connector does on a resize)
  setTimeout(() => reportSize("f", { mw: 640, h: 90 }), 30);
  const r = await call(k, { type: "fit", id: "f" });
  assert.equal(r["fitted"], "f");
  assert.equal(r["overflowed"], false);
  assert.deepEqual(rectsOf(k, "view_root", "canvas")["f"], { x: 10, y: 20, width: 640, height: 90 }, "the height is the one at the new width; x/y kept");
  // a max: clamped, and said
  // wider than the max but it WRAPS there: not overflowed
  forgetSize("f");
  reportSize("f", { mw: 2000, h: 50 });
  setTimeout(() => reportSize("f", { mw: 2000, h: 200, ox: false }), 30);
  const w = await call(k, { type: "fit", id: "f", max_width: 500, max_height: 300 });
  assert.equal(w["overflowed"], false, "a paragraph wrapped at the max fits");
  assert.deepEqual(rectsOf(k, "view_root", "canvas")["f"], { x: 10, y: 20, width: 500, height: 200 });
  // too tall for the max, or too wide to wrap: clamped, and said
  forgetSize("f");
  reportSize("f", { mw: 2000, h: 50 });
  setTimeout(() => reportSize("f", { mw: 2000, h: 400, ox: true }), 30);
  const c = await call(k, { type: "fit", id: "f", max_width: 500, max_height: 300 });
  assert.equal(c["overflowed"], true);
  assert.deepEqual(rectsOf(k, "view_root", "canvas")["f"], { x: 10, y: 20, width: 500, height: 300 });
});

test("fit refuses what is not its cell, and a cell that never measured", async () => {
  const k = page();
  assert.equal((await call(k, { type: "fit", id: "nope" }))["reason"], "not_found");
  await call(k, { type: "place", kind: "gl", id: "g", x: 0, y: 0 });
  forgetSize("g");
  const t0 = Date.now();
  const r = await call(k, { type: "fit", id: "g" });
  assert.equal(r["reason"], "not_ready");
  assert.ok(Date.now() - t0 >= 2900, "it waited for a first measure");
});
