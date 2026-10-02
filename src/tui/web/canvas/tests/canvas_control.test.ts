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

test("the canvas names its MAIN chat through update on itself; it clears when that chat goes", async () => {
  const k = page();
  k.registerBundle("ai_view.ts", () => null);
  await call(k, { type: "place", kind: "chat", id: "c", x: 0, y: 0, backend_id: "ai_1" });
  await call(k, { type: "place", kind: "html", id: "h", x: 0, y: 0 });
  assert.equal((await call(k, { type: "update", id: "canvas", fields: { main_ai_chat: "c" } }))["updated"], "canvas");
  assert.equal(k.get("canvas")?.meta["main_ai_chat"], "c");
  assert.equal((await call(k, { type: "update", id: "canvas", fields: { main_ai_chat: "h" } }))["arg"], "main_ai_chat", "only a chat cell");
  assert.equal((await call(k, { type: "update", id: "canvas", fields: { main_ai_chat: "nope" } }))["arg"], "main_ai_chat");
  assert.equal((await call(k, { type: "update", id: "canvas", fields: { x: 1 } }))["arg"], "fields", "nothing else on the canvas");
  await call(k, { type: "remove", id: "c" });
  assert.equal(k.get("canvas")?.meta["main_ai_chat"], null, "its main chat gone, the canvas has no voice");
  assert.equal((await call(k, { type: "update", id: "canvas", fields: { main_ai_chat: null } }))["updated"], "canvas");
});

test("place {kind: canvas} nests a CHILD CANVAS with its own card; the child rewrites the card; around answers only for the viewed canvas", async () => {
  const k = page();
  const p = await call(k, { type: "place", kind: "canvas", id: "kid", x: 10, y: 10 });
  assert.equal(p["placed"], "kid");
  assert.equal(k.get("kid")?.parentId, "canvas");
  assert.equal(k.get("kid")?.record().handler_module, "canvas.ts");
  assert.match(String(k.get("kid")?.meta["html"]), /child canvas/, "it starts with a card");
  // the child places its OWN cells, and rewrites its card on the parent
  const kid = async (payload: Record<string, unknown>): Promise<Payload> =>
    (await k.send([{ target: "kid", payload: payload as Payload }]))[0]!.result as Payload;
  assert.equal((await kid({ type: "place", kind: "html", id: "inner", x: 0, y: 0 }))["placed"], "inner");
  assert.equal(k.get("inner")?.parentId, "kid");
  assert.equal((await kid({ type: "set_html", bytes: "<b>sql: 3 queries</b>" }))["ok"], true);
  assert.equal((await kid({ type: "get_html" }))["html"], "<b>sql: 3 queries</b>");
  // geometry: only the canvas a page VIEWS is rendered
  const { setGeometry } = await import("../src/bundles/canvas/canvas_agent.ts");
  setGeometry((c) => (c === "canvas" ? [] : null));
  assert.deepEqual((await call(k, { type: "around", x: 0, y: 0, radius: 10 }))["members"], []);
  assert.equal((await kid({ type: "around", x: 0, y: 0, radius: 10 }))["reason"], "not_found", "not viewed, not rendered");
});
