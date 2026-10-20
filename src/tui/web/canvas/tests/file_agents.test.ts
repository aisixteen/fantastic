import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { grantViewLists, registerCanvas } from "../src/bundles/canvas/canvas_agent.ts";
import { registerLayout } from "../src/bundles/canvas/layout.ts";
import { setCanvasNode } from "../src/bundles/canvas/host_views.ts";
import { changeOf, isRefusal, openViewAgent, routeOf, routedBridge } from "../src/bundles/files/file_bridge.ts";
import type { Served } from "../src/bundles/files/file_bridge.ts";
import { FakeBridge } from "./fake_bridge.ts";
import { project } from "./rig.ts";

// #150: the page's file and folder agents. A path a view agent serves goes to that
// agent (its text, its listing, its watch); every other path and verb goes to the
// bridge. The views' own verbs are the Python reference's (file_bridge/views.py), here
// answered by the fake bridge's `viewCall`.

type Dict = Record<string, unknown>;

const served = (...xs: Served[]): Map<string, Served> => new Map(xs.map((x) => [x.path, x] as const));

/** The route's payload, or undefined when the page answers itself or the bridge does. */
const payloadOf = (r: ReturnType<typeof routeOf>): Dict | undefined => (r !== null && "payload" in r ? r.payload : undefined);

/** A kernel whose host calls answer as the fake bridge does, a view's by its target. */
function bridgeKernel(b: FakeBridge): Kernel {
  const k = new Kernel();
  k.bridge = {
    forward: async (t: string, p: Payload) => (await (t.startsWith("view:") ? b.viewCall(t, p as Dict) : b.call(p as Dict))) as never,
  } as unknown as Kernel["bridge"];
  return k;
}

// ─── ROUTING, pure ──────────────────────────────────────────────────────────

test("a file view takes its own path's text reads and writes, and nothing else", () => {
  const s = served({ view: "view:file:docs/a.md", kind: "file", path: "docs/a.md" });
  assert.deepEqual(payloadOf(routeOf(s, { type: "read", path: "docs/a.md", offset: 0 })), { type: "read", path: "docs/a.md", offset: 0 });
  assert.deepEqual(payloadOf(routeOf(s, { type: "write", path: "docs/a.md", bytes: "x", if_match: "v" })), { type: "write", path: "docs/a.md", bytes: "x", if_match: "v" });
  assert.equal(routeOf(s, { type: "read", path: "top.txt" }), null, "another path is the bridge's");
  assert.equal(routeOf(s, { type: "read_stream", path: "docs/a.md" }), null, "a binary read is the bridge's");
  assert.equal(routeOf(s, { type: "edit", path: "docs/a.md" }), null, "an edit is the bridge's");
});

test("a folder view lists inside itself, its subfolders with it, in the view's own terms", () => {
  const s = served({ view: "view:folder:docs", kind: "folder", path: "docs" });
  assert.deepEqual(payloadOf(routeOf(s, { type: "list", path: "docs" })), { type: "list", path: "." }, "the folder itself");
  assert.deepEqual(payloadOf(routeOf(s, { type: "list", path: "docs/sub" })), { type: "list", path: "sub" });
  assert.equal(routeOf(s, { type: "list", path: "top" }), null, "a listing outside it is the bridge's");
});

test("a folder view at the root reads its operands as the root's own paths", () => {
  const s = served({ view: "view:folder:", kind: "folder", path: "" });
  assert.deepEqual(payloadOf(routeOf(s, { type: "list", path: "" })), { type: "list", path: "." });
  assert.deepEqual(payloadOf(routeOf(s, { type: "list", path: "a/b" })), { type: "list", path: "a/b" });
});

test("a watch of the view's own path is the view's watch; a subfolder's watch stays with the bridge", () => {
  const s = served({ view: "view:folder:docs", kind: "folder", path: "docs" });
  assert.deepEqual(routeOf(s, { type: "watch_path", path: "docs" }), { view: "view:folder:docs", payload: { type: "watch" } });
  assert.equal(routeOf(s, { type: "watch_path", path: "docs/sub" }), null, "the view relays its own entries only");
  assert.deepEqual(routeOf(s, { type: "unwatch_path", path: "docs" }), { local: { watching: false, paths: [] } }, "the relay lives with the view");
  assert.equal(routeOf(s, { type: "unwatch_path", path: "docs/sub" }), null);
});

test("the innermost folder view covers a path, and a file view wins over the folder it sits in", () => {
  const s = served(
    { view: "view:folder:a", kind: "folder", path: "a" },
    { view: "view:folder:a/b", kind: "folder", path: "a/b" },
    { view: "view:file:a/x.md", kind: "file", path: "a/x.md" },
  );
  assert.equal((routeOf(s, { type: "list", path: "a/b/c" }) as { view: string }).view, "view:folder:a/b");
  assert.equal((routeOf(s, { type: "list", path: "a/c" }) as { view: string }).view, "view:folder:a");
  assert.equal((routeOf(s, { type: "read", path: "a/x.md" }) as { view: string }).view, "view:file:a/x.md");
  assert.equal(routeOf(s, { type: "read", path: "a/y.md" }), null, "a file in the folder with no view is the bridge's");
});

test("a request with no path is the bridge's, except a listing, which names the root", () => {
  const s = served({ view: "view:folder:docs", kind: "folder", path: "docs" });
  assert.equal(routeOf(s, { type: "read" }), null);
  assert.equal(routeOf(s, { type: "list" }), null, "no path lists the project root, not this folder");
  const root = served({ view: "view:folder:", kind: "folder", path: "" });
  assert.equal((routeOf(root, { type: "list" }) as { view: string }).view, "view:folder:");
});

