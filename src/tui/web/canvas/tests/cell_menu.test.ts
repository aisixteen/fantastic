import { test } from "node:test";
import assert from "node:assert/strict";
import { CELL_MENU, closeIntent, exportCall, exportTarget, isCellHandler } from "../src/bundles/canvas/cell_menu.ts";

// #133: ✕ hides a cell, Shift+✕ deletes it; the head's menu offers both.

test("✕ hides; Shift+✕ deletes", () => {
  assert.equal(closeIntent(false), "hide");
  assert.equal(closeIntent(true), "delete");
});

test("the head menu offers Hide, Delete and Export…, in that order", () => {
  assert.deepEqual(
    CELL_MENU.map((m) => [m.intent, m.label]),
    [
      ["hide", "Hide"],
      ["delete", "Delete"],
      ["export", "Export…"],
    ],
  );
});

test("the head menu offers Export… with a title", () => {
  const e = CELL_MENU.find((m) => m.intent === "export");
  assert.equal(e?.label, "Export…");
  assert.match(e?.title ?? "", /blueprint folder/);
});

test("exportCall sends export_subtree with the cell's agent id and the path", () => {
  assert.deepEqual(exportCall("worker", "out/team"), { type: "export_subtree", agent_id: "worker", path: "out/team" });
});

test("exportTarget is the backend_id and undefined for a page-only cell", () => {
  assert.equal(exportTarget({ backend_id: "worker", kind: "html" }), "worker");
  assert.equal(exportTarget({ kind: "html" }), undefined);
  assert.equal(exportTarget({ backend_id: "" }), undefined);
  assert.equal(exportTarget({ backend_id: 3 }), undefined);
});

test("the page's own agents are no cells: a background, a popup and a layout", () => {
  assert.equal(isCellHandler("background.ts"), false);
  assert.equal(isCellHandler("selector.ts"), false);
  assert.equal(isCellHandler("layout.ts"), false);
  assert.equal(isCellHandler("terminal_view.ts"), true);
  assert.equal(isCellHandler("html_agent.ts"), true);
});
