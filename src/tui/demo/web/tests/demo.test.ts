import { test } from "node:test";
import assert from "node:assert/strict";
import { exitPoint, fits, pickLayout, visibleAt } from "../src/geometry.ts";
import { fromHash, next, prev, START, toHash } from "../src/player.ts";
import { SECTIONS } from "../src/sections/index.ts";
import type { Scene } from "../src/model.ts";

const scenes = SECTIONS.flatMap((s) => s.scenes);

test("every section's data is well formed: unique ids, edges and steps name real parts", () => {
  assert.equal(new Set(SECTIONS.map((s) => s.id)).size, SECTIONS.length);
  for (const sc of scenes) {
    const ids = [...sc.nodes.map((n) => n.id), ...sc.edges.map((e) => e.id)];
    assert.equal(new Set(ids).size, ids.length, `${sc.id}: duplicate ids`);
    const nodes = new Set(sc.nodes.map((n) => n.id));
    const edges = new Set(sc.edges.map((e) => e.id));
    for (const e of sc.edges) assert.ok(nodes.has(e.from) && nodes.has(e.to), `${sc.id}/${e.id}: a missing end`);
    assert.ok(sc.steps.length > 0, `${sc.id}: no steps`);
    for (const st of sc.steps) {
      for (const id of st.show ?? []) assert.ok(nodes.has(id) || edges.has(id), `${sc.id}: shows unknown ${id}`);
      for (const id of st.focus ?? []) assert.ok(nodes.has(id), `${sc.id}: focuses unknown ${id}`);
      for (const f of st.flow ?? []) assert.ok(edges.has(f.edge), `${sc.id}: flows on unknown ${f.edge}`);
    }
  }
});

test("every node fits both stages, the wide and the tall", () => {
  assert.deepEqual(scenes.flatMap(fits), []);
});

test("a flow and a focus only touch parts visible at their step", () => {
  for (const sc of scenes) {
    sc.steps.forEach((st, i) => {
      const on = visibleAt(sc, i);
      for (const f of st.flow ?? []) assert.ok(on.has(f.edge), `${sc.id} step ${i + 1}: flow on hidden ${f.edge}`);
      for (const id of st.focus ?? []) assert.ok(on.has(id), `${sc.id} step ${i + 1}: focus on hidden ${id}`);
    });
  }
});

test("visibility: unrevealed parts show from the start, revealed ones from their step, edges with both ends", () => {
  const sc: Scene = {
    id: "s", title: "",
    nodes: [
      { id: "a", label: "", kind: "agent", wide: { x: 0, y: 0 }, tall: { x: 0, y: 0 } },
      { id: "b", label: "", kind: "agent", wide: { x: 0, y: 0 }, tall: { x: 0, y: 0 } },
    ],
    edges: [{ id: "ab", from: "a", to: "b" }],
    steps: [{ caption: "" }, { caption: "", show: ["b"] }],
  };
  assert.deepEqual([...visibleAt(sc, 0)], ["a"]);
  assert.deepEqual([...visibleAt(sc, 1)].sort(), ["a", "ab", "b"]);
});

test("pickLayout: tall on a phone or a portrait box, wide otherwise", () => {
  assert.equal(pickLayout(390, 700), "tall");
  assert.equal(pickLayout(1200, 700), "wide");
  assert.equal(pickLayout(800, 1100), "tall");
});

test("exitPoint leaves the box on its border, toward the target", () => {
  assert.deepEqual(exitPoint({ x: 0, y: 0 }, 100, 50, { x: 500, y: 0 }, 0), { x: 50, y: 0 });
  assert.deepEqual(exitPoint({ x: 0, y: 0 }, 100, 50, { x: 0, y: -500 }, 0), { x: 0, y: -25 });
});

test("the playhead walks every beat forward and back, and a hash names one beat", () => {
  const beats = scenes.reduce((n, s) => n + s.steps.length, 0);
  let h = START;
  let walked = 1;
  for (let n = next(SECTIONS, h); n; n = next(SECTIONS, h)) {
    h = n;
    walked++;
  }
  assert.equal(walked, beats);
  for (let p = prev(SECTIONS, h); p; p = prev(SECTIONS, h)) h = p;
  assert.deepEqual(h, START);
  const mid = { section: 1, scene: 0, step: 2 };
  assert.deepEqual(fromHash(SECTIONS, toHash(SECTIONS, mid)), mid);
  assert.deepEqual(fromHash(SECTIONS, "#nope/x/9"), START);
});
