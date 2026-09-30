import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { registerCanvas } from "../src/bundles/canvas/canvas_agent.ts";
import { registerHtmlAgent } from "../src/bundles/html_agent/html_agent.ts";

function page(): Kernel {
  const k = new Kernel();
  registerCanvas(k);
  registerHtmlAgent(k);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  return k;
}
const call = async (k: Kernel, payload: Record<string, unknown>): Promise<Payload> =>
  (await k.send([{ target: "canvas", payload: payload as Payload }]))[0]!.result as Payload;

test("the canvas places, moves, updates, wires and removes its own cells", async () => {
  const k = page();
  const p = await call(k, { type: "place", kind: "html", id: "a", x: 10, y: 20, html: "<p>a</p>", display_name: "A" });
  assert.equal(p["placed"], "a");
  assert.equal(k.get("a")?.parentId, "canvas");
  assert.equal(k.get("a")?.meta["html"], "<p>a</p>");
  await call(k, { type: "place", kind: "html", id: "b", x: 400, y: 20 });
  assert.equal((await call(k, { type: "move", id: "a", x: 99 }))["moved"], "a");
  assert.equal(k.get("a")?.meta["x"], 99);
  assert.equal((await call(k, { type: "update", id: "b", fields: { html: "<p>b</p>" } }))["updated"], "b");
  const bad = await call(k, { type: "update", id: "b", fields: { can_call: {} } });
  assert.equal(bad["reason"], "bad_arg", "grants are never settable through update");
  await call(k, { type: "wire", sender: "a", receiver: "b", verbs: ["emit"] });
  assert.deepEqual((k.get("a")?.meta["can_call"] as Record<string, unknown>)["b"], ["emit"]);
  assert.deepEqual((k.get("b")?.meta["callable_by"] as Record<string, unknown>)["a"], ["emit"]);
  assert.equal((await call(k, { type: "remove", id: "a" }))["deleted"], true);
  assert.equal(k.get("a"), undefined);
});

test("place refuses what it cannot make, naming the arg", async () => {
  const k = page();
  assert.equal((await call(k, { type: "place", kind: "rocket", x: 0, y: 0 }))["arg"], "kind");
  assert.equal((await call(k, { type: "place", kind: "html" }))["arg"], "x");
  assert.equal((await call(k, { type: "place", kind: "terminal", x: 0, y: 0 }))["arg"], "backend_id");
  assert.equal((await call(k, { type: "move", id: "nope", x: 1 }))["reason"], "not_found");
});

test("a cell's body is `bytes`, verbatim — a parsed object is refused, not stored", async () => {
  const k = page();
  const ok = await call(k, { type: "place", kind: "html", id: "p", x: 0, y: 0, bytes: "<p>verbatim</p>" });
  assert.equal(ok["placed"], "p");
  assert.equal(k.get("p")?.meta["html"], "<p>verbatim</p>");
  const bad = await call(k, { type: "place", kind: "html", id: "q", x: 0, y: 0, bytes: { style: "x" } });
  assert.equal(bad["arg"], "bytes");
  assert.equal(k.get("q"), undefined, "nothing placed");
  assert.equal((await call(k, { type: "update", id: "p", bytes: "<p>new</p>" }))["updated"], "p");
  assert.equal(k.get("p")?.meta["html"], "<p>new</p>");
  assert.equal((await call(k, { type: "update", id: "p", bytes: { a: 1 } }))["arg"], "bytes");
});

test("a body that arrives as RAW BYTES (the bridge's binary frame) is decoded as UTF-8", async () => {
  const k = page();
  const bytes = new TextEncoder().encode("<p>über</p>");
  assert.equal((await call(k, { type: "place", kind: "html", id: "b", x: 0, y: 0, bytes: bytes as unknown as string }))["placed"], "b");
  assert.equal(k.get("b")?.meta["html"], "<p>über</p>");
  const r = (await k.send([{ target: "b", payload: { type: "set_html", bytes: new TextEncoder().encode("<p>v2</p>") } as unknown as Payload }]))[0]!.result as Payload;
  assert.equal(r["ok"], true);
  assert.equal(k.get("b")?.meta["html"], "<p>v2</p>");
});
