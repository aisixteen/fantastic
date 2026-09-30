import { test } from "node:test";
import assert from "node:assert/strict";
import { around, distance } from "../src/bundles/canvas/canvas_agent.ts";

const m = (id: string, x: number, y: number, w = 100, h = 100) => ({ id, kind: "k", title: "", rect: { x, y, w, h } });

test("distance is to the nearest edge, 0 inside", () => {
  assert.equal(distance(50, 50, m("a", 0, 0).rect), 0);
  assert.equal(distance(130, 50, m("a", 0, 0).rect), 30);
  assert.equal(distance(103, 104, m("a", 0, 0).rect), 5);
});

test("around keeps members within radius, nearest first", () => {
  const got = around([m("far", 1000, 0), m("near", 120, 0), m("in", 0, 0)], 50, 50, 200);
  assert.deepEqual(got.map((g) => [g.id, g.distance]), [["in", 0], ["near", 70]]);
});
