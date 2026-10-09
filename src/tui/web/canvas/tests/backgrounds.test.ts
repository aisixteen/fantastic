import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { bgCallableBy, registerBackground } from "../src/bundles/canvas/background_agent.ts";
import { BACKGROUNDS, grantsFor, registerBackgrounds } from "../src/bundles/backgrounds/backgrounds_agent.ts";
import { STOCK, sourceOf } from "../src/bundles/backgrounds/stock.ts";
import { DEFAULT_BG_SOURCE } from "../src/bundles/canvas/default_bg.ts";

// THE BACKGROUNDS AGENT: stock + the user's own, applied through its view's
// background (granted on both ends when made), its state in its record.

function page(): { k: Kernel; call: (p: Payload) => Promise<Record<string, unknown>> } {
  const k = new Kernel();
  registerBackground(k);
  registerBackgrounds(k);
  k.registerBundle("view.ts", () => null);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "view.ts" }));
  k.register(new Agent({ id: "view_root", parentId: "canvas", handlerModule: "view.ts" }));
  k.register(new Agent({ id: "background", parentId: "view_root", handlerModule: "background.ts", meta: { callable_by: bgCallableBy() } }));
  // made as the canvas makes it (spawnBackgroundsAt)
  const g = grantsFor(k, "backgrounds_t", "view_root");
  k.updateMeta(g.bg!.id, { callable_by: g.bg!.callableBy } as unknown as Payload);
  k.register(new Agent({ id: "backgrounds_t", parentId: "view_root", handlerModule: BACKGROUNDS,
    meta: { can_call: g.canCall, callable_by: { $self: "*" }, user_bgs: [], hidden_stock: [], current: null, rotate: { on: false, every_s: 60 } } as unknown as Payload }));
  const call = async (p: Payload) => ((await k.send([{ target: "backgrounds_t", payload: p }]))[0]!.result ?? {}) as Record<string, unknown>;
  return { k, call };
}

test("the stock is the bganim panel's ten backgrounds, and the default is Isoline · Ember", async () => {
  assert.equal(STOCK.length, 10);
  assert.deepEqual(STOCK.map((s) => s.name), ["Isoline", "Bitmap", "Halftone", "Glyph", "Copper", "Signal", "Damson", "Thread", "Orbit", "Riso"]);
  const iso = STOCK[0]!;
  assert.equal(DEFAULT_BG_SOURCE, sourceOf(iso.body, iso.sets.find(([n]) => n === "Ember")![1]));
});

test("set applies a stock background through the view's background; list says what is current", async () => {
  const { k, call } = page();
  const r = await call({ type: "set", name: "Halftone", set: 1 });
  assert.equal(r["applied"], "Halftone", JSON.stringify(r));
  const h = STOCK.find((s) => s.name === "Halftone")!;
  assert.equal(k.get("background")?.meta["bg_source"], sourceOf(h.body, h.sets[1]![1]));
  const l = await call({ type: "list" });
  assert.deepEqual(l["current"], { name: "Halftone", set: h.sets[1]![0] });
  assert.equal((l["stock"] as unknown[]).length, 10);
});

test("delete hides a stock background; restore_stock brings it back; random never picks a hidden one", async () => {
  const { call } = page();
  for (const s of STOCK.slice(1)) await call({ type: "delete", name: s.name });
  let l = await call({ type: "list" });
  assert.deepEqual((l["stock"] as Array<{ name: string }>).map((s) => s.name), ["Isoline"]);
  assert.equal((l["hidden_stock"] as string[]).length, 9);
  const r = await call({ type: "random" });
  assert.equal(r["applied"], "Isoline");
  await call({ type: "restore_stock" });
  l = await call({ type: "list" });
  assert.equal((l["stock"] as unknown[]).length, 10);
});

test("add: a user background (full source, or a body on the prelude); a bad one is refused; delete removes it", async () => {
  const { k, call } = page();
  const src = "scene.add(new THREE.Mesh(new THREE.PlaneGeometry(1, 1)));";
  assert.equal((await call({ type: "add", name: "plane", note: "one quad", bytes: src }))["added"], "plane");
  assert.equal((await call({ type: "add", name: "body", body: "backdrop(`gl_FragColor = vec4(uC3, 1.0);`);" }))["added"], "body");
  const bad = await call({ type: "add", name: "broken", bytes: "this is not js (" });
  assert.equal(bad["reason"], "bad_arg");
  let l = await call({ type: "list" });
  assert.deepEqual((l["user"] as Array<{ name: string }>).map((u) => u.name), ["plane", "body"]);
  await call({ type: "set", name: "plane" });
  assert.equal(k.get("background")?.meta["bg_source"], src);
  await call({ type: "delete", name: "plane" });
  l = await call({ type: "list" });
  assert.deepEqual((l["user"] as Array<{ name: string }>).map((u) => u.name), ["body"]);
  // its state is its record (persisted by the loader)
  assert.equal((k.get("backgrounds_t")?.meta["user_bgs"] as unknown[]).length, 1);
});

test("none clears the background; default restores Isoline; rotate is kept in the record", async () => {
  const { k, call } = page();
  await call({ type: "none" });
  assert.equal(k.get("background")?.meta["bg_source"], "");
  await call({ type: "default" });
  assert.equal(k.get("background")?.meta["bg_source"], DEFAULT_BG_SOURCE);
  await call({ type: "rotate", on: true, every_s: 60 });
  assert.deepEqual(k.get("backgrounds_t")?.meta["rotate"], { on: true, every_s: 60 });
  await call({ type: "rotate", on: false });
  await call({ type: "shutdown" });
});
