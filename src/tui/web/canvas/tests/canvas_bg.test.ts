import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel, ProxyLoader } from "../src/lib.ts";
import type { Bridge, Json, Payload } from "../src/lib.ts";
import { registerCanvas } from "../src/bundles/canvas/canvas_agent.ts";
import { registerHtmlAgent } from "../src/bundles/html_agent/html_agent.ts";
import { registerBackground } from "../src/bundles/canvas/background_agent.ts";

// THE BACKGROUND is its own singleton agent under the canvas (`background`,
// delete-locked): a GL source in its record's `bg_source`, changed by its verbs
// (the canvas reaches it as its parent; anyone else needs a grant), persisted
// through the loader.

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
  registerBackground(k);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k.register(new Agent({ id: "background", parentId: "canvas", handlerModule: "background.ts", meta: { delete_lock: true } }));
  k.register(new Agent({ id: "panel", parentId: "canvas", handlerModule: "html_agent.ts" }));
  return { k, bridge };
}

const SRC = "scene.add(new THREE.Mesh(new THREE.PlaneGeometry(1, 1)));";

test("the canvas sets its background agent's source, and it persists with that agent", async () => {
  const { k, bridge } = page();
  const loader = new ProxyLoader(k, "web_loader", { debounceMs: 100000 });
  loader.start();
  await loader.flush();
  bridge.calls.length = 0;
  try {
    // AS THE CANVAS: the background's parent reaches it with the born-with grant
    const as = k.scopedFor("canvas", null);
    const r = (await as.send([{ target: "background", payload: { type: "set_bg", source: SRC } }]))[0]!.result as Payload;
    assert.deepEqual(r, { ok: true, bytes: SRC.length });
    assert.equal(k.get("background")?.meta["bg_source"], SRC);
    // a SIBLING panel is not covered by the baseline
    const refused = (await k.scopedFor("panel", null).send([{ target: "background", payload: { type: "get_bg" } }]))[0]!;
    assert.notEqual(refused.status, "completed");
    const view = (await k.send([{ target: "background", payload: { type: "get_gl_view" } }]))[0]!.result as Payload;
    assert.equal(view["source"], SRC, "the GL host mounts the background like any GL view");
    await loader.flush();
    const persisted = bridge.calls.filter((c) => c.payload["type"] === "persist_record").map((c) => c.payload["record"] as Payload);
    assert.ok(
      persisted.some((rec) => rec["id"] === "background" && rec["bg_source"] === SRC && rec["delete_lock"] === true),
      "the loader persists it with the background agent, delete-locked",
    );
    // clear
    await k.send([{ target: "background", payload: { type: "clear_bg" } }]);
    const got = (await k.send([{ target: "background", payload: { type: "get_bg" } }]))[0]!.result as Payload;
    assert.equal(got["source"], "");
  } finally {
    await loader.stop();
  }
});

test("set_bg refuses an empty or missing source, naming the arg", async () => {
  const { k } = page();
  for (const payload of [{ type: "set_bg" }, { type: "set_bg", source: "  " }, { type: "set_bg", source: 3 }]) {
    const r = (await k.send([{ target: "background", payload: payload as unknown as Payload }]))[0]!.result as Payload;
    assert.equal(r["reason"], "bad_arg");
    assert.equal(r["arg"], "bytes", "the body is `bytes`, verbatim");
  }
  assert.equal(k.get("background")?.meta["bg_source"], undefined, "nothing was written");
});

test("the background cannot be deleted — nor the canvas above it — until its lock is cleared", () => {
  const { k } = page();
  const r = k.remove("background") as unknown as Payload;
  assert.equal(r["reason"], "locked");
  assert.ok(k.get("background"), "still there");
  // the canvas is the page kernel's ROOT: the kernel refuses a root first
  assert.notEqual((k.remove("canvas") as unknown as Payload)["deleted"], true, "the root stays");
  assert.ok(k.get("canvas"));
  // the cascade rule, on an ancestor that is not the root: a locked
  // descendant blocks it whole
  k.register(new Agent({ id: "group", parentId: "canvas", handlerModule: "html_agent.ts" }));
  k.register(new Agent({ id: "kept", parentId: "group", handlerModule: "html_agent.ts", meta: { delete_lock: true } as unknown as Payload }));
  const up = k.remove("group") as unknown as Payload;
  assert.equal(up["reason"], "locked", "a locked descendant blocks the whole cascade");
  assert.equal(up["blocked_by"], "kept");
  assert.ok(k.get("group") && k.get("panel"), "nothing removed — no partial cascade");
  assert.equal((k.remove("panel") as unknown as Payload)["deleted"], true, "an unlocked sibling goes");
});

test("set_bg refuses a source that does not compile — CSS is not a GL source", async () => {
  const { k } = page();
  const r = (await k.send([{ target: "background", payload: { type: "set_bg", bytes: "@keyframes x { 0% { color: red } }" } }]))[0]!.result as Payload;
  assert.equal(r["reason"], "bad_arg");
  assert.match(String(r["error"]), /does not compile/);
  assert.equal(k.get("background")?.meta["bg_source"], undefined, "nothing stored");
});
