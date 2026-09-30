import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel, ProxyLoader } from "../src/lib.ts";
import type { Bridge, Json, Payload } from "../src/lib.ts";
import { registerCanvas } from "../src/bundles/canvas/canvas_agent.ts";
import { registerHtmlAgent } from "../src/bundles/html_agent/html_agent.ts";

// THE BACKGROUND is the canvas's own state: a GL source in the root record's
// `bg_source`, changed by the canvas's verbs from anywhere in the js kernel
// (a panel under it sends `set_bg`), and persisted through the loader.

class Recorder implements Bridge {
  calls: { target: string; payload: Payload }[] = [];
  forward(target: string, payload: Payload): Promise<Json> {
    this.calls.push({ target, payload });
    return Promise.resolve(payload["type"] === "load_tree" ? { records: [] } : { ok: true });
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

function page(): { k: Kernel; bridge: Recorder } {
  const bridge = new Recorder();
  const k = new Kernel();
  k.bridge = bridge;
  registerCanvas(k);
  registerHtmlAgent(k);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k.register(new Agent({ id: "panel", parentId: "canvas", handlerModule: "html_agent.ts" }));
  return { k, bridge };
}

const SRC = "scene.add(new THREE.Mesh(new THREE.PlaneGeometry(1, 1)));";

test("a panel sets the background through the canvas's verb, and it persists", async () => {
  const { k, bridge } = page();
  const loader = new ProxyLoader(k, "web_loader", { debounceMs: 100000 });
  loader.start();
  await loader.flush();
  bridge.calls.length = 0;
  try {
    // AS THE PANEL: a child of the canvas reaches it with the born-with grant
    const as = k.scopedFor("panel", null);
    const r = (await as.send([{ target: "canvas", payload: { type: "set_bg", source: SRC } }]))[0]!.result as Payload;
    assert.deepEqual(r, { ok: true, bytes: SRC.length });
    assert.equal(k.get("canvas")?.meta["bg_source"], SRC);
    const view = (await k.send([{ target: "canvas", payload: { type: "get_gl_view" } }]))[0]!.result as Payload;
    assert.equal(view["source"], SRC, "the GL host mounts the background like any GL view");
    await loader.flush();
    const persisted = bridge.calls.filter((c) => c.payload["type"] === "persist_record").map((c) => c.payload["record"] as Payload);
    assert.ok(persisted.some((rec) => rec["id"] === "canvas" && rec["bg_source"] === SRC), "the loader persists it with the canvas");
    // clear
    await k.send([{ target: "canvas", payload: { type: "clear_bg" } }]);
    const got = (await k.send([{ target: "canvas", payload: { type: "get_bg" } }]))[0]!.result as Payload;
    assert.equal(got["source"], "");
  } finally {
    await loader.stop();
  }
});

test("set_bg refuses an empty or missing source, naming the arg", async () => {
  const { k } = page();
  for (const payload of [{ type: "set_bg" }, { type: "set_bg", source: "  " }, { type: "set_bg", source: 3 }]) {
    const r = (await k.send([{ target: "canvas", payload: payload as unknown as Payload }]))[0]!.result as Payload;
    assert.equal(r["reason"], "bad_arg");
    assert.equal(r["arg"], "source");
  }
  assert.equal(k.get("canvas")?.meta["bg_source"], undefined, "nothing was written");
});
