import { test } from "node:test";
import assert from "node:assert/strict";
import { allHidden, forest, glyph } from "../src/bundles/canvas/sidebar.ts";

test("forest nests by parent_id, sorted, orphans become roots", () => {
  const f = forest([
    { id: "kernel_state", memory_store_id: "memory" },
    { id: "web", parent_id: "kernel_state" },
    { id: "canvas_ws", parent_id: "web" },
    { id: "aimem_1", parent_id: "canvas_ws", handler_module: "ram_store.tools" },
    { id: "ai_1", parent_id: "aimem_1", memory_store_id: "aimem_1" },
    { id: "b", parent_id: "kernel_state" },
    { id: "stray", parent_id: "gone" },
  ]);
  assert.deepEqual(f.map((n) => n.id), ["kernel_state", "stray"]);
  assert.deepEqual(f[0]!.children.map((n) => n.id), ["b", "web"]);
  const ai = f[0]!.children[1]!.children[0]!.children[0]!.children[0]!;
  assert.equal(ai.id, "ai_1");
  assert.equal(glyph(ai), "✦");
  assert.equal(glyph(f[0]!), "◇", "the root names the project store; it is not an AI");
  assert.equal(glyph(f[0]!.children[1]!.children[0]!.children[0]!), "▣");
});

test("an agent is hidden when ALL its cells are off their views (#133); no cell is not hidden", () => {
  const hidden = new Set(["c1", "c2"]);
  const isHidden = (c: string): boolean => hidden.has(c);
  assert.equal(allHidden([], isHidden), false, "no cell: shown in no view, not hidden");
  assert.equal(allHidden(["c1"], isHidden), true);
  assert.equal(allHidden(["c1", "c2"], isHidden), true);
  assert.equal(allHidden(["c1", "c3"], isHidden), false, "one cell still shows it");
  assert.equal(allHidden(["c3"], isHidden), false);
});
