// The TREE WALKS the kernel does: rebuilding a tree from records in any order,
// and the id guard every loaded record passes.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { validateRecords, SnapshotError } from "../src/kernel/state.ts";
import type { AgentRecord } from "../src/kernel/state.ts";

test("load rebuilds the same tree from records listed children-first, depth-first in record order", () => {
  const k = new Kernel();
  k.registerBundle("v.ts", () => null);
  const events: string[] = [];
  k.addStateSubscriber((e) => events.push(`${e.kind}:${e.agent_id}`));
  const records: AgentRecord[] = [
    { id: "c2", handler_module: "v.ts", parent_id: "b" },
    { id: "c1", handler_module: "v.ts", parent_id: "b" },
    { id: "d", handler_module: "v.ts", parent_id: "c1" },
    { id: "b", handler_module: "v.ts", parent_id: "a" },
    { id: "e", handler_module: "v.ts", parent_id: "a" },
    { id: "a", handler_module: "v.ts" },
  ];
  k.load(records);
  assert.equal(k.rootId, "a");
  // registration order: the root, then each child's subtree before its next sibling
  assert.deepEqual([...k.agents.keys()], ["a", "b", "c2", "c1", "d", "e"]);
  assert.deepEqual([...k.get("a")!.children.keys()], ["b", "e"]);
  assert.deepEqual([...k.get("b")!.children.keys()], ["c2", "c1"]);
  assert.deepEqual([...k.get("c1")!.children.keys()], ["d"]);
  // the root publishes nothing; every other record is `added`, in that order
  assert.deepEqual(events, ["added:b", "added:c2", "added:c1", "added:d", "added:e"]);
  assert.deepEqual(
    k.save().records.map((r) => r.id),
    ["a", "b", "c1", "c2", "d", "e"],
    "save is id-sorted whatever the load order",
  );
});

test("validateRecords refuses a traversal id and a hidden id with python's text", () => {
  const msg = (id: string): string => {
    try {
      validateRecords([{ id: "root" }, { id, parent_id: "root" }]);
    } catch (e) {
      assert.ok(e instanceof SnapshotError);
      return (e as Error).message;
    }
    return "accepted";
  };
  assert.equal(msg("a..b"), "record 'a..b': load: id must be a name that is not a path traversal; got 'a..b'");
  assert.equal(msg(".."), "record '..': load: id must be a name that is not a path traversal; got '..'");
  assert.equal(msg("."), "record '.': load: id must be a name that is not a path traversal; got '.'");
  assert.equal(msg(".h"), "record '.h': load: id must be a name that is not hidden; got '.h'");
  assert.equal(msg("a\\b"), "record 'a\\\\b': load: id must be a single path segment (no separators); got 'a\\\\b'");
  // python `str.isdigit()` over Unicode decimal digits
  assert.equal(msg("١٢"), "record '١٢': load: id must be a name, not a number (message ids are ints); got '١٢'");
});
