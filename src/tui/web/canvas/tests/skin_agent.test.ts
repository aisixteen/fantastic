import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { SKIN_ID, SKIN_VERBS, ensureSkin, registerSkin } from "../src/bundles/skin/skin_agent.ts";
import { SKIN_SCHEMA, cssName } from "../src/bundles/skin/schema.ts";
import { bootSkin } from "../src/bundles/skin/apply.ts";
import { DEFAULT_SKINS } from "../src/bundles/skin/stock.ts";

// THE SKIN AGENT (phase 4, #49): the page's one skin, fixed by the backdrop — a
// read-only agent: `get` and `schema`.

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

test("ensureSkin makes one delete-locked skin agent under the page root, once; its record holds no skin choice", () => {
  const { k } = page();
  const a = k.get(SKIN_ID)!;
  assert.equal(a.parentId, "canvas");
  assert.equal(a.meta["delete_lock"], true);
  assert.equal(a.meta["skins"], undefined);
  assert.equal(a.meta["user_skins"], undefined);
  ensureSkin(k);
  assert.equal([...k.get("canvas")!.children.values()].length, 1);
});

test("the skin shown is the default of the backdrop (day on the web, glass in the window)", () => {
  assert.deepEqual(DEFAULT_SKINS, { web: "day", native: "glass" });
  // node has no shell flag: a plain tab, web
  assert.equal(bootSkin().name, "day");
  assert.equal(bootSkin().skin.backdrop, "web");
});

test("get answers a stock skin's tokens and backdrop; schema lists every SKIN_SCHEMA path with its CSS name", async () => {
  const { call } = page();
  const d = await call({ type: "get", name: "day" });
  assert.equal(d["backdrop"], "web");
  assert.ok(((d["tokens"] as Dict)["color"] as Dict)["accent"] !== undefined, "a complete example");
  assert.equal((await call({ type: "get", name: "glass" }))["backdrop"], "native");
  assert.equal((await call({ type: "get", name: "none" }))["reason"], "not_found");
  const s = await call({ type: "schema" });
  assert.match(String(s["format"]), /W3C Design Tokens 2025\.10/);
  assert.equal(s["extension"], "fantastic");
  const toks = s["tokens"] as Array<{ path: string; css: string; type: string; opaque: boolean }>;
  assert.deepEqual(toks.map((t) => t.path), SKIN_SCHEMA.map((t) => t.path));
  for (const t of toks) assert.equal(t.css, cssName(t.path));
  assert.ok((s["refused"] as string[]).some((x) => x.includes("$ref")));
});

test("the skin agent answers get and schema only: list, use, add and delete are unknown verbs", async () => {
  const { k, call } = page();
  assert.deepEqual([...SKIN_VERBS], ["get", "schema"]);
  for (const type of ["list", "use", "add", "delete", "paint"]) {
    const u = await call({ type, name: "dark" });
    assert.equal(u["reason"], "unhandled", type);
    for (const v of SKIN_VERBS) assert.ok(String(u["hint"]).includes(v));
    assert.ok(!/use|add|delete/.test(String(u["hint"])));
  }
  assert.equal(k.get(SKIN_ID)!.meta["skins"], undefined, "nothing was written");
});

test("a stored `skins` or `user_skins` on the record changes nothing: the skin applied stays the backdrop's default", async () => {
  const { k, call } = page();
  k.updateMeta(SKIN_ID, { skins: { web: "ocean", native: "ocean" }, user_skins: [{ name: "ocean", base: "dark", note: "", tokens: {} }] } as unknown as Payload);
  assert.equal(bootSkin().name, "day");
  assert.equal((await call({ type: "get", name: "ocean" }))["reason"], "not_found", "a stored user skin is not a skin");
});
