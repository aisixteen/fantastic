// The TREE WALKS the kernel does: rebuilding a tree from records in any order,
// the parent chain a heritable grant is read up, the `$subtree:<id>` membership a
// callable_by entry tests, and the id guard every loaded record passes.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Payload } from "../src/kernel/json.ts";
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

test("a heritable grant is read up the WHOLE live parent chain; $subtree covers every depth", async () => {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  k.registerBundle("probe.tools", () => ({ ok: true }));
  k.register(
    new Agent({
      id: "served",
      parentId: "kernel_state",
      handlerModule: "probe.tools",
      meta: { callable_by: { "$subtree:hub": ["list"] } },
    }),
  );
  k.register(
    new Agent({
      id: "hub",
      parentId: "kernel_state",
      handlerModule: "probe.tools",
      meta: { can_call: { served: { verbs: ["list"], for: "descendants" } } },
    }),
  );
  let parent = "hub";
  for (const id of ["l1", "l2", "l3", "l4"]) {
    k.register(new Agent({ id, parentId: parent, handlerModule: "probe.tools" }));
    parent = id;
  }
  const send = async (from: string): Promise<Payload> => {
    const [out] = await k.scopedFor(from, null).send([{ target: "served", payload: { type: "list" } }]);
    return out!.status === "completed" ? { ok: true } : (out!.result as Payload);
  };
  for (const id of ["l1", "l2", "l3", "l4"]) assert.deepEqual(await send(id), { ok: true }, id);
  // the chain is LIVE: removing l2 takes l3 and l4 with it
  k.remove("l2");
  assert.equal(k.get("l3"), undefined, "the cascade took l3");
  assert.equal(k.get("l4"), undefined, "the cascade took l4");
  // an agent whose parent is not in the tree reads no ancestor at all
  k.register(new Agent({ id: "stray", parentId: "ghost", handlerModule: "probe.tools" }));
  assert.equal((await send("stray"))["side"], "egress");
  // a grant on an ancestor TWO levels up, not on the parent, still covers
  k.register(new Agent({ id: "m1", parentId: "l1", handlerModule: "probe.tools" }));
  k.register(new Agent({ id: "m2", parentId: "m1", handlerModule: "probe.tools" }));
  assert.deepEqual(await send("m2"), { ok: true });
  // the ingress half alone: $subtree:hub admits hub's descendants and hub itself, nobody else
  assert.equal(k.ingressGate("m2", "list", "served"), null);
  assert.equal(k.ingressGate("hub", "list", "served"), null);
  assert.equal(k.ingressGate("stray", "list", "served")?.["side"], "ingress");
  k.register(new Agent({ id: "outsider", parentId: "kernel_state", handlerModule: "probe.tools" }));
  assert.equal(k.ingressGate("outsider", "list", "served")?.["side"], "ingress");
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
