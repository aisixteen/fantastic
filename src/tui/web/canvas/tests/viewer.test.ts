import { test } from "node:test";
import assert from "node:assert/strict";
import { VIEW, containerOf, hashFor, isContainer, isFocusClick, pathTo, viewFromHash } from "../src/bundles/canvas/viewer.ts";
import type { Node } from "../src/bundles/canvas/viewer.ts";

// the canvas ▸ the root view ▸ a nested view ▸ an html cell on it; a terminal on the root view
const tree: Node[] = [
  { id: "canvas", parentId: null, handler: "canvas.ts" },
  { id: "view_root", parentId: "canvas", handler: VIEW },
  { id: "view_kid", parentId: "view_root", handler: VIEW },
  { id: "html_1", parentId: "view_kid", handler: "html_agent.ts" },
  { id: "cell_t", parentId: "view_root", handler: "terminal_view.ts" },
];
const get = (id: string): Node | undefined => tree.find((n) => n.id === id);

test("a view is a container; a cell is drawn on its nearest view", () => {
  assert.ok(isContainer(get, "view_kid"));
  assert.ok(!isContainer(get, "html_1"));
  assert.ok(!isContainer(get, "canvas"), "the canvas root holds views; it is not one");
  assert.equal(containerOf(get, "html_1"), "view_kid");
  assert.equal(containerOf(get, "view_kid"), "view_kid", "a view contains itself");
  assert.equal(containerOf(get, "cell_t"), "view_root");
  assert.equal(containerOf(get, "nope"), null);
});

test("the breadcrumb runs root → the viewed agent", () => {
  assert.deepEqual(pathTo(get, "html_1"), ["canvas", "view_root", "view_kid", "html_1"]);
  assert.deepEqual(pathTo(get, "canvas"), ["canvas"]);
});

test("#view=<id> is the link, round trip", () => {
  assert.equal(viewFromHash(hashFor("view_kid")), "view_kid");
  assert.equal(viewFromHash("#x=1&view=html_1"), "html_1");
  assert.equal(viewFromHash(""), null);
});

test("focus is ⌘-click on macOS, Ctrl-click elsewhere — Ctrl-click is the Mac's right click", () => {
  assert.ok(isFocusClick({ metaKey: true, ctrlKey: false }, true));
  assert.ok(!isFocusClick({ metaKey: false, ctrlKey: true }, true));
  assert.ok(isFocusClick({ metaKey: false, ctrlKey: true }, false));
  assert.ok(!isFocusClick({ metaKey: true, ctrlKey: false }, false));
});
