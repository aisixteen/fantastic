import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { SKIN_ID, SKIN_VERBS, ensureSkin, registerSkin } from "../src/bundles/skin/skin_agent.ts";
import { SKIN_SCHEMA, cssName } from "../src/bundles/skin/schema.ts";

// THE SKIN AGENT (phase 4, #49): the page's skin per backdrop, and the skins an
// AI writes — its state on its record.

type Dict = Record<string, unknown>;

function page(): { k: Kernel; call: (p: Dict) => Promise<Dict> } {
  const k = new Kernel();
  registerSkin(k);
  k.registerBundle("canvas.ts", () => null);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  // made as mountCanvas makes it
  ensureSkin(k);
  const call = async (p: Dict) => ((await k.send([{ target: SKIN_ID, payload: p as Payload }]))[0]!.result ?? {}) as Dict;
  return { k, call };
}

const RED = { color: { accent: { base: { $value: { colorSpace: "srgb", components: [1, 0.2, 0.2], hex: "#ff3333" } } } } };

test("ensureSkin makes one delete-locked skin agent under the page root, once", () => {
  const { k } = page();
  const a = k.get(SKIN_ID)!;
  assert.equal(a.parentId, "canvas");
  assert.equal(a.meta["delete_lock"], true);
  assert.deepEqual(a.meta["skins"], { web: "dark", native: "glass" });
  assert.deepEqual(a.meta["user_skins"], []);
  ensureSkin(k);
  assert.equal([...k.get("canvas")!.children.values()].length, 1);
});

test("list names the stock skins, the slots and the skin in use (web in node)", async () => {
  const { call } = page();
  const l = await call({ type: "list" });
  assert.deepEqual(l["stock"], [
    { name: "dark", note: "the canvas's own look: deep blue-black, a lilac accent", backdrop: "web" },
    { name: "glass", note: "translucent, over the window's native material (the app, macOS and Windows)", backdrop: "native" },
  ]);
  assert.deepEqual(l["user"], []);
  assert.deepEqual(l["skins"], { web: "dark", native: "glass" });
  assert.equal(l["backdrop"], "web");
  assert.equal(l["in_use"], "dark");
});

test("use sets this tab's slot; use {backdrop: native} sets the other; a native skin is refused for the web slot; an unknown name is refused", async () => {
  const { k, call } = page();
  await call({ type: "add", name: "red", base: "dark", tokens: RED });
  const u = await call({ type: "use", name: "red" });
  assert.deepEqual(u, { skins: { web: "red", native: "glass" }, in_use: "red" });
  const n = await call({ type: "use", name: "dark", backdrop: "native" });
  assert.deepEqual(n, { skins: { web: "red", native: "dark" }, in_use: "red" }, "an opaque web skin may sit in the native slot");
  const g = await call({ type: "use", name: "glass" });
  assert.equal(g["reason"], "bad_arg");
  assert.match(String(g["error"]), /nothing is behind a browser tab/);
  const x = await call({ type: "use", name: "nope" });
  assert.equal(x["reason"], "not_found");
  const b = await call({ type: "use", name: "dark", backdrop: "moon" });
  assert.equal(b["arg"], "backdrop");
  assert.deepEqual(k.get(SKIN_ID)!.meta["skins"], { web: "red", native: "dark" }, "refusals change nothing");
});

test("add validates and stores a partial skin over a base; the refusal names the bad token path; a stock name and a base cycle are refused; the same name replaces", async () => {
  const { k, call } = page();
  const a = await call({ type: "add", name: "red", base: "dark", tokens: RED, note: "a red accent" });
  assert.deepEqual(a, { added: "red", base: "dark", backdrop: "web" });
  const bad = await call({ type: "add", name: "bad", base: "dark", tokens: { color: { accent: { base: { $value: { colorSpace: "srgb", components: [2, 0, 0] } } } } } });
  assert.equal(bad["reason"], "bad_arg");
  assert.equal(bad["path"], "color.accent.base");
  assert.match(String(bad["error"]), /color\.accent\.base/);
  const unknown = await call({ type: "add", name: "bad", base: "dark", tokens: { color: { foo: { $value: { colorSpace: "srgb", components: [0, 0, 0] } } } } });
  assert.equal(unknown["path"], "color.foo");
  assert.match(String((await call({ type: "add", name: "dark", base: "glass", tokens: {} }))["error"]), /stock skin/);
  assert.equal((await call({ type: "add", name: "Bad Name", base: "dark", tokens: {} }))["arg"], "name");
  assert.equal((await call({ type: "add", name: "x", base: "nowhere", tokens: {} }))["reason"], "not_found");
  assert.equal((await call({ type: "add", name: "x", base: "dark" }))["arg"], "tokens");
  // a chain: blue over red over dark; then red re-based on blue is a cycle
  assert.equal((await call({ type: "add", name: "blue", base: "red", tokens: {} }))["added"], "blue");
  const cyc = await call({ type: "add", name: "red", base: "blue", tokens: RED });
  assert.equal(cyc["reason"], "bad_arg");
  assert.match(String(cyc["error"]), /cycle/);
  // the same name replaces; a user skin may set its own backdrop
  const re = await call({ type: "add", name: "red", base: "dark", tokens: { ...RED, $extensions: { fantastic: { backdrop: "native" } } } });
  assert.deepEqual(re, { added: "red", base: "dark", backdrop: "native" });
  const users = k.get(SKIN_ID)!.meta["user_skins"] as Dict[];
  assert.deepEqual(users.map((u) => u["name"]), ["blue", "red"]);
  // a native version cannot replace the skin in the web slot
  await call({ type: "add", name: "plain", base: "dark", tokens: {} });
  await call({ type: "use", name: "plain" });
  const swap = await call({ type: "add", name: "plain", base: "glass", tokens: {} });
  assert.equal(swap["reason"], "bad_arg");
});

