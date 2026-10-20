import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { registerCanvas, VIEW_VERBS } from "../src/bundles/canvas/canvas_agent.ts";
import { registerHtmlAgent } from "../src/bundles/html_agent/html_agent.ts";
import { hiddenOf, prune, registerLayout, rectsOf, setHidden, showHidden } from "../src/bundles/canvas/layout.ts";
import { setCanvasNode } from "../src/bundles/canvas/host_views.ts";

// #133: `hide` takes a cell off its view and keeps its agent; `show` puts it back
// where it was (or at x, y). The hidden set lives on the view, so it persists.

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

test("hide and show are view verbs", () => {
  assert.ok((VIEW_VERBS as readonly string[]).includes("hide"));
  assert.ok((VIEW_VERBS as readonly string[]).includes("show"));
});

test("hide takes the cell off the view and keeps its agent, record and rect", async () => {
  const k = page();
  await call(k, { type: "place", kind: "html", id: "a", x: 10, y: 20, html: "<p>a</p>" });
  const before = { ...k.get("a")!.record() };
  assert.deepEqual(await call(k, { type: "hide", id: "a" }), { hidden: "a", hidden_cells: ["a"] });
  assert.ok(k.get("a") !== undefined, "the agent is still there");
  assert.equal(k.get("a")?.parentId, "view_root", "still a child of the view");
  assert.equal(k.get("a")?.meta["html"], "<p>a</p>", "its record is untouched");
  assert.equal(k.get("a")?.record().handler_module, before.handler_module);
  assert.deepEqual(rectsOf(k, "view_root", "canvas")["a"], { x: 10, y: 20, width: 480, height: 320 }, "its rect is kept");
});

test("hide is idempotent; the hidden set holds each cell once", async () => {
  const k = page();
  await call(k, { type: "place", kind: "html", id: "a", x: 0, y: 0 });
  await call(k, { type: "hide", id: "a" });
  assert.deepEqual(await call(k, { type: "hide", id: "a" }), { hidden: "a", hidden_cells: ["a"] });
});

test("hide refuses what is no cell of the view, and the view's own background or layout", async () => {
  const k = page();
  assert.equal((await call(k, { type: "hide", id: "ghost" }))["reason"], "not_found");
  k.register(new Agent({ id: "bg", parentId: "view_root", handlerModule: "background.ts" }));
  const r = await call(k, { type: "hide", id: "bg" });
  assert.equal(r["reason"], "bad_arg");
  assert.equal(r["arg"], "id");
  assert.deepEqual([...hiddenOf(k, "view_root")], [], "nothing hidden");
});

test("show puts a hidden cell back where it was, or at x, y in the theme in use", async () => {
  const k = page();
  await call(k, { type: "place", kind: "html", id: "a", x: 10, y: 20, html: "<p>a</p>" });
  await call(k, { type: "hide", id: "a" });
  assert.deepEqual(await call(k, { type: "show", id: "a" }), {
    shown: "a",
    theme: "canvas",
    hidden_cells: [],
  });
  assert.deepEqual([...hiddenOf(k, "view_root")], []);
  assert.deepEqual(rectsOf(k, "view_root", "canvas")["a"], { x: 10, y: 20, width: 480, height: 320 });
  await call(k, { type: "hide", id: "a" });
  const moved = await call(k, { type: "show", id: "a", x: 300, y: 40 });
  assert.equal(moved["shown"], "a");
  assert.deepEqual(rectsOf(k, "view_root", "canvas")["a"], { x: 300, y: 40, width: 480, height: 320 });
});

test("show of a cell not hidden is harmless; show refuses what is no cell of the view", async () => {
  const k = page();
  await call(k, { type: "place", kind: "html", id: "a", x: 0, y: 0 });
  assert.equal((await call(k, { type: "show", id: "a" }))["shown"], "a");
  assert.equal((await call(k, { type: "show", id: "ghost" }))["reason"], "not_found");
});

test("the hidden set is on the view's record, so a reload that restores it keeps the cell hidden", async () => {
  const k = page();
  await call(k, { type: "place", kind: "html", id: "a", x: 0, y: 0 });
  await call(k, { type: "hide", id: "a" });
  const saved = JSON.parse(JSON.stringify(k.get("view_root")!.meta)) as Record<string, unknown>;
  assert.deepEqual(saved["hidden"], ["a"], "stored on the view");
  // a fresh page over the same record: the cell is still hidden
  const again = page();
  again.updateMeta("view_root", { hidden: saved["hidden"] } as unknown as Payload);
  assert.deepEqual([...hiddenOf(again, "view_root")], ["a"]);
});

test("hidden is per view: another view's cells are not hidden", async () => {
  const k = page();
  k.register(new Agent({ id: "view_two", parentId: "canvas", handlerModule: "view.ts" }));
  setHidden(k, "view_root", "a", true);
  assert.deepEqual([...hiddenOf(k, "view_root")], ["a"]);
  assert.equal(hiddenOf(k, "view_two").size, 0);
});

test("a removed cell leaves the hidden set with its rects", async () => {
  const k = page();
  await call(k, { type: "place", kind: "html", id: "a", x: 0, y: 0 });
  await call(k, { type: "hide", id: "a" });
  assert.equal((await call(k, { type: "remove", id: "a" }))["deleted"], true);
  assert.deepEqual([...hiddenOf(k, "view_root")], [], "a hidden cell removed is dropped from the set");
});

test("show of a cell that is not hidden keeps its place: x, y do not move it", async () => {
  const k = page();
  await call(k, { type: "place", kind: "html", id: "a", x: 10, y: 20, html: "<p>a</p>" });
  assert.deepEqual(await call(k, { type: "show", id: "a", x: 300, y: 40 }), {
    shown: "a",
    theme: "canvas",
    hidden_cells: [],
  });
  assert.deepEqual(rectsOf(k, "view_root", "canvas")["a"], { x: 10, y: 20, width: 480, height: 320 }, "the rect is not moved");
});

test("prune clears a hidden id whose cell is no longer live, and keeps a live one hidden", async () => {
  const k = page();
  await call(k, { type: "place", kind: "html", id: "a", x: 0, y: 0 });
  await call(k, { type: "place", kind: "html", id: "b", x: 0, y: 0 });
  await call(k, { type: "hide", id: "a" });
  await call(k, { type: "hide", id: "b" });
  // removed straight from the kernel: no view verb drops it from the hidden set
  k.remove("a");
  assert.deepEqual([...hiddenOf(k, "view_root")].sort(), ["a", "b"], "the stale id is still listed");
  prune(k, "view_root", new Set(["b"]));
  assert.deepEqual([...hiddenOf(k, "view_root")], ["b"], "only the removed cell's id goes");
});

test("showHidden brings a hidden cell back (its rect kept) and reports whether it was hidden", async () => {
  const k = page();
  await call(k, { type: "place", kind: "html", id: "a", x: 10, y: 20, html: "<p>a</p>" });
  assert.equal(showHidden(k, "a"), false, "a shown cell is left as it is");
  await call(k, { type: "hide", id: "a" });
  assert.equal(showHidden(k, "a"), true);
  assert.deepEqual([...hiddenOf(k, "view_root")], []);
  assert.deepEqual(rectsOf(k, "view_root", "canvas")["a"], { x: 10, y: 20, width: 480, height: 320 });
  assert.equal(showHidden(k, "ghost"), false, "an unknown id is not a hidden cell");
});
