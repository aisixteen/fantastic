import { test } from "node:test";
import assert from "node:assert/strict";
import { CELL_MENU, closeIntent, isCellHandler } from "../src/bundles/canvas/cell_menu.ts";

// #133: ✕ hides a cell, Shift+✕ deletes it; the head's menu offers both.

test("✕ hides; Shift+✕ deletes", () => {
  assert.equal(closeIntent(false), "hide");
  assert.equal(closeIntent(true), "delete");
});

test("the head menu offers Hide and Delete, in that order", () => {
  assert.deepEqual(
    CELL_MENU.map((m) => [m.intent, m.label]),
    [
      ["hide", "Hide"],
      ["delete", "Delete"],
    ],
  );
});

test("the page's own agents are no cells: a background, a popup and a layout", () => {
  assert.equal(isCellHandler("background.ts"), false);
  assert.equal(isCellHandler("selector.ts"), false);
  assert.equal(isCellHandler("layout.ts"), false);
  assert.equal(isCellHandler("terminal_view.ts"), true);
  assert.equal(isCellHandler("html_agent.ts"), true);
});