test("get answers the authored tokens and the merged document; schema lists every SKIN_SCHEMA path with its CSS name", async () => {
  const { call } = page();
  const d = await call({ type: "get", name: "dark" });
  assert.equal(d["from"], "stock");
  assert.equal(d["backdrop"], "web");
  assert.ok(((d["merged"] as Dict)["color"] as Dict)["accent"] !== undefined, "a complete example");
  await call({ type: "add", name: "red", base: "dark", tokens: RED, note: "n" });
  const r = await call({ type: "get", name: "red" });
  assert.equal(r["from"], "user");
  assert.equal(r["base"], "dark");
  assert.deepEqual(r["tokens"], RED, "as authored");
  const merged = r["merged"] as Dict;
  assert.deepEqual((((merged["color"] as Dict)["accent"] as Dict)["base"] as Dict)["$value"], RED.color.accent.base.$value);
  assert.ok(((merged["color"] as Dict)["surface"] as Dict)["bezel"] !== undefined, "the base's tokens are in it");
  assert.equal((await call({ type: "get", name: "none" }))["reason"], "not_found");
  const s = await call({ type: "schema" });
  assert.match(String(s["format"]), /W3C Design Tokens 2025\.10/);
  assert.equal(s["extension"], "fantastic");
  const toks = s["tokens"] as Array<{ path: string; css: string; type: string; opaque: boolean }>;
  assert.deepEqual(toks.map((t) => t.path), SKIN_SCHEMA.map((t) => t.path));
  for (const t of toks) assert.equal(t.css, cssName(t.path));
  assert.ok((s["refused"] as string[]).some((x) => x.includes("$ref")));
});

test("delete removes a user skin; refused for a stock skin, a skin in use, or another skin's base", async () => {
  const { call } = page();
  await call({ type: "add", name: "red", base: "dark", tokens: RED });
  await call({ type: "add", name: "redder", base: "red", tokens: {} });
  assert.equal((await call({ type: "delete", name: "dark" }))["reason"], "bad_arg");
  assert.match(String((await call({ type: "delete", name: "red" }))["error"]), /redder's base/);
  await call({ type: "use", name: "redder", backdrop: "native" });
  assert.match(String((await call({ type: "delete", name: "redder" }))["error"]), /use another first/);
  await call({ type: "use", name: "glass", backdrop: "native" });
  assert.deepEqual(await call({ type: "delete", name: "redder" }), { deleted: "redder" });
  assert.deepEqual(await call({ type: "delete", name: "red" }), { deleted: "red" });
  assert.equal((await call({ type: "delete", name: "red" }))["reason"], "not_found");
});

test("the state lives on the record: skins and user_skins after each verb", async () => {
  const { k, call } = page();
  const changes: Dict[] = [];
  k.addStateSubscriber((e) => {
    if (e.agent_id === SKIN_ID) changes.push(e as unknown as Dict);
  });
  await call({ type: "add", name: "red", base: "dark", tokens: RED, note: "warm" });
  await call({ type: "use", name: "red" });
  const meta = k.get(SKIN_ID)!.meta;
  assert.deepEqual(meta["skins"], { web: "red", native: "glass" });
  assert.deepEqual(meta["user_skins"], [{ name: "red", base: "dark", note: "warm", tokens: RED }]);
  assert.deepEqual(changes.map((c) => c["changed"]), [["user_skins"], ["skins"]], "each verb publishes the field it changed (the loader persists it)");
  // reads change nothing
  await call({ type: "list" });
  await call({ type: "get", name: "red" });
  await call({ type: "schema" });
  assert.equal(changes.length, 2);
  // any other verb: the kernel's unhandled refusal, with the verbs
  const u = await call({ type: "paint" });
  assert.equal(u["reason"], "unhandled");
  for (const v of SKIN_VERBS) assert.ok(String(u["hint"]).includes(v));
});
