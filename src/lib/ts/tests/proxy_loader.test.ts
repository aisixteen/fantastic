import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { ProxyLoader } from "../src/bundles/loader/proxy_loader.ts";
import type { Bridge } from "../src/kernel/kernel.ts";
import type { Json, Payload } from "../src/kernel/json.ts";

// A headless fake bridge: records every forwarded call + returns a canned
// load_tree reply. The host session kernel_state is exercised separately in
// python (test_kernel_state.py::test_session_loader_serves_sub_namespace).
class FakeBridge implements Bridge {
  calls: { target: string; payload: Payload }[] = [];
  loadTreeReply: Json = { version: 1, records: [] };

  forward(target: string, payload: Payload): Promise<Json> {
    this.calls.push({ target, payload });
    if (payload["type"] === "load_tree") return Promise.resolve(this.loadTreeReply);
    return Promise.resolve({ ok: true });
  }
  forwardBinary(target: string, payload: Record<string, unknown>): Promise<Json> {
    return this.forward(target, payload as Payload);
  }
  emitRemote(): void {}
  watchRemote(): void {}
  unwatchRemote(): void {}
  onLifecycle(): () => void {
    return () => {};
  }
  subscribeState(): () => void {
    return () => {};
  }
}

function record(c: { payload: Payload }): Payload {
  return c.payload["record"] as Payload;
}

test("loadTree drops the host anchor and re-roots the JS root", async () => {
  const k = new Kernel();
  const fake = new FakeBridge();
  k.bridge = fake;
  fake.loadTreeReply = {
    version: 1,
    records: [
      { id: "sess1", handler_module: "kernel_state.tools" }, // namespace anchor
      { id: "canvas", handler_module: "canvas.ts", parent_id: "sess1" },
      { id: "term1", handler_module: "terminal_view.ts", parent_id: "canvas" },
    ],
  };
  const loader = new ProxyLoader(k, "sess1");
  const recs = await loader.loadTree();
  assert.deepEqual(recs, [
    { id: "canvas", handler_module: "canvas.ts", parent_id: null },
    { id: "term1", handler_module: "terminal_view.ts", parent_id: "canvas" },
  ]);
  // a clean re-rooted snapshot rebuilds a JS-local tree
  k.registerBundle("canvas.ts", () => null);
  k.registerBundle("terminal_view.ts", () => null);
  k.load(recs);
  assert.equal(k.rootId, "canvas");
  assert.ok(k.get("canvas")?.children.has("term1"));
});

test("start persists the existing tree; mutations persist/forget over the bridge", async () => {
  const k = new Kernel();
  k.registerBundle("canvas.ts", () => null);
  k.registerBundle("terminal_view.ts", () => null);
  const fake = new FakeBridge();
  k.bridge = fake;
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k.register(
    new Agent({ id: "term1", parentId: "canvas", handlerModule: "terminal_view.ts" }),
  );

  // large debounce → only the explicit flush() runs (deterministic).
  const loader = new ProxyLoader(k, "sess1", { debounceMs: 100000 });
  loader.start();
  await loader.flush();

  const persisted = fake.calls
    .filter((c) => c.payload["type"] === "persist_record")
    .map(record);
  const byId = new Map(persisted.map((r) => [r["id"], r]));
  assert.ok(byId.has("canvas") && byId.has("term1"));
  // canvas (local root, parent null) is re-rooted onto the host loader id;
  // term1 keeps its real parent.
  assert.equal(byId.get("canvas")?.["parent_id"], "sess1");
  assert.equal(byId.get("term1")?.["parent_id"], "canvas");
  // every persist went to the host session loader.
  for (const c of fake.calls) {
    if (c.payload["type"] === "persist_record") assert.equal(c.target, "sess1");
  }

  // remove → forget with the cached parent (so the host rmtrees the nested dir)
  k.remove("term1");
  await loader.flush();
  const forget = fake.calls.filter((c) => c.payload["type"] === "forget_record");
  assert.equal(forget.length, 1);
  assert.equal(forget[0].payload["id"], "term1");
  assert.equal(forget[0].payload["parent_id"], "canvas");

  await loader.stop();
});
