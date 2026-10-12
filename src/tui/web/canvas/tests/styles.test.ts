import { test } from "node:test";
import assert from "node:assert/strict";
import { CANVAS_CSS } from "../src/bundles/canvas/styles.ts";

// desktop hides the chrome off hover; a minimized cell is its bar alone, so the
// bar must show regardless or the cell vanishes (owner 2026-10-12)
test("a minimized cell's head bar and bezel show without hover", () => {
  const css = CANVAS_CSS.replace(/\s+/g, " ");
  assert.match(css, /\.agent-frame\.minimized > \.frame-head \{ opacity: 1 !important; pointer-events: auto !important; \}/);
  assert.match(css, /\.agent-frame\.minimized::before \{ opacity: 1 !important; \}/);
});

test("a minimized cell hides its body, not unmounts it", () => {
  const css = CANVAS_CSS.replace(/\s+/g, " ");
  assert.match(css, /\.agent-frame\.minimized > \.frame-body,[^{]*\{ visibility: hidden;/);
});

test("a stopped terminal's overlay covers the cell; its ▶ buttons take clicks", () => {
  const css = CANVAS_CSS.replace(/\s+/g, " ");
  assert.match(css, /\.term-stopped \{ position: absolute; inset: 0;/);
  assert.match(css, /\.term-stopped\[hidden\] \{ display: none; \}/);
  assert.match(css, /\.term-stopped button \{[^}]*cursor: pointer;/);
});
