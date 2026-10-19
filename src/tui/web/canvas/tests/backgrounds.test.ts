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

test("the stock is the owner's set — Isoline, Glyph, Copper, Moire — and the default is Isoline · Ember", async () => {
  assert.deepEqual(STOCK.map((s) => s.name), ["Isoline", "Glyph", "Copper", "Moire"]);
  const iso = STOCK[0]!;
  assert.equal(DEFAULT_BG_SOURCE, sourceOf(iso.body, iso.sets.find(([n]) => n === "Ember")![1]));
});

test("set applies a stock background through the view's background; list says what is current", async () => {
  const { k, call } = page();
  const r = await call({ type: "set", name: "Copper", set: 1 });
  assert.equal(r["applied"], "Copper", JSON.stringify(r));
  const h = STOCK.find((s) => s.name === "Copper")!;
  assert.equal(k.get("background")?.meta["bg_source"], sourceOf(h.body, h.sets[1]![1]));
  const l = await call({ type: "list" });
  assert.deepEqual(l["current"], { name: "Copper", set: h.sets[1]![0] });
  assert.equal((l["stock"] as unknown[]).length, STOCK.length);
});

test("delete hides a stock background; restore_stock brings it back; random never picks a hidden one", async () => {
  const { call } = page();
  for (const s of STOCK.slice(1)) await call({ type: "delete", name: s.name });
  let l = await call({ type: "list" });
  assert.deepEqual((l["stock"] as Array<{ name: string }>).map((s) => s.name), ["Isoline"]);
  assert.equal((l["hidden_stock"] as string[]).length, STOCK.length - 1);
  const r = await call({ type: "random" });
  assert.equal(r["applied"], "Isoline");
  await call({ type: "restore_stock" });
  l = await call({ type: "list" });
  assert.equal((l["stock"] as unknown[]).length, STOCK.length);
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

test("the default theme `canvas` cannot be deleted; a stock `mobile` still hides", async () => {
  const { call } = page();
  const r = await call({ type: "delete_theme", name: "canvas" });
  assert.equal(r["reason"], "bad_arg");
  assert.ok((((await call({ type: "themes" }))["themes"]) as { name: string }[]).some((t) => t.name === "canvas"), "canvas still listed");
  assert.equal((await call({ type: "delete_theme", name: "mobile" }))["deleted"], "mobile");
});

test("the theme panel has no `+ new theme`: a new theme is the AI agent's to add", async () => {
  const src = await import("node:fs").then((fs) => fs.readFileSync(new URL("../src/bundles/backgrounds/backgrounds_view.ts", import.meta.url), "utf8"));
  assert.ok(!src.includes("+ new theme"));
  assert.match(src, /To make a new theme, ask your AI agent/);
  assert.match(src, /t\.name === DEFAULT_THEME && t\.from === "stock"\) r\.append\(b, ed\)/, "no × on the default theme");
});

test("the theme panel draws once per redraw: every list (backgrounds, themes, skins) read first, an overtaken redraw dropped (no two Themes)", async () => {
  const src = await import("node:fs").then((fs) => fs.readFileSync(new URL("../src/bundles/backgrounds/backgrounds_view.ts", import.meta.url), "utf8"));
  const body = src.slice(src.indexOf("const render = async"), src.indexOf("const themeForm"));
  assert.equal((body.match(/await /g) ?? []).length, 1, "one wait, before anything is drawn");
  assert.match(body, /const \[l, th, sk\] = await Promise\.all\(\[call\(\{ type: "list" \}\), call\(\{ type: "themes" \}\), skinCall\(\{ type: "list" \}\)\]\);\s*if \(mine !== drawing\) return;/);
});

test("background none costs no GPU: with no GL view the frame loop stops, and a new view wakes it", async () => {
  const src = await import("node:fs").then((fs) => fs.readFileSync(new URL("../src/bundles/canvas/gl_host.ts", import.meta.url), "utf8"));
  assert.match(src, /raf = views\.size === 0 && !dirty \? 0 : requestAnimationFrame\(frame\);/, "the loop stops when nothing is drawn");
  assert.match(src, /views\.set\(agentId, entry\);\s*dirty = true;\s*wake\(\);/, "installing a view starts it again");
});

test("a background's clock ticks at 30 Hz: frames between ticks are not drawn (a pan still draws every frame)", async () => {
  const fs = await import("node:fs");
  const host = fs.readFileSync(new URL("../src/bundles/canvas/gl_host.ts", import.meta.url), "utf8");
  assert.match(host, /cb\.hz === undefined \|\| time - cb\.last >= 1 \/ cb\.hz/, "a clock is drawn only when due; a free-running view every frame");
  assert.match(host, /function applyCamera\(\): void \{\s*dirty = true;/, "a camera move draws the next frame");
  const { PRELUDE } = await import("../src/bundles/backgrounds/stock.ts");
  assert.match(PRELUDE, /onFrame\(time => \{ uniforms\.uT\.value = time; \}, \{ hz: 30 \}\);/);
  assert.match(PRELUDE, /PAD_PX = 72,/, "the title's top-left padding, doubled");
});
