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

test("an MCP server agent is drawn by its own view: url + tools, its grant reflect/boot/watch, kernel docs and configure_mcp on the node", async () => {
  const v = viewOf({ id: "mcp_x", handler_module: "mcp_client.tools", url: "https://h/mcp" });
  assert.equal(v.handler, "mcp_view.ts");
  assert.equal(v.kind, "mcp");
  const { hostViewCell } = await import("../src/bundles/canvas/canvas_agent.ts");
  const cell = hostViewCell("mcp_x", { id: "mcp_x", handler_module: "mcp_client.tools", url: "https://h/mcp" });
  const { canvasNode } = await import("../src/bundles/canvas/host_views.ts");
  // ⚙ settings: `configure_mcp` on this canvas's node (owner 2026-10-11)
  assert.deepEqual(cell.meta["can_call"], { mcp_x: ["reflect", "boot", "watch"], kernel: ["docs"], [canvasNode()]: ["configure_mcp"] });
  const { toolsOf } = await import("../src/bundles/mcp_view/mcp_view.ts");
  assert.deepEqual(toolsOf({ verbs: { search: "args: q:str (req)", fetch: "args: url:str" } }), [["search", "args: q:str (req)"], ["fetch", "args: url:str"]]);
  assert.deepEqual(toolsOf({ error: "x" }), []);
});

test("a cell head names it once: the id beside the title only when they differ", async () => {
  const { headTitle } = await import("../src/bundles/canvas/host_views.ts");
  assert.deepEqual(headTitle("ai_view_e96d14e9", "ai_view_e96d14e9"), { title: "ai_view_e96d14e9", id: null });
  assert.deepEqual(headTitle("tree view", "tree_42511a66"), { title: "tree view", id: "tree_42511a66" });
  // record fields are text: markup in a display name stays a string
  assert.deepEqual(headTitle("<img src=x onerror=alert(1)>", "c1"), { title: "<img src=x onerror=alert(1)>", id: "c1" });
});

test("the user store tile places a view of the host's user_store agent", async () => {
  const { tiles } = await import("../src/bundles/canvas/action_popup.ts");
  const t = tiles("user", null).find((x) => x.label === "user store");
  assert.deepEqual(t?.pick, { kind: "user_store" });
  assert.equal(t?.section, "host");
});

// THE READ-ONLY VIEWS OF THE AGENTS THAT HAD ONLY THE STUB (#438)

const TARGETS: Array<[string, string, Record<string, unknown>?]> = [
  ["agentic_resources.tools", "resources_view.ts"],
  ["task_registry.tools", "tasks_view.ts"],
  ["ram_store.tools", "store_view.ts"],
  ["sqlite_store.tools", "store_view.ts"],
  ["scheduler.tools", "scheduler_view.ts"],
  ["cabinet.tools", "cabinets_view.ts", { cabinet_role: "manager" }],
  ["credential_store.tools", "credentials_view.ts"],
];
const WRITES = ["set", "get", "forget", "append", "add", "assign", "reassign", "status", "report", "schedule", "unschedule", "pause", "resume", "tick_now", "spawn", "set_limit", "send", "write", "delete", "close_cabinet", "conclude"];

test("each_target_agent_module_gets_its_own_view_kind: 7 modules, 6 handler files, the two stores share one", () => {
  const handlers = new Set<string>();
  for (const [module, handler, extra] of TARGETS) {
    const v = viewOf({ id: "a", handler_module: module, ...extra });
    assert.equal(v.handler, handler, module);
    handlers.add(v.handler);
  }
  assert.equal(handlers.size, 6);
  assert.equal(viewOf({ id: "a", handler_module: "ram_store.tools" }).handler, viewOf({ id: "b", handler_module: "sqlite_store.tools" }).handler);
});

test("an_explicit_record_view_still_beats_the_module_default: the cabinet cell keeps its html view, and only the manager gets the cabinets view", () => {
  const cell = { id: "cab1", handler_module: "cabinet.tools", cabinet_role: "cabinet", view: { kind: "html", bytes: "<p>status</p>", calls: ["view"] } };
  const v = viewOf(cell);
  assert.deepEqual([v.kind, v.handler, v.bytes], ["html", "agent_view.ts", "<p>status</p>"]);
  assert.equal(viewOf({ id: "cab1", handler_module: "cabinet.tools", cabinet_role: "cabinet" }).kind, "stub", "a cell without a view is not the manager");
  assert.equal(viewOf({ id: "m", handler_module: "task_registry.tools", view: { handler: "my_tasks" } }).handler, "my_tasks.ts");
});

test("their_grants_are_read_only_verbs: the cell's grant is exactly the declared reads, and each view bundle's own grant is the same", async () => {
  const bundles = {
    "resources_view.ts": (await import("../src/bundles/resources_view/resources_view.ts")).resourcesView,
    "tasks_view.ts": (await import("../src/bundles/tasks_view/tasks_view.ts")).tasksView,
    "store_view.ts": (await import("../src/bundles/store_view/store_view.ts")).storeView,
    "scheduler_view.ts": (await import("../src/bundles/scheduler_view/scheduler_view.ts")).schedulerView,
    "cabinets_view.ts": (await import("../src/bundles/cabinets_view/cabinets_view.ts")).cabinetsView,
    "credentials_view.ts": (await import("../src/bundles/credentials_view/credentials_view.ts")).credentialsView,
  } as const;
  const want: Record<string, string[]> = {
    "resources_view.ts": ["roles", "fleet", "role", "get_count", "watch"],
    "tasks_view.ts": ["list", "watch"],
    "store_view.ts": ["dump", "search", "watch"],
    "scheduler_view.ts": ["list", "history", "watch"],
    "cabinets_view.ts": ["cabinets", "watch"],
    "credentials_view.ts": ["list", "reflect", "watch"],
  };
  for (const [module, handler, extra] of TARGETS) {
    const v = viewOf({ id: "a", handler_module: module, ...extra });
    assert.deepEqual(cellGrant("a", v), { a: want[handler] }, module);
    assert.deepEqual(bundles[handler as keyof typeof bundles].grants("a"), { a: want[handler] }, `${handler} bundle grant matches RENDERER_CALLS`);
    assert.deepEqual(bundles[handler as keyof typeof bundles].handles, [handler]);
    for (const w of WRITES) assert.ok(!v.calls.includes(w), `${module} must not be granted ${w}`);
  }
});

test("credentials_grant_never_includes_get", () => {
  const v = viewOf({ id: "cred", handler_module: "credential_store.tools" });
  assert.ok(!v.calls.includes("get") && !v.calls.includes("set") && !v.calls.includes("forget"));
  assert.ok(!cellGrant("cred", v)["cred"]!.includes("get"));
});

test("an_unknown_module_still_gets_the_stub", () => {
  for (const m of ["kernel_state.tools", "scheduler", "cabinet.tools", "credential_store", ""]) {
    assert.equal(viewOf({ id: "x", handler_module: m }).kind, "stub", m);
  }
  assert.equal(viewOf({ id: "x" }).kind, "stub");
});
