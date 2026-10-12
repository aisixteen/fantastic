import { test } from "node:test";
import { setCanvasNode } from "../src/bundles/canvas/host_views.ts";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { registerCanvas } from "../src/bundles/canvas/canvas_agent.ts";
import { registerHtmlAgent } from "../src/bundles/html_agent/html_agent.ts";
import { registerLayout, rectsOf } from "../src/bundles/canvas/layout.ts";

function page(): Kernel {
  setCanvasNode("canvas_root");
  const k = new Kernel();
  registerCanvas(k);
  registerHtmlAgent(k);
  registerLayout(k);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k.register(new Agent({ id: "view_root", parentId: "canvas", handlerModule: "view.ts" }));
  return k;
}
const call = async (k: Kernel, payload: Record<string, unknown>): Promise<Payload> =>
  (await k.send([{ target: "view_root", payload: payload as Payload }]))[0]!.result as Payload;

test("a view places, moves, updates, wires and removes its own cells", async () => {
  const k = page();
  const p = await call(k, { type: "place", kind: "html", id: "a", x: 10, y: 20, html: "<p>a</p>", display_name: "A", callable_by: { b: ["emit"] } });
  assert.equal(p["placed"], "a");
  assert.equal(k.get("a")?.parentId, "view_root");
  assert.equal(k.get("a")?.meta["html"], "<p>a</p>");
  assert.deepEqual((k.get("a")?.meta["callable_by"] as Record<string, unknown>)["b"], ["emit"], "place takes grants");
  await call(k, { type: "place", kind: "html", id: "b", x: 400, y: 20 });
  assert.equal((await call(k, { type: "move", id: "a", x: 99 }))["moved"], "a");
  assert.equal(k.get("a")?.meta["x"], undefined, "geometry is never on the cell");
  assert.deepEqual(rectsOf(k, "view_root", "canvas")["a"], { x: 99, y: 20, width: 480, height: 320 }, "it is in the layout of the theme in use");
  assert.equal((await call(k, { type: "update", id: "b", fields: { html: "<p>b</p>" } }))["updated"], "b");
  // a cell's grants are the view's to write — it is the cell's parent
  await call(k, { type: "update", id: "b", fields: { can_call: { dev: ["start", "stop"] } } });
  assert.deepEqual((k.get("b")?.meta["can_call"] as Record<string, unknown>)["dev"], ["start", "stop"]);
  await call(k, { type: "wire", sender: "a", receiver: "b", verbs: ["emit"] });
  assert.deepEqual((k.get("a")?.meta["can_call"] as Record<string, unknown>)["b"], ["emit"]);
  assert.deepEqual((k.get("b")?.meta["callable_by"] as Record<string, unknown>)["a"], ["emit"]);
  // a receiver that is no cell of the view (a host agent): the sender's side only
  assert.ok((await call(k, { type: "wire", sender: "a", receiver: "dev", verbs: ["start"] }))["wired"]);
  assert.deepEqual((k.get("a")?.meta["can_call"] as Record<string, unknown>)["dev"], ["start"]);
  assert.equal((await call(k, { type: "remove", id: "a" }))["deleted"], true);
  assert.equal(k.get("a"), undefined);
});

test("place takes the cell's handler_module as well as a kind", async () => {
  const k = page();
  assert.equal((await call(k, { type: "place", handler_module: "html_agent", id: "p", x: 0, y: 0, bytes: "<p>p</p>" }))["placed"], "p");
  assert.equal(k.get("p")?.record().handler_module, "html_agent.ts");
  assert.equal(k.get("p")?.meta["html"], "<p>p</p>");
  assert.equal((await call(k, { type: "place", handler_module: "stream_view.ts", id: "s", x: 0, y: 0 }))["placed"], "s");
  assert.equal(k.get("s")?.record().handler_module, "stream_view.ts");
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

test("place makes no nested view: one view for now (owner, 2026-10-04)", async () => {
  const k = page();
  const r = await call(k, { type: "place", kind: "view", id: "kid", x: 0, y: 0 });
  assert.equal(r["arg"], "kind");
  assert.equal(k.get("kid"), undefined);
});

test("a body that calls an agent the cell is not granted SAYS so, with the fix", async () => {
  const k = page();
  const body = `<button onclick="fantastic.send('dev',{type:'start',command:'sh'})">go</button><script>fantastic.watch('dev',e=>{})</script>`;
  const p = await call(k, { type: "place", kind: "html", id: "c", x: 0, y: 0, bytes: body });
  assert.deepEqual(p["grants_missing"], { dev: ["start", "watch"] });
  assert.match(String(p["hint"]), /update \{id: c, fields: \{can_call:/);
  // granted → no hint
  const u = await call(k, { type: "update", id: "c", fields: { can_call: { $self: "*", dev: ["start", "watch"] } } });
  assert.equal(u["grants_missing"], undefined);
});

test("the page kernel's docs answer the page agents' verbs (documentPage)", async () => {
  const { documentPage } = await import("../src/bundles/verb_docs.ts");
  const k = page();
  await call(k, { type: "place", kind: "html", id: "h", x: 0, y: 0 });
  documentPage(k);
  const d = (await k.send([{ target: "kernel", payload: { type: "docs", of: "view_root" } as Payload }]))[0]!.result as Payload;
  assert.ok(Object.keys(d["verbs"] as object).includes("wire"), JSON.stringify(d));
  const h = (await k.send([{ target: "kernel", payload: { type: "docs", of: "h", verb: "set_html" } as Payload }]))[0]!.result as Payload;
  assert.ok((h["verbs"] as Payload)["set_html"], JSON.stringify(h));
});

test("an html body must BE html: a placeholder or a truncated one is refused, saying why", async () => {
  const k = page();
  const ph = await call(k, { type: "place", kind: "html", id: "a", x: 0, y: 0, bytes: "...html..." });
  assert.equal(ph["reason"], "bad_arg"); assert.match(String(ph["error"]), /no element/);
  assert.equal(k.get("a"), undefined, "nothing is placed");
  const cut = await call(k, { type: "place", kind: "html", id: "b", x: 0, y: 0, bytes: "<div>x</div><script>let a = 1;" });
  assert.match(String(cut["error"]), /<script> is opened and never closed/);
  const tag = await call(k, { type: "place", kind: "html", id: "c", x: 0, y: 0, bytes: "<div>x</div><button cla" });
  assert.match(String(tag["error"]), /ends inside a tag/);
  assert.equal((await call(k, { type: "place", kind: "html", id: "d", x: 0, y: 0, bytes: "<p>ok</p>" }))["placed"], "d");
  const up = await call(k, { type: "update", id: "d", bytes: "TODO" });
  assert.match(String(up["error"]), /no element/);
  const sh = (await k.send([{ target: "d", payload: { type: "set_html", bytes: "" } as Payload }]))[0]!.result as Payload;
  assert.match(String(sh["error"]), /is empty/);
  assert.equal(k.get("d")?.meta["html"], "<p>ok</p>", "a refused body changes nothing");
});
