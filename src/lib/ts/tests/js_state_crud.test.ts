import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { ProxyLoader } from "../src/bundles/loader/proxy_loader.ts";
import type { Handler } from "../src/kernel/agent.ts";
import type { Bridge } from "../src/kernel/kernel.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import { str } from "../src/kernel/json.ts";

// How JS STATE regulates under agentic CRUD, end to end, with a MOCK loader.
// An app's CONTENT agents hold mutable content in their records (the canvas's
// html_agent / gl_agent do — their own tests live with them); here two
// TEST-LOCAL stubs of that shape stand in, so the library proves its half: a
// ProxyLoader watches the local state stream and persists/forgets over a fake
// bridge that stands in for the host session kernel_state. We assert the loader
// sees exactly the right, coalesced CRUD — no stale writes, create+delete nets
// to a forget.

/** A content agent holding one mutable `field` on its record: `set_<verb>`
 *  writes it (→ an `updated` event → a persist), `get_<verb>` reads it. */
function contentStub(verb: string, field: string): Handler {
  return (id, payload, kernel): Json => {
    switch (str(payload, "type")) {
      case `get_${verb}`:
        return { id, [field]: String(kernel.get(id)?.meta[field] ?? "") };
      case `set_${verb}`: {
        const v = str(payload, "value");
        kernel.updateMeta(id, { [field]: v });
        return { ok: true, bytes: v.length };
      }
      default:
        return { error: `stub: unknown verb '${str(payload, "type")}'` };
    }
  };
}

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
  k.registerBundle("note.ts", contentStub("text", "text"));
  k.registerBundle("shader.ts", contentStub("source", "source"));
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

test("content agent: agentic CRUD persists the LATEST content, coalesced", async () => {
  const bridge = new MockLoaderBridge();
  const k = jsKernel(bridge);
  const loader = await freshLoader(k, bridge);
  try {
    // CREATE a content agent (added → dirty persist)
    k.register(
      new Agent({ id: "panel", parentId: "canvas", handlerModule: "note.ts" }),
    );
    // UPDATE twice via the agent's own verb (agentic CRUD over send)
    const r1 = (await k.send([{ target: "panel", payload: { type: "set_text", value: "<h1>v1</h1>" } }]))[0]!.result;
    assert.deepEqual(r1, { ok: true, bytes: "<h1>v1</h1>".length });
    (await k.send([{ target: "panel", payload: { type: "set_text", value: "<h1>v2</h1>" } }]))[0]!.result;
    await loader.flush();

    // one coalesced persist carrying the latest content + structure
    const persists = bridge.persistsOf("panel");
    assert.equal(persists.length, 1, "coalesced to a single persist");
    assert.equal(persists[0]["text"], "<h1>v2</h1>", "latest content, not stale");
    assert.equal(persists[0]["handler_module"], "note.ts");
    assert.equal(persists[0]["parent_id"], "canvas");

    // the live record + the get verb agree with what was persisted
    assert.equal(k.get("panel")?.meta["text"], "<h1>v2</h1>");
    const got = ((await k.send([{ target: "panel", payload: { type: "get_text" } }]))[0]!.result) as Payload;
    assert.equal(got["text"], "<h1>v2</h1>");

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

test("a second content bundle: its field persists; its get verb reflects it", async () => {
  const bridge = new MockLoaderBridge();
  const k = jsKernel(bridge);
  const loader = await freshLoader(k, bridge);
  try {
    k.register(
      new Agent({ id: "vfx", parentId: "canvas", handlerModule: "shader.ts" }),
    );
    const src = "void main(){ gl_FragColor = vec4(1.0); }";
    (await k.send([{ target: "vfx", payload: { type: "set_source", value: src } }]))[0]!.result;
    await loader.flush();

    const persists = bridge.persistsOf("vfx");
    assert.equal(persists.length, 1);
    assert.equal(persists[0]["source"], src);
    assert.equal(persists[0]["handler_module"], "shader.ts");

    const view = ((await k.send([{ target: "vfx", payload: { type: "get_source" } }]))[0]!.result) as Payload;
    assert.equal(view["source"], src);
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
        handlerModule: "note.ts",
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
      new Agent({ id: "p", parentId: "canvas", handlerModule: "note.ts" }),
    );
    await loader.flush();
    bridge.calls.length = 0;
    const r = ((await k.send([{ target: "p", payload: { type: "frobnicate" } }]))[0]!.result) as Payload;
    assert.ok(typeof r["error"] === "string");
    await loader.flush();
    assert.equal(bridge.persistsOf("p").length, 0, "a no-op verb persists nothing");
  } finally {
    await loader.stop();
  }
});
