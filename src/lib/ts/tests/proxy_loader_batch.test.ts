// proxy_loader's two loops, pinned item for item: what `loadTree` keeps of the
// host's reply, and the ONE batch `flush` sends — every dirty record's persist
// (in the order the records became dirty), then every forget.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { ProxyLoader } from "../src/bundles/loader/proxy_loader.ts";
import type { Bridge } from "../src/kernel/kernel.ts";
import type { Json, Payload } from "../src/kernel/json.ts";

function host(reply: Json): { bridge: Bridge; calls: { target: string; payload: Payload }[] } {
  const calls: { target: string; payload: Payload }[] = [];
  const bridge: Bridge = {
    forward: (target, payload) => {
      calls.push({ target, payload });
      return Promise.resolve(payload["type"] === "load_tree" ? reply : { ok: true });
    },
    forwardBinary: () => Promise.resolve(null),
    emitRemote: () => {},
    watchRemote: () => {},
    unwatchRemote: () => {},
    onLifecycle: () => () => {},
    subscribeState: () => () => {},
  };
  return { bridge, calls };
}

test("loadTree: no records is an empty tree; every record but the anchor is kept, in order, only the anchor's child re-rooted", async () => {
  {
    const k = new Kernel();
    k.bridge = host({ version: 1 }).bridge;
    assert.deepEqual(await new ProxyLoader(k, "sess1").loadTree(), []);
  }
  const k = new Kernel();
  const h = host({
    version: 1,
    records: [
      { id: "b", handler_module: "v.ts", parent_id: "canvas", x: 1 },
      { id: "sess1", handler_module: "kernel_state.tools" },
      { id: "canvas", handler_module: "canvas.ts", parent_id: "sess1", guid: "g" },
      { id: "loose", handler_module: "v.ts" },
    ],
  });
  k.bridge = h.bridge;
  assert.deepEqual(await new ProxyLoader(k, "sess1").loadTree(), [
    { id: "b", handler_module: "v.ts", parent_id: "canvas", x: 1 },
    { id: "canvas", handler_module: "canvas.ts", parent_id: null, guid: "g" },
    { id: "loose", handler_module: "v.ts" },
  ]);
  assert.deepEqual(h.calls, [{ target: "sess1", payload: { type: "load_tree" } }]);
});

test("flush: the persists in dirty order, then the forgets; a second start is a no-op; stop unsubscribes", async () => {
  const k = new Kernel();
  k.registerBundle("v.ts", () => null);
  const h = host({ version: 1, records: [] });
  k.bridge = h.bridge;
  k.setRoot(new Agent({ id: "canvas", handlerModule: "v.ts" }));
  k.register(new Agent({ id: "a", parentId: "canvas", handlerModule: "v.ts" }));
  k.register(new Agent({ id: "b", parentId: "a", handlerModule: "v.ts" }));
  const loader = new ProxyLoader(k, "sess1", { debounceMs: 100000 });
  loader.start();
  await loader.flush();
  const shape = (c: { target: string; payload: Payload }): string =>
    `${c.target}:${String(c.payload["type"])}:${String((c.payload["record"] as Payload | undefined)?.["id"] ?? c.payload["id"])}`;
  assert.deepEqual(h.calls.map(shape), ["sess1:persist_record:canvas", "sess1:persist_record:a", "sess1:persist_record:b"]);
  h.calls.length = 0;
  // a second start is a no-op: it does not enqueue the tree again
  loader.start();
  await loader.flush();
  assert.deepEqual(h.calls, []);
  k.updateMeta("a", { x: 1 });
  k.register(new Agent({ id: "c", parentId: "canvas", handlerModule: "v.ts" }));
  k.remove("b");
  k.updateMeta("a", { x: 2 });
  await loader.flush();
  assert.deepEqual(h.calls.map(shape), ["sess1:persist_record:a", "sess1:persist_record:c", "sess1:forget_record:b"]);
  assert.deepEqual(h.calls[2]!.payload, { type: "forget_record", id: "b", parent_id: "a" });
  assert.equal((h.calls[0]!.payload["record"] as Payload)["x"], 2, "the record as it is at flush time");
  // nothing dirty: nothing sent
  h.calls.length = 0;
  await loader.flush();
  assert.deepEqual(h.calls, []);
  await loader.stop();
  // stopped: no longer subscribed
  k.updateMeta("a", { x: 3 });
  await loader.flush();
  assert.deepEqual(h.calls, []);
});
