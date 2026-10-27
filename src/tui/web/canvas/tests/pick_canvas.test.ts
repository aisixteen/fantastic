import { test } from "node:test";
import assert from "node:assert/strict";
import { pickCanvas } from "../src/canvas_pick.ts";
import type { CanvasEntry } from "../src/canvas_list.ts";

const c = (id: string, main = false): CanvasEntry => ({ id, loader: `${id}_loader`, root: `.fantastic/${id}`, main, name: id });
const all = [c("canvas_root", true), c("canvas_a"), c("canvas_b")];

test("?canvas=<id> opens that canvas", () => {
  assert.equal(pickCanvas(all, "?canvas=canvas_b"), all[2]);
});

test("no parameter opens the main canvas", () => {
  assert.equal(pickCanvas(all, ""), all[0]);
});

test("no parameter and no main opens the first", () => {
  const peers = [c("canvas_a"), c("canvas_b")];
  assert.equal(pickCanvas(peers, ""), peers[0]);
});

test("an unknown id is reported missing, not silently the main canvas", () => {
  assert.deepEqual(pickCanvas(all, "?canvas=nope"), { missing: "nope" });
});

test("an empty host is reported as none", () => {
  assert.deepEqual(pickCanvas([], ""), { none: true });
  assert.deepEqual(pickCanvas([], "?canvas=x"), { missing: "x" });
});

test("other query parameters and a hash are ignored", () => {
  assert.equal(pickCanvas(all, "?x=1&canvas=canvas_a&y=2#canvas_b"), all[1]);
  assert.equal(pickCanvas(all, "?x=1"), all[0]);
});