// ─── THE VIEW'S EVENTS ──────────────────────────────────────────────────────

test("a folder view's event is the bridge's changed, its kind mapped; a file's kind stands", () => {
  assert.deepEqual(changeOf({ type: "removed", path: "docs/a.md", name: "a.md" }), { type: "removed", path: "docs/a.md", name: "a.md", kind: "deleted" });
  assert.equal(changeOf({ type: "added", path: "docs/b.md" }).kind, "added");
  assert.equal(changeOf({ type: "changed", path: "docs/b.md" }).kind, "modified");
  assert.deepEqual(changeOf({ type: "changed", path: "notes.md", kind: "added" }), { type: "changed", path: "notes.md", kind: "added" });
});

// ─── THROUGH A VIEW AGENT, on the fake bridge ───────────────────────────────

test("a served file reads and writes through its view; the stream verbs stay with the bridge", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const opened = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(opened));
  const bridge = routedBridge(k);
  const read = (await bridge.call({ type: "read", path: "docs/a.md", offset: 0, limit: 200 })) as { lines: Array<{ text: string }> };
  assert.equal(read.lines[0]!.text, "# Hello");
  await bridge.call({ type: "write", path: "docs/a.md", bytes: "# Changed\n" });
  assert.equal(b.files.get("docs/a.md"), "# Changed\n", "the write landed on the disk through the view");
  const chunk = (await bridge.call({ type: "read_stream", path: "docs/a.md", offset: 0, length: 4 })) as Dict;
  assert.equal(chunk["size"], new TextEncoder().encode("# Changed\n").length, "a stream read is the bridge's, and it works");
});

test("a served file's watch is the view's: its relay is armed on the bridge for its own path", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const opened = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(opened));
  const r = (await routedBridge(k).call({ type: "watch_path", path: "docs/a.md" })) as Dict;
  assert.equal(r["relayed"], false);
  assert.ok(b.watched.has("docs/a.md"), "the view armed the disk watch for its path");
});

test("a served folder lists through its view, its subfolder included; the bridge serves the rest", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const opened = await openViewAgent(k, "folder", "src");
  assert.ok(!isRefusal(opened));
  const bridge = routedBridge(k);
  const names = (r: unknown): string[] => ((r as { files: Array<{ name: string }> }).files).map((f) => f.name);
  assert.deepEqual(names(await bridge.call({ type: "list", path: "src" })), ["deep", "one.txt"]);
  assert.deepEqual(names(await bridge.call({ type: "list", path: "src/deep" })), ["x.ts"], "a subfolder, through the folder's view");
  assert.deepEqual(names(await bridge.call({ type: "list", path: "" })).sort(), [".fantastic", "docs", "src", "top.txt"].sort(), "the project root is the bridge's");
});

test("opening the same path twice is one view agent: the second open reuses it", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const first = await openViewAgent(k, "file", "docs/a.md");
  const again = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(first) && !isRefusal(again));
  assert.equal(first.view, again.view, "one agent per path");
  assert.equal(first.created, true);
  assert.equal(again.created, false);
  assert.equal(b.views.size, 1);
});

// ─── THE DROP AND THE `open` VERB: the agent is made or reused, its view placed ──

function page(files: FakeBridge): Kernel {
  setCanvasNode("canvas_root");
  const k = bridgeKernel(files);
  registerCanvas(k);
  registerLayout(k);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k.register(new Agent({ id: "view_root", parentId: "canvas", handlerModule: "view.ts" }));
  grantViewLists(k); // what the canvas does at mount
  return k;
}
const call = async (k: Kernel, payload: Dict): Promise<Dict> => (await k.send([{ target: "view_root", payload: payload as Payload }]))[0]!.result as Dict;

test("a file dropped on the canvas makes its agent, and its preview is that agent's view", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  const r = await call(k, { type: "open", path: "docs/a.md", x: 10, y: 20 });
  assert.equal(r["kind"], "file");
  assert.equal(r["created"], true);
  const cell = k.get(String(r["cell"]));
  assert.equal(cell?.record().handler_module, "preview_view.ts");
  assert.equal(cell?.meta["view_agent"], r["agent"], "the cell names the agent it views");
  assert.equal(b.views.get(String(r["agent"]))?.path, "docs/a.md");
});

test("a second view of the same file shares its one agent: two cells, one agent, one view", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  const first = await call(k, { type: "open", path: "docs/a.md", x: 10, y: 20 });
  const second = await call(k, { type: "open", path: "docs/a.md", x: 300, y: 20 });
  assert.equal(second["agent"], first["agent"], "the same agent");
  assert.equal(second["created"], false, "reused, not made again");
  assert.notEqual(second["cell"], first["cell"], "a second view, a second cell");
  assert.equal(k.get(String(second["cell"]))?.meta["view_agent"], first["agent"]);
  assert.equal(b.views.size, 1);
});

test("a folder dropped on the canvas makes its agent, and its tree is that agent's view", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  const r = await call(k, { type: "open", path: "src", x: 0, y: 0 });
  assert.equal(r["kind"], "dir");
  const cell = k.get(String(r["cell"]));
  assert.equal(cell?.record().handler_module, "files_view.ts");
  assert.equal(cell?.meta["root"], "src");
  assert.equal(cell?.meta["view_agent"], r["agent"]);
  assert.equal(b.views.get(String(r["agent"]))?.kind, "folder");
  // the same folder again: the agent is reused, a second tree is a second view
  const again = await call(k, { type: "open", path: "src", x: 300, y: 0 });
  assert.equal(again["agent"], r["agent"]);
  assert.equal(again["created"], false);
});
