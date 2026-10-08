import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel, loadBundle } from "../src/lib.ts";
import { registerHtmlAgent } from "../src/bundles/html_agent/html_agent.ts";
import type { Payload } from "../src/lib.ts";
import { registerCanvas } from "../src/bundles/canvas/canvas_agent.ts";
import { registerAgentView } from "../src/bundles/agent_view/agent_view.ts";
import { cellGrant, seenHere, setCanvasNode, setHostRecords, sideOf, viewOf } from "../src/bundles/canvas/host_views.ts";

// BACKEND-OWNED VIEWS: a host agent's record says how it is drawn; a page cell
// is layout + `backend_id` + a grant DERIVED from the record.

test("a host record gets its own view, else its kind's default, else the stub", () => {
  const html = viewOf({ id: "search", handler_module: "exec_bridge.tools", view: { kind: "html", bytes: "<p>s</p>", calls: ["search"] } });
  assert.deepEqual([html.handler, html.kind, html.bytes, html.calls], ["agent_view.ts", "html", "<p>s</p>", ["search", "watch"]], "an html view: agent_view, its body, its calls + watch");
  assert.equal(viewOf({ id: "x", view: { handler: "my_chart_view" } }).handler, "my_chart_view.ts", "any page bundle by handler");
  assert.equal(viewOf({ id: "t", handler_module: "exec_bridge.tools" }).kind, "terminal", "an exec agent defaults to its terminal");
  assert.equal(viewOf({ id: "ai", handler_module: "vllm_backend.tools", memory_store_id: "memory" }).kind, "chat", "an AI defaults to its chat");
  const stub = viewOf({ id: "files", handler_module: "file_bridge.tools" });
  assert.deepEqual([stub.kind, stub.handler], ["stub", "reflect_view.ts"], "anything else: the stub");
  assert.deepEqual(cellGrant("search", html), { search: ["search", "watch"] });
  assert.equal(sideOf({ id: "c", backend_id: "search" }), "host");
  assert.equal(sideOf({ id: "c", html: "<p/>" }), "page");
});

test("place {backend_id} draws a host agent by its record — layout + derived grant, no content", async () => {
  const k = new Kernel();
  registerCanvas(k);
  registerAgentView(k);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k.register(new Agent({ id: "view_root", parentId: "canvas", handlerModule: "view.ts" }));
  const host: Record<string, Record<string, unknown>> = {
    search: { id: "search", handler_module: "exec_bridge.tools", view: { kind: "html", bytes: "<p>s</p>", calls: ["search"] } },
    term: { id: "term", handler_module: "exec_bridge.tools" },
  };
  setHostRecords((id) => host[id]);
  const call = async (p: Record<string, unknown>): Promise<Payload> =>
    (await k.send([{ target: "view_root", payload: p as Payload }]))[0]!.result as Payload;

  const r = await call({ type: "place", backend_id: "search", id: "v1", x: 5, y: 6 });
  assert.deepEqual([r["placed"], r["shows"], r["view"]], ["v1", "search", "html"]);
  const m = k.get("v1")!;
  assert.equal(m.record().handler_module, "agent_view.ts");
  assert.equal(m.meta["backend_id"], "search");
  assert.equal(m.meta["html"], undefined, "the body stays on the host record");
  assert.deepEqual((m.meta["can_call"] as Record<string, unknown>)["search"], ["search", "watch"], "the grant is derived from view.calls");

  await call({ type: "place", backend_id: "term", id: "v2", x: 0, y: 0 });
  assert.equal(k.get("v2")!.record().handler_module, "terminal_view.ts", "no view: the kind's default renderer");

  const none = await call({ type: "place", backend_id: "ghost", x: 0, y: 0 });
  assert.equal(none["reason"], "not_found");
  // the cell holds no content of its own: only the panel relay
  const own = (await k.send([{ target: "v1", payload: { type: "get_html" } as Payload }]))[0]!.result as Payload;
  assert.equal(own["reason"], "unhandled");
});

test("a canvas sees its own subtree and the shell's own agents, never another canvas's", () => {
  const parent: Record<string, string> = {
    canvas_page: "web", canvas_root: "canvas_page", canvas_b: "canvas_page",
    ai_1: "canvas_root", deep: "ai_1", ai_2: "canvas_b", ai: "user", web: "user",
  };
  const isCanvas = (id: string): boolean => id === "canvas_root" || id === "canvas_b";
  setCanvasNode("canvas_root");
  const sees = (id: string): boolean => seenHere(id, (x) => parent[x], isCanvas);
  assert.equal(sees("ai_1"), true);
  assert.equal(sees("deep"), true);
  assert.equal(sees("canvas_root"), true);
  assert.equal(sees("ai"), true, "the root AI belongs to no canvas");
  assert.equal(sees("ai_2"), false);
  assert.equal(sees("canvas_b"), false);
  setCanvasNode("canvas_b");
  assert.equal(sees("ai_2"), true);
  assert.equal(sees("ai_1"), false);
});

test("a page bundle on the html base starts with its bundle's body; set_html makes it its own", async () => {
  const k = new Kernel();
  registerHtmlAgent(k);
  assert.equal(loadBundle(k, { name: "card", folder: "web/bundles/card", text: "base: html_agent\nsentence: A card.\nhtml: |\n  <p>hi</p>\n" }), null);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k.register(new Agent({ id: "c1", parentId: "canvas", handlerModule: "card" }));
  const get = async (): Promise<unknown> => ((await k.send([{ target: "c1", payload: { type: "get_html" } as never }]))[0]!.result as Record<string, unknown>)["html"];
  assert.equal(await get(), "<p>hi</p>\n");
  await k.send([{ target: "c1", payload: { type: "set_html", bytes: "<p>mine</p>" } as never }]);
  assert.equal(await get(), "<p>mine</p>");
  assert.match(String(loadBundle(k, { name: "bad", folder: "web/bundles/bad", text: "base: html_agent\nsentence: Bad.\nhtml: [1]\n" })), /html: the body/);
});

test("an MCP server agent is drawn by its own view: url + tools, its grant reflect/boot/watch and kernel docs", async () => {
  const v = viewOf({ id: "mcp_x", handler_module: "mcp_client.tools", url: "https://h/mcp" });
  assert.equal(v.handler, "mcp_view.ts");
  assert.equal(v.kind, "mcp");
  const { hostViewCell } = await import("../src/bundles/canvas/canvas_agent.ts");
  const cell = hostViewCell("mcp_x", { id: "mcp_x", handler_module: "mcp_client.tools", url: "https://h/mcp" });
  assert.deepEqual(cell.meta["can_call"], { mcp_x: ["reflect", "boot", "watch"], kernel: ["docs"] });
  const { toolsOf } = await import("../src/bundles/mcp_view/mcp_view.ts");
  assert.deepEqual(toolsOf({ verbs: { search: "args: q:str (req)", fetch: "args: url:str" } }), [["search", "args: q:str (req)"], ["fetch", "args: url:str"]]);
  assert.deepEqual(toolsOf({ error: "x" }), []);
});
