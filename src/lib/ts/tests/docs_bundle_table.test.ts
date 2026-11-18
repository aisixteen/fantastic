// `docs` serves a bundle's OWN verb table, declared when the
// bundle registers — rust's `Bundle::verb_docs`, python's `VERBS`. So a bundle is
// readable by its name before any agent of it exists, and an agent reads its
// bundle's table.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Payload } from "../src/kernel/json.ts";
import { RAM_STORE_VERBS, registerRamStore } from "../src/bundles/memory/ram_store.ts";

const ROOT = "kernel_state";

async function docs(k: Kernel, of: string): Promise<Payload> {
  const out = (await k.send([{ target: "kernel", payload: { type: "docs", of, tier: "detailed" } }]))[0]!;
  return out.result as Payload;
}

test("docs reads a bundle's table by name before any agent of it exists, and through an agent", async () => {
  const k = new Kernel();
  registerRamStore(k);
  k.setRoot(new Agent({ id: ROOT, meta: { guid: "g1" } }));
  const byName = await docs(k, "ram_store");
  assert.equal(byName["bundle"], "ram_store.ts");
  assert.deepEqual(Object.keys(byName["verbs"] as Payload), Object.keys(RAM_STORE_VERBS));
  k.register(new Agent({ id: "mem", parentId: ROOT, handlerModule: "ram_store.ts" }));
  const byAgent = await docs(k, "mem");
  assert.deepEqual(byAgent["verbs"], byName["verbs"]);
});

test("a bundle registered without docs answers an empty table", async () => {
  const k = new Kernel();
  k.registerBundle("bare.ts", () => null);
  k.setRoot(new Agent({ id: ROOT }));
  assert.deepEqual((await docs(k, "bare"))["verbs"], {});
});

test("docs part: schema answers the registered verb schemas, docs the text, all (the default) both (#719)", async () => {
  const k = new Kernel();
  const place = { type: "object", properties: { x: { type: "number" } }, required: ["x"] };
  k.registerBundle("view.ts", () => null, { place: "Put a cell at x." }, { schemas: { place } });
  k.setRoot(new Agent({ id: ROOT }));
  k.register(new Agent({ id: "view_root", parentId: ROOT, handlerModule: "view.ts" }));
  const ask = async (part?: string): Promise<Payload> => {
    const p: Payload = { type: "docs", of: "view_root", verb: "place" };
    if (part !== undefined) p["part"] = part;
    return (await k.send([{ target: "kernel", payload: p }]))[0]!.result as Payload;
  };
  const schema = await ask("schema");
  assert.deepEqual(schema["schemas"], { place });
  assert.equal(schema["verbs"], undefined);
  const text = await ask("docs");
  assert.deepEqual(Object.keys(text["verbs"] as Payload), ["place"]);
  assert.equal(text["schemas"], undefined);
  const all = await ask();
  assert.deepEqual(all["schemas"], { place });
  assert.deepEqual(Object.keys(all["verbs"] as Payload), ["place"]);
  assert.equal((await ask("most"))["reason"], "bad_arg");
});
