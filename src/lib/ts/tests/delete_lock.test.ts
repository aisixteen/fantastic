import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Payload } from "../src/kernel/json.ts";

// `delete_lock` on a PAGE agent (owner, 2026-09-30: the canvas background lives on
// this kernel with `delete_lock: true`). This kernel has no `delete_agent` verb —
// tree CRUD is the host root's — so `remove` is where the page deletes, and where
// the lock is enforced exactly as the host kernels' `delete_agent` enforces it.

function tree(): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "canvas" }));
  k.register(new Agent({ id: "background", parentId: "canvas", meta: { delete_lock: true } }));
  k.register(new Agent({ id: "group", parentId: "canvas" }));
  k.register(new Agent({ id: "leaf", parentId: "group", meta: { delete_lock: true } }));
  k.register(new Agent({ id: "free", parentId: "group" }));
  return k;
}

test("a locked agent is refused, naming the lock, and nothing is removed", () => {
  const k = tree();
  const r = k.remove("background");
  assert.equal(r["reason"], "locked", JSON.stringify(r));
  assert.equal(r["blocked_by"], "background");
  assert.equal(r["id"], "background");
  assert.ok(k.get("background"), "the locked agent is still there");
});

test("a locked DESCENDANT blocks the whole cascade — no partial removal", () => {
  const k = tree();
  const removed: string[] = [];
  k.addStateSubscriber((e) => {
    if (e.kind === "removed") removed.push(e.agent_id);
  });
  const r = k.remove("group");
  assert.equal(r["reason"], "locked");
  assert.equal(r["blocked_by"], "leaf");
  assert.deepEqual(removed, [], "nothing was removed, not even the unlocked sibling");
  assert.ok(k.get("free") && k.get("group") && k.get("leaf"));
});

test("clearing the lock with updateMeta lets the remove through", () => {
  const k = tree();
  k.updateMeta("background", { delete_lock: false } as Payload);
  assert.deepEqual(k.remove("background"), { deleted: true, id: "background" });
  assert.equal(k.get("background"), undefined);
});

test("a falsy lock (python truthiness) does not block", () => {
  const k = tree();
  k.register(new Agent({ id: "zero", parentId: "canvas", meta: { delete_lock: 0 } }));
  assert.deepEqual(k.remove("zero"), { deleted: true, id: "zero" });
});

test("an unknown id is the generic unknown_agent", () => {
  const k = tree();
  assert.equal(k.remove("nobody")["reason"], "unknown_agent");
});
