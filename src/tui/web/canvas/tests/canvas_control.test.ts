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
  k.register(new Agent({ id: "view_root", parentId: "canvas", handlerModule: "view.ts" }));
  return k;
}
const call = async (k: Kernel, payload: Record<string, unknown>): Promise<Payload> =>
  (await k.send([{ target: "view_root", payload: payload as Payload }]))[0]!.result as Payload;

test("a view places, moves, updates, wires and removes its own cells", async () => {
  const k = page();
  const p = await call(k, { type: "place", kind: "html", id: "a", x: 10, y: 20, html: "<p>a</p>", display_name: "A" });
  assert.equal(p["placed"], "a");
  assert.equal(k.get("a")?.parentId, "view_root");
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

test("a view names its MAIN chat through update on itself: locked while main, one per view, cleared when that chat goes", async () => {
  const k = page();
  k.registerBundle("ai_view.ts", () => null);
  await call(k, { type: "place", kind: "chat", id: "c", x: 0, y: 0, backend_id: "ai_1" });
  await call(k, { type: "place", kind: "html", id: "h", x: 0, y: 0 });
  assert.equal((await call(k, { type: "update", id: "view_root", fields: { main_ai_chat: "c" } }))["updated"], "view_root");
  assert.equal(k.get("view_root")?.meta["main_ai_chat"], "c");
  assert.equal((await call(k, { type: "update", id: "view_root", fields: { main_ai_chat: "h" } }))["arg"], "main_ai_chat", "only a chat cell");
  assert.equal((await call(k, { type: "update", id: "view_root", fields: { main_ai_chat: "nope" } }))["arg"], "main_ai_chat");
  assert.equal((await call(k, { type: "update", id: "view_root", fields: { x: 1 } }))["arg"], "fields", "nothing else on the view");
  // while main it is delete-locked; another made main demotes and unlocks it
  assert.equal(k.get("c")?.meta["delete_lock"], true);
  assert.equal((await call(k, { type: "remove", id: "c" }))["reason"], "locked");
  await call(k, { type: "place", kind: "chat", id: "c2", x: 0, y: 0, backend_id: "ai_2" });
  await call(k, { type: "update", id: "view_root", fields: { main_ai_chat: "c2" } });
  assert.equal(k.get("c")?.meta["delete_lock"], false);
  assert.equal(k.get("c2")?.meta["delete_lock"], true);
  // unmade main: unlocked, the view voiceless
  assert.equal((await call(k, { type: "update", id: "view_root", fields: { main_ai_chat: null } }))["updated"], "view_root");
  assert.equal(k.get("c2")?.meta["delete_lock"], false);
  assert.equal(k.get("view_root")?.meta["main_ai_chat"], null);
  // unlocked by hand while main and removed: the view has no voice
  await call(k, { type: "update", id: "view_root", fields: { main_ai_chat: "c" } });
  k.updateMeta("c", { delete_lock: false } as unknown as Payload);
  await call(k, { type: "remove", id: "c" });
  assert.equal(k.get("view_root")?.meta["main_ai_chat"], null, "its main chat gone, the view has no voice");
});

test("place {kind: view} nests a VIEW with its own card; the nested view rewrites the card", async () => {
  const k = page();
  const p = await call(k, { type: "place", kind: "view", id: "kid", x: 10, y: 10 });
  assert.equal(p["placed"], "kid");
  assert.equal(k.get("kid")?.parentId, "view_root");
  assert.equal(k.get("kid")?.record().handler_module, "view.ts");
  assert.match(String(k.get("kid")?.meta["html"]), /a view/, "it starts with a card");
  // the nested view places its OWN cells, and rewrites its card on the parent
  const kid = async (payload: Record<string, unknown>): Promise<Payload> =>
    (await k.send([{ target: "kid", payload: payload as Payload }]))[0]!.result as Payload;
  assert.equal((await kid({ type: "place", kind: "html", id: "inner", x: 0, y: 0 }))["placed"], "inner");
  assert.equal(k.get("inner")?.parentId, "kid");
  assert.equal((await kid({ type: "set_html", bytes: "<b>sql: 3 queries</b>" }))["ok"], true);
  assert.equal((await kid({ type: "get_html" }))["html"], "<b>sql: 3 queries</b>");
  // geometry: only the view a page VIEWS is rendered
});

test("place {kind: view, template} scaffolds the nested view WITH its main chat; template is for a view only", async () => {
  const k = page();
  k.registerBundle("ai_view.ts", () => null);
  const { setScaffold } = await import("../src/bundles/canvas/canvas_agent.ts");
  // the page's spawning, stood in for: a chat cell in the nested view, made main
  setScaffold(async (view, template) => {
    k.register(new Agent({ id: `chat_${template}`, parentId: view, handlerModule: "ai_view.ts", meta: { kept: true } }));
    const [o] = await k.send([{ target: view, payload: { type: "update", id: view, fields: { main_ai_chat: `chat_${template}` } } as unknown as Payload }]);
    return o!.result as never;
  });
  const r = await call(k, { type: "place", kind: "view", id: "spec", x: 0, y: 0, template: "sql" });
  assert.equal(r["placed"], "spec");
  assert.equal((r["main_chat"] as Payload)["updated"], "spec");
  assert.equal(k.get("spec")?.meta["main_ai_chat"], "chat_sql", "the nested view's main chat is named on it");
  assert.equal(k.get("chat_sql")?.meta["delete_lock"], true, "and locked while main");
  assert.equal((await call(k, { type: "place", kind: "html", x: 0, y: 0, template: "sql" }))["arg"], "template", "a template is for kind view");
  assert.equal((await call(k, { type: "place", kind: "view", x: 0, y: 0, template: "" }))["arg"], "template");
});
