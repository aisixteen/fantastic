import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { validateRecords, SnapshotError } from "../src/kernel/state.ts";
import type { AgentRecord } from "../src/kernel/state.ts";
import type { Payload } from "../src/kernel/json.ts";

// Mirrors python tests/test_kernel_save_load.py — the JS kernel's save/load is
// symmetric with the python host's (and rust's): a flat record list, two-pass
// rebuild, weak-load of bundles not registered in this runtime.

function loaderKernel(): Kernel {
  // The JS view bundles a frontend kernel knows how to run locally.
  const k = new Kernel();
  k.registerBundle("canvas.ts", () => null);
  k.registerBundle("terminal_view.ts", () => null);
  return k;
}

const TREE: AgentRecord[] = [
  { id: "canvas", handler_module: "canvas.ts", display_name: "Canvas" },
  {
    id: "term1",
    handler_module: "terminal_view.ts",
    parent_id: "canvas",
    x: 10,
  },
];

test("load builds the tree in memory from a flat record list", () => {
  const k = loaderKernel();
  k.load(TREE);
  assert.equal(k.rootId, "canvas");
  assert.deepEqual([...k.agents.keys()].sort(), ["canvas", "term1"]);
  const canvas = k.get("canvas");
  assert.ok(canvas?.children.has("term1"));
  assert.equal(k.get("term1")?.meta["x"], 10);
});

test("save → load is a deterministic round-trip (id-sorted)", () => {
  const k = loaderKernel();
  k.load(TREE);
  const snap = k.save();
  assert.equal(snap.version, 1);
  assert.deepEqual(
    snap.records.map((r) => r.id),
    ["canvas", "term1"],
  );
  // reload the snapshot into a fresh kernel → identical tree
  const k2 = loaderKernel();
  k2.load(snap);
  assert.deepEqual(k2.save(), snap);
});

test("weak-load skips a record whose bundle isn't registered (+ its subtree)", () => {
  const k = loaderKernel();
  const withGhost: AgentRecord[] = [
    ...TREE,
    {
      id: "ghost",
      handler_module: "ghost_bundle.ts",
      parent_id: "canvas",
    },
    { id: "ghost_child", handler_module: "canvas.ts", parent_id: "ghost" },
  ];
  const warnings: string[] = [];
  const orig = console.warn;
  console.warn = (m: string) => warnings.push(m);
  try {
    k.load(withGhost);
  } finally {
    console.warn = orig;
  }
  assert.ok(!k.agents.has("ghost"));
  assert.ok(!k.agents.has("ghost_child")); // subtree skipped too
  assert.ok(k.agents.has("canvas") && k.agents.has("term1"));
  assert.ok(
    warnings.some((w) =>
      w.includes(
        "[kernel] skipping agent ghost: bundle ghost_bundle.ts not installed in this runtime",
      ),
    ),
  );
});

test("local state stream: register/updateMeta/remove publish added/updated/removed", () => {
  const k = loaderKernel();
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  const events: { agent_id: string; kind: string }[] = [];
  const unsub = k.addStateSubscriber((e) =>
    events.push({ agent_id: e.agent_id, kind: e.kind }),
  );

  k.register(
    new Agent({ id: "v", parentId: "canvas", handlerModule: "terminal_view.ts" }),
  );
  k.updateMeta("v", { x: 5 } as Payload);
  k.remove("v");
  unsub();
  k.register(new Agent({ id: "w", parentId: "canvas" })); // after unsub → no event

  assert.deepEqual(events, [
    { agent_id: "v", kind: "added" },
    { agent_id: "v", kind: "updated" },
    { agent_id: "v", kind: "removed" },
  ]);
});

test("setRoot publishes no added event (root has no parent)", () => {
  const k = loaderKernel();
  const events: string[] = [];
  k.addStateSubscriber((e) => events.push(e.kind));
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  assert.deepEqual(events, []);
});

test("validateRecords rejects bad snapshots", () => {
  assert.throws(() => validateRecords([], 1), SnapshotError); // no root
  assert.throws(
    () =>
      validateRecords(
        [
          { id: "a" },
          { id: "b" },
        ],
        1,
      ),
    SnapshotError,
  ); // two roots
  assert.throws(
    () => validateRecords([{ id: "a" }, { id: "a", parent_id: "a" }], 1),
    SnapshotError,
  ); // duplicate id
  assert.throws(
    () => validateRecords([{ id: "a" }, { id: "b", parent_id: "missing" }], 1),
    SnapshotError,
  ); // dangling parent
  assert.throws(() => validateRecords([{ id: "a" }], 2), SnapshotError); // bad version
  assert.equal(validateRecords([{ id: "root" }], 1), "root");
});
