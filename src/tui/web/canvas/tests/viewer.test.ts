import { test } from "node:test";
import assert from "node:assert/strict";
import { CANVAS, containerOf, hashFor, isContainer, isFocusClick, pathTo, viewFromHash } from "../src/bundles/canvas/viewer.ts";
import type { Node } from "../src/bundles/canvas/viewer.ts";

// root canvas ▸ a child canvas ▸ an html cell on it; a terminal view on the root
const tree: Node[] = [
  { id: "canvas", parentId: null, handler: CANVAS },
  { id: "canvas_kid", parentId: "canvas", handler: CANVAS },
  { id: "html_1", parentId: "canvas_kid", handler: "html_agent.ts" },
  { id: "view_t", parentId: "canvas", handler: "terminal_view.ts" },
];
const get = (id: string): Node | undefined => tree.find((n) => n.id === id);

test("a canvas is a container; a cell is drawn on its nearest canvas", () => {
  assert.ok(isContainer(get, "canvas_kid"));
  assert.ok(!isContainer(get, "html_1"));
  assert.equal(containerOf(get, "html_1"), "canvas_kid");
  assert.equal(containerOf(get, "canvas_kid"), "canvas_kid", "a canvas contains itself");
  assert.equal(containerOf(get, "view_t"), "canvas");
  assert.equal(containerOf(get, "nope"), null);
});

test("the breadcrumb runs root → the viewed agent", () => {
  assert.deepEqual(pathTo(get, "html_1"), ["canvas", "canvas_kid", "html_1"]);
  assert.deepEqual(pathTo(get, "canvas"), ["canvas"]);
});

test("#view=<id> is the link, round trip", () => {
  assert.equal(viewFromHash(hashFor("canvas_kid")), "canvas_kid");
  assert.equal(viewFromHash("#x=1&view=html_1"), "html_1");
  assert.equal(viewFromHash(""), null);
});

test("focus is ⌘-click on macOS, Ctrl-click elsewhere — Ctrl-click is the Mac's right click", () => {
  assert.ok(isFocusClick({ metaKey: true, ctrlKey: false }, true));
  assert.ok(!isFocusClick({ metaKey: false, ctrlKey: true }, true));
  assert.ok(isFocusClick({ metaKey: false, ctrlKey: true }, false));
  assert.ok(!isFocusClick({ metaKey: true, ctrlKey: false }, false));
});
