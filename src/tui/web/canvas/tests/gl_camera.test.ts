import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { viewportMiddleInWorld } from "../src/bundles/canvas/gl_host.ts";
import { worldAt } from "../src/bundles/files/drag.ts";

// #148: the GL camera follows the canvas's own box, as the DOM world does. Beside the
// tree sidebar the canvas starts at its right edge, so the world point under the
// viewport's middle is not the one under the viewport's middle when the canvas starts
// at the viewport's left edge. The camera (`gl_host.ts`) and the DOM zoom share it.

const VIEWPORT = { width: 1200, height: 800 };
/** The canvas's box at each of the sidebar's three places: open (300px), collapsed (22px), and absent (the viewport). */
const BOXES: Array<[string, { left: number; top: number; width: number; height: number }]> = [
  ["open", { left: 300, top: 0, width: VIEWPORT.width - 300, height: VIEWPORT.height }],
  ["collapsed", { left: 22, top: 0, width: VIEWPORT.width - 22, height: VIEWPORT.height }],
  ["absent", { left: 0, top: 0, width: VIEWPORT.width, height: VIEWPORT.height }],
];
const VIEWS = [
  { z: 1, ox: 0, oy: 0 },
  { z: 1.5, ox: -340, oy: 120 },
  { z: 0.4, ox: 80, oy: -90 },
];

test("the world point under the viewport's middle is the one the DOM world shows there, at each sidebar place", () => {
  for (const [name, box] of BOXES) {
    for (const view of VIEWS) {
      const mid = viewportMiddleInWorld(VIEWPORT, box, view);
      // the DOM world's transform puts that point back under the viewport's middle
      assert.ok(Math.abs(mid.x * view.z + box.left + view.ox - VIEWPORT.width / 2) < 1e-9, `${name}: x lands on the viewport's middle`);
      assert.ok(Math.abs(mid.y * view.z + box.top + view.oy - VIEWPORT.height / 2) < 1e-9, `${name}: y lands on the viewport's middle`);
    }
  }
});

test("beside the sidebar the camera's world point moves by the sidebar's width, not the viewport's middle", () => {
  const view = { z: 1, ox: 0, oy: 0 };
  const absent = viewportMiddleInWorld(VIEWPORT, BOXES[2]![1], view);
  const open = viewportMiddleInWorld(VIEWPORT, BOXES[0]![1], view);
  const collapsed = viewportMiddleInWorld(VIEWPORT, BOXES[1]![1], view);
  assert.equal(absent.x, VIEWPORT.width / 2, "absent: the viewport's middle is the world's");
  assert.equal(open.x, VIEWPORT.width / 2 - 300, "open: the canvas starts 300px in, so the world's middle is 300px left of it");
  assert.equal(collapsed.x, VIEWPORT.width / 2 - 22, "collapsed: the canvas starts 22px in");
  assert.equal(open.y, absent.y, "the sidebar shifts the x alone");
});

test("the viewport's middle as the world's middle would drift the GL world beside the sidebar: the box is the start", () => {
  const view = { z: 2, ox: 0, oy: 0 };
  const box = BOXES[0]![1];
  const wrong = worldAt(VIEWPORT.width / 2, VIEWPORT.height / 2, { left: 0, top: 0 }, view);
  const right = viewportMiddleInWorld(VIEWPORT, box, view);
  assert.notEqual(wrong.x, right.x, "the two differ beside the sidebar");
  assert.equal(right.x, worldAt(VIEWPORT.width / 2, VIEWPORT.height / 2, box, view).x);
});

test("the GL host reads the canvas's box at each camera change, and takes no viewport middle as the world's", () => {
  const src = readFileSync(new URL("../src/bundles/canvas/gl_host.ts", import.meta.url), "utf8");
  assert.doesNotMatch(src, /\(W \/ 2 - view\.ox\)|\(H \/ 2 - view\.oy\)/, "no viewport middle as the world's middle");
  assert.match(src, /const mid = viewportMiddleInWorld\(\{ width: W, height: H \}, box\(\), view\);/, "the camera maps the viewport's middle through the box");
  const canvas = readFileSync(new URL("../src/bundles/canvas/canvas.ts", import.meta.url), "utf8");
  assert.match(canvas, /box: \(\) => canvas\.getBoundingClientRect\(\),/, "the canvas hands the GL host its own box");
  assert.match(canvas, /onPlace: \(\) => glHost\?\.applyCamera\(\)/, "a change of the sidebar's width re-applies the camera");
});
