import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel, ProxyLoader } from "../src/lib.ts";
import type { Bridge, Json, Payload } from "../src/lib.ts";
import { registerCanvas } from "../src/bundles/canvas/canvas_agent.ts";
import { registerHtmlAgent } from "../src/bundles/html_agent/html_agent.ts";
import { bgCallableBy, viewToBg, registerBackground } from "../src/bundles/canvas/background_agent.ts";
import { DEFAULT_BG_SOURCE } from "../src/bundles/canvas/default_bg.ts";

// THE BACKGROUND is its own singleton agent under a view (`background` on the
// root view, delete-locked): a GL source in its record's `bg_source`, changed by
// its verbs (its view reaches it by the grants on both ends; anyone else needs
// a grant), persisted through the loader.

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
  // a RELAY: sends `inner` to `to` from inside its own handler — the call
  // carries the relay as its sender (a send outside any handler is the operator)
  k.registerBundle("relay.ts", async (_id, p, kk) => (await kk.send([{ target: String(p["to"]), payload: p["inner"] as Payload }]))[0]!.result);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  // the grants the page writes when it makes a view's background (ensureBackground)
  k.register(new Agent({ id: "view_root", parentId: "canvas", handlerModule: "relay.ts", meta: { can_call: viewToBg(undefined) } }));
  k.register(new Agent({ id: "background", parentId: "view_root", handlerModule: "background.ts", meta: { delete_lock: true, callable_by: bgCallableBy() } }));
  k.register(new Agent({ id: "panel", parentId: "view_root", handlerModule: "relay.ts" }));
  return { k, bridge };
}

const SRC = "scene.add(new THREE.Mesh(new THREE.PlaneGeometry(1, 1)));";

test("a view sets its background agent's source, and it persists with that agent", async () => {
  const { k, bridge } = page();
  const loader = new ProxyLoader(k, "web_loader", { debounceMs: 100000 });
  loader.start();
  await loader.flush();
  bridge.calls.length = 0;
  try {
    // AS THE VIEW: the background's parent reaches it by the grants on both ends
    const via = async (relay: string, inner: Record<string, unknown>): Promise<Payload> =>
      (await k.send([{ target: relay, payload: { type: "relay", to: "background", inner } as unknown as Payload }]))[0]!.result as Payload;
    assert.deepEqual(await via("view_root", { type: "set_bg", source: SRC }), { ok: true, bytes: SRC.length });
    assert.equal(k.get("background")?.meta["bg_source"], SRC);
    // a SIBLING panel holds no grant
    const refused = await via("panel", { type: "get_bg" });
    assert.notEqual(refused["source"], SRC, `refused: ${JSON.stringify(refused)}`);
    const view = (await k.send([{ target: "background", payload: { type: "get_gl_view" } }]))[0]!.result as Payload;
    assert.equal(view["source"], SRC, "the GL host mounts the background like any GL view");
    await loader.flush();
    const persisted = bridge.calls.filter((c) => c.payload["type"] === "persist_record").map((c) => c.payload["record"] as Payload);
    assert.ok(
      persisted.some((rec) => rec["id"] === "background" && rec["bg_source"] === SRC && rec["delete_lock"] === true),
      "the loader persists it with the background agent, delete-locked",
    );
    // the bundled one
    assert.deepEqual(await via("view_root", { type: "default_bg" }), { ok: true, bytes: DEFAULT_BG_SOURCE.length });
    assert.equal(k.get("background")?.meta["bg_source"], DEFAULT_BG_SOURCE);
    new Function("THREE", "scene", "t", "onFrame", "cleanup", DEFAULT_BG_SOURCE); // it compiles as the GL host runs it
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

test("the background cannot be deleted — nor the view above it — until its lock is cleared", () => {
  const { k } = page();
  const r = k.remove("background") as unknown as Payload;
  assert.equal(r["reason"], "locked");
  assert.ok(k.get("background"), "still there");
  // the canvas is the page kernel's ROOT: the kernel refuses a root first
  assert.notEqual((k.remove("canvas") as unknown as Payload)["deleted"], true, "the root stays");
  assert.ok(k.get("canvas"));
  // the cascade rule, on an ancestor that is not the root: a locked
  // descendant blocks it whole
  assert.equal((k.remove("view_root") as unknown as Payload)["reason"], "locked", "its view is held by the locked background");
  k.register(new Agent({ id: "group", parentId: "view_root", handlerModule: "html_agent.ts" }));
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
