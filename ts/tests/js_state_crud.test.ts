import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { ProxyLoader } from "../src/bundles/loader/proxy_loader.ts";
import { registerHtmlAgent } from "../src/bundles/html_agent/html_agent.ts";
import { registerGlAgent } from "../src/bundles/gl_agent/gl_agent.ts";
import type { Bridge } from "../src/kernel/kernel.ts";
import type { Json, Payload } from "../src/kernel/json.ts";

// How JS STATE regulates under agentic CRUD, end to end, with a MOCK loader.
// The JS kernel runs content view-agents (html_agent / gl_agent) that hold
// mutable content in their records; a ProxyLoader watches the local state
// stream and persists/forgets over a fake bridge that stands in for the host
// session fs_loader. We assert the loader sees exactly the right, coalesced
// CRUD — no stale writes, create+delete nets to a forget.

class MockLoaderBridge implements Bridge {
  calls: { target: string; payload: Payload }[] = [];
  forward(target: string, payload: Payload): Promise<Json> {
    this.calls.push({ target, payload });
    if (payload["type"] === "load_tree") {
      return Promise.resolve({ version: 1, records: [] });
    }
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
  persistsOf(id: string): Payload[] {
    return this.calls
      .filter((c) => c.payload["type"] === "persist_record")
      .map((c) => c.payload["record"] as Payload)
      .filter((r) => r["id"] === id);
  }
  forgetsOf(id: string): Payload[] {
    return this.calls
      .filter(
        (c) => c.payload["type"] === "forget_record" && c.payload["id"] === id,
      )
      .map((c) => c.payload);
  }
}

function jsKernel(bridge: Bridge): Kernel {
  const k = new Kernel();
  k.bridge = bridge;
  registerHtmlAgent(k);
  registerGlAgent(k);
  k.setRoot(new Agent({ id: "canvas" })); // bare local root
  return k;
}

/** Boot a loader (huge debounce → only explicit flush runs), drain the initial
 *  root-persist, and clear the call log so a test sees only its own CRUD. */
async function freshLoader(
  k: Kernel,
  bridge: MockLoaderBridge,
): Promise<ProxyLoader> {
  const loader = new ProxyLoader(k, "sess1", { debounceMs: 100000 });
  loader.start();
  await loader.flush();
  bridge.calls.length = 0;
  return loader;
}

test("html_agent: agentic CRUD persists the LATEST content, coalesced", async () => {
  const bridge = new MockLoaderBridge();
  const k = jsKernel(bridge);
  const loader = await freshLoader(k, bridge);
  try {
    // CREATE a content agent (added → dirty persist)
    k.register(
      new Agent({ id: "panel", parentId: "canvas", handlerModule: "html_agent.ts" }),
    );
    // UPDATE twice via the agent's own verb (agentic CRUD over send)
    const r1 = await k.send("panel", { type: "set_html", html: "<h1>v1</h1>" });
    assert.deepEqual(r1, { ok: true, bytes: "<h1>v1</h1>".length });
    await k.send("panel", { type: "set_html", html: "<h1>v2</h1>" });
    await loader.flush();

    // one coalesced persist carrying the latest content + structure
    const persists = bridge.persistsOf("panel");
    assert.equal(persists.length, 1, "coalesced to a single persist");
    assert.equal(persists[0]["html"], "<h1>v2</h1>", "latest content, not stale");
    assert.equal(persists[0]["handler_module"], "html_agent.ts");
    assert.equal(persists[0]["parent_id"], "canvas");

    // the live record + the get verb agree with what was persisted
    assert.equal(k.get("panel")?.meta["html"], "<h1>v2</h1>");
    const got = (await k.send("panel", { type: "get_html" })) as Payload;
    assert.equal(got["html"], "<h1>v2</h1>");

    // DELETE → forget (with parent_id so the host rmtrees the nested dir)
    k.remove("panel");
    await loader.flush();
    const forgets = bridge.forgetsOf("panel");
    assert.equal(forgets.length, 1);
    assert.equal(forgets[0]["parent_id"], "canvas");
  } finally {
    await loader.stop();
  }
});

test("gl_agent: source mutation persists; get_gl_view reflects it", async () => {
  const bridge = new MockLoaderBridge();
  const k = jsKernel(bridge);
  const loader = await freshLoader(k, bridge);
  try {
    k.register(
      new Agent({ id: "vfx", parentId: "canvas", handlerModule: "gl_agent.ts" }),
    );
    const src = "void main(){ gl_FragColor = vec4(1.0); }";
    await k.send("vfx", { type: "set_gl_source", source: src });
    await loader.flush();

    const persists = bridge.persistsOf("vfx");
    assert.equal(persists.length, 1);
    assert.equal(persists[0]["gl_source"], src);
    assert.equal(persists[0]["handler_module"], "gl_agent.ts");

    const view = (await k.send("vfx", { type: "get_gl_view" })) as Payload;
    assert.equal(view["source"], src); // GlView contract shape ({source})
  } finally {
    await loader.stop();
  }
});

test("create-then-delete in one window nets to a forget, never a persist", async () => {
  const bridge = new MockLoaderBridge();
  const k = jsKernel(bridge);
  const loader = await freshLoader(k, bridge);
  try {
    k.register(
      new Agent({
        id: "ephemeral",
        parentId: "canvas",
        handlerModule: "html_agent.ts",
      }),
    );
    k.remove("ephemeral"); // gone before the debounce window closes
    await loader.flush();

    assert.equal(bridge.persistsOf("ephemeral").length, 0, "no stale persist");
    assert.equal(bridge.forgetsOf("ephemeral").length, 1, "exactly one forget");
  } finally {
    await loader.stop();
  }
});

test("unknown verb on a content agent errors without mutating state", async () => {
  const bridge = new MockLoaderBridge();
  const k = jsKernel(bridge);
  const loader = await freshLoader(k, bridge);
  try {
    k.register(
      new Agent({ id: "p", parentId: "canvas", handlerModule: "html_agent.ts" }),
    );
    await loader.flush();
    bridge.calls.length = 0;
    const r = (await k.send("p", { type: "frobnicate" })) as Payload;
    assert.ok(typeof r["error"] === "string");
    await loader.flush();
    assert.equal(bridge.persistsOf("p").length, 0, "a no-op verb persists nothing");
  } finally {
    await loader.stop();
  }
});
