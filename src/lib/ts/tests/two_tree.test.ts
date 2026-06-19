import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { ProxyLoader } from "../src/bundles/loader/proxy_loader.ts";
import { registerHtmlAgent } from "../src/bundles/html_agent/html_agent.ts";
import type { Bridge } from "../src/kernel/kernel.ts";
import type { AgentRecord } from "../src/kernel/state.ts";
import type { Json, Payload } from "../src/kernel/json.ts";

// Two-tree round-trip, frontend side: the REAL ProxyLoader against an in-memory
// host that actually STORES + serves records — a faithful stand-in for the host
// `web_loader` (the python `test_two_tree.py` covers the real disk side). One
// frontend kernel persists its tree; a fresh one hydrates it; assert identity.
// This proves the persist-re-root + load-re-root round-trip is the identity.

class InMemoryHostBridge implements Bridge {
  readonly records = new Map<string, AgentRecord>(); // the host's web/ namespace
  forward(target: string, payload: Payload): Promise<Json> {
    switch (payload["type"]) {
      case "load_tree":
        return Promise.resolve({
          version: 1,
          records: [...this.records.values()] as unknown as Json,
        });
      case "persist_record": {
        const r = payload["record"] as unknown as AgentRecord;
        this.records.set(r.id, r);
        return Promise.resolve({ ok: true });
      }
      case "forget_record":
        this.records.delete(payload["id"] as string);
        return Promise.resolve({ ok: true });
      default:
        return Promise.resolve({ ok: true });
    }
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

function frontend(host: Bridge): Kernel {
  const k = new Kernel();
  k.bridge = host;
  k.registerBundle("canvas.ts", () => null);
  registerHtmlAgent(k);
  return k;
}

test("two-tree: a frontend tree persists through the host + rehydrates identically", async () => {
  const host = new InMemoryHostBridge();

  // kernel 1 — build a tree (content agent + bare view-of-backend), persist it
  const k1 = frontend(host);
  k1.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k1.register(
    new Agent({
      id: "panel",
      parentId: "canvas",
      handlerModule: "html_agent.ts",
      meta: { html: "<p>x</p>" },
    }),
  );
  k1.register(
    new Agent({ id: "term", parentId: "canvas", meta: { backend_id: "tb_1" } }),
  );
  const loader1 = new ProxyLoader(k1, "web_loader", { debounceMs: 100000 });
  loader1.start();
  await loader1.flush();

  // kernel 2 — a FRESH frontend hydrates the same host namespace
  const k2 = frontend(host);
  const loader2 = new ProxyLoader(k2, "web_loader", { debounceMs: 100000 });
  k2.load(await loader2.loadTree());

  assert.equal(k2.rootId, "canvas");
  assert.deepEqual([...k2.agents.keys()].sort(), ["canvas", "panel", "term"]);
  assert.equal(k2.get("panel")?.handlerModule, "html_agent.ts");
  assert.equal(k2.get("panel")?.meta["html"], "<p>x</p>"); // content round-tripped
  assert.equal(k2.get("term")?.meta["backend_id"], "tb_1"); // weak peer ref round-tripped
  assert.ok(k2.get("canvas")?.children.has("panel"));
  assert.ok(k2.get("canvas")?.children.has("term"));

  await loader1.stop();
});

test("two-tree: removing a member on one kernel drops it for the next reload", async () => {
  const host = new InMemoryHostBridge();
  const k1 = frontend(host);
  k1.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k1.register(
    new Agent({ id: "panel", parentId: "canvas", handlerModule: "html_agent.ts" }),
  );
  const loader1 = new ProxyLoader(k1, "web_loader", { debounceMs: 100000 });
  loader1.start();
  await loader1.flush();
  assert.ok(host.records.has("panel"));

  k1.remove("panel"); // → removed event → forget over the bridge
  await loader1.flush();
  assert.ok(!host.records.has("panel"));

  const k2 = frontend(host);
  const loader2 = new ProxyLoader(k2, "web_loader", { debounceMs: 100000 });
  k2.load(await loader2.loadTree());
  assert.ok(k2.agents.has("canvas"));
  assert.ok(!k2.agents.has("panel"));

  await loader1.stop();
});
