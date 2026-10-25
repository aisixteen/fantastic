import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { grantViewLists, registerCanvas } from "../src/bundles/canvas/canvas_agent.ts";
import { registerLayout } from "../src/bundles/canvas/layout.ts";
import { setCanvasNode } from "../src/bundles/canvas/host_views.ts";
import { awaitGrant, bindCellToView, grantOwed, changeOf, isRefusal, onStateFrame, openViewAgent, release, routeOf, routedBridge, servedOf } from "../src/bundles/files/file_bridge.ts";
import type { Refusal, Served } from "../src/bundles/files/file_bridge.ts";
import { filesView } from "../src/bundles/files/files_view.ts";
import { setFilesDeps } from "../src/bundles/files/files_deps.ts";
import { previewView } from "../src/bundles/preview/preview_view.ts";
import type { ViewContext, ViewHandle } from "../src/bundles/view.ts";
import { installDom, stub } from "./dom_stub.ts";
import { FakeBridge, settle } from "./fake_bridge.ts";
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
    subscribeState: b.subscribeState,
    // a view's `changed` events: the fake relays none, so there is nothing to subscribe
    watchRemote: () => {},
    unwatchRemote: () => {},
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

// ─── THE CELL'S BINDING, at mount, and the page's wait for its grant (#150 review) ──

/** Every call the page makes to a view, by the view's id, in order. */
function spyViews(b: FakeBridge): string[] {
  const seen: string[] = [];
  const real = b.viewCall.bind(b);
  b.viewCall = (view: string, p: Dict) => {
    seen.push(view);
    return real(view, p);
  };
  return seen;
}

test("a reloaded cell that carries its view_agent is served by that agent again", async () => {
  const b = new FakeBridge();
  project(b);
  const first = await openViewAgent(bridgeKernel(b), "file", "docs/a.md");
  assert.ok(!isRefusal(first));
  // a fresh page: nothing is served until the cell mounts and binds to its saved agent
  const reloaded = bridgeKernel(b);
  const seen = spyViews(b);
  const bound = await bindCellToView(reloaded, "file", "docs/a.md", first.view, () => assert.fail("a saved agent is not named again"));
  assert.deepEqual(bound, { ok: true, view: first.view });
  const read = (await routedBridge(reloaded).call({ type: "read", path: "docs/a.md", offset: 0, limit: 200 })) as { lines: Array<{ text: string }> };
  assert.equal(read.lines[0]!.text, "# Hello");
  assert.ok(seen.includes(first.view), "the reloaded cell reads through its agent");
});

test("a cell saved before views, with no view_agent, is bound to the view of its path and names it", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const named: string[] = [];
  const bound = await bindCellToView(k, "folder", "src", undefined, (view) => named.push(view));
  assert.ok(bound.ok, "it opens");
  assert.equal(named.length, 1, "the cell is told its view");
  assert.equal(bound.ok ? bound.view : "", named[0]);
  assert.equal(b.views.get(named[0]!)?.kind, "folder");
  const seen = spyViews(b);
  await routedBridge(k).call({ type: "list", path: "src" });
  assert.ok(seen.includes(named[0]!), "its listing is the view's");
});

test("a cell whose view is refused stays open unbound, and the refusal is returned for it to show", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  b.openViewRefusal = { error: "open_view: docs/a.md is not exposed", reason: "unauthorized" };
  const named: string[] = [];
  const bound = await bindCellToView(k, "file", "docs/a.md", undefined, (view) => named.push(view));
  assert.equal(bound.ok, false);
  assert.equal(bound.ok ? "" : bound.refusal.error, "open_view: docs/a.md is not exposed", "the refusal, as it is");
  assert.equal(named.length, 0, "nothing is named: there is no view to name");
});

test("a view the host has not granted the page yet is waited for, then reached", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const first = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(first));
  b.pendingGrants = 1; // the page's probe is refused; the host's grant lands after it, on the state stream
  const bound = await bindCellToView(k, "file", "docs/a.md", first.view, () => {});
  assert.deepEqual(bound, { ok: true, view: first.view });
  assert.equal(b.pendingGrants, 0, "the refusal was waited out, not skipped");
  assert.equal(b.calls.filter((c) => c["type"] === "open_view").length, 1, "the view is opened once");
});

test("a grant the host refuses is the page's refusal, and says why", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const first = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(first));
  b.pendingGrants = 1;
  b.grantRefusal = "reach: `user`.can_call: refused";
  const bound = await bindCellToView(k, "file", "docs/a.md", first.view, () => {});
  assert.equal(bound.ok, false);
  assert.equal(bound.ok ? "" : (bound.refusal as Dict)["reason"], "grant_refused");
  assert.equal(bound.ok ? "" : (bound.refusal as Dict)["error"], "reach: `user`.can_call: refused");
});

// #153: the host's word on a grant and the page's probe race; every order settles.

test("a grant the state stream reports before the probe's refused reply is heard: the cell opens", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const first = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(first));
  b.pendingGrants = 1;
  b.grantWord = "before"; // the stream's task ran first: `granted` lands while the probe is out
  const bound = await bindCellToView(k, "file", "docs/a.md", first.view, () => {});
  assert.deepEqual(bound, { ok: true, view: first.view });
});

test("a grant refusal reported before the probe's refused reply is the page's refusal", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const first = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(first));
  b.pendingGrants = 1;
  b.grantWord = "before";
  b.grantRefusal = "reach: `user`.can_call: refused";
  const bound = await bindCellToView(k, "file", "docs/a.md", first.view, () => {});
  assert.equal(bound.ok ? "" : (bound.refusal as Dict)["reason"], "grant_refused");
});

test("a grant refusal reported before the wait began is remembered, and answers it", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const first = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(first));
  b.emitState({ kind: "grant_refused", agent_id: first.view, error: "reach: refused" });
  b.pendingGrants = 1;
  b.grantWord = "never"; // the host has said its word; it will not say it again
  const bound = await bindCellToView(k, "file", "docs/a.md", first.view, () => {});
  assert.equal(bound.ok ? "" : (bound.refusal as Dict)["error"], "reach: refused");
});

test("a grant reported before the wait, yet the probe is still refused: the probe's refusal stands", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const first = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(first));
  b.emitState({ kind: "granted", agent_id: first.view });
  b.pendingGrants = 1;
  b.grantWord = "never";
  const bound = await bindCellToView(k, "file", "docs/a.md", first.view, () => {});
  assert.equal(bound.ok ? "" : (bound.refusal as Dict)["reason"], "unauthorized");
});

test("a grant word that beats the open_view reply leaves the view not owed", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  b.grantOnOpen = true;
  b.grantWord = "before";
  const r = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(r));
  assert.equal(grantOwed(k, r.view), false);
});

test("a grant_refused word that beats the reply is kept and not owed", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  b.grantOnOpen = true;
  b.grantWord = "before";
  b.grantRefusal = "reach: refused";
  const r = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(r));
  assert.equal(grantOwed(k, r.view), false);
  b.pendingGrants = 1;
  const w = await awaitGrant(k, r.view);
  assert.equal(w !== null && w.reason, "grant_refused");
});

test("a view whose word comes after the reply stays owed until it comes", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  b.grantOnOpen = true;
  b.grantWord = "after";
  const r = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(r));
  assert.equal(grantOwed(k, r.view), true);
  await settle();
  assert.equal(grantOwed(k, r.view), false);
});

test("a restored cell whose view is refused settles at once: no `added` will come for it", async () => {
  const b = new FakeBridge();
  project(b);
  const first = await openViewAgent(bridgeKernel(b), "file", "docs/a.md");
  assert.ok(!isRefusal(first));
  // a fresh page: the cell carries its saved view_agent, which the host never announces again
  const reloaded = bridgeKernel(b);
  b.pendingGrants = 1;
  b.grantWord = "never";
  const bound = await bindCellToView(reloaded, "file", "docs/a.md", first.view, () => {});
  assert.equal(bound.ok, false);
  assert.equal(bound.ok ? "" : (bound.refusal as Dict)["reason"], "unauthorized");
  assert.equal(bound.ok ? "" : (bound.refusal as Dict)["side"], "egress");
});

test("a view removed while its grant is owed settles the wait, said", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const first = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(first));
  b.pendingGrants = 1;
  b.grantWord = "never";
  let out: Refusal | null | undefined;
  void awaitGrant(k, first.view).then((r) => (out = r));
  await settle();
  assert.equal(out, undefined, "owed: still waiting");
  b.emitState({ kind: "removed", agent_id: first.view });
  await settle();
  assert.equal((out as Refusal | undefined)?.reason, "removed");
});

test("a removed view's word is forgotten: a later refusal is the probe's, not the old word", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const first = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(first));
  b.emitState({ kind: "grant_refused", agent_id: first.view, error: "reach: refused" });
  b.emitState({ kind: "removed", agent_id: first.view });
  b.pendingGrants = 1;
  b.grantWord = "never";
  const r = await awaitGrant(k, first.view);
  assert.equal(r?.reason, "unauthorized");
});

test("a refusal that is not a missing grant is returned at once, not waited for", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const bound = await bindCellToView(k, "file", "docs/a.md", "view:file:gone", () => {});
  assert.equal(bound.ok, false);
  assert.equal(bound.ok ? "" : (bound.refusal as Dict)["reason"], "unknown_agent");
});

test("awaitGrant returns null straight away when the view answers", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const first = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(first));
  const seen = spyViews(b);
  assert.equal(await awaitGrant(k, first.view), null);
  assert.deepEqual(seen, [first.view], "one probe, no wait");
});

test("a file dropped while its view is refused makes no cell, and the refusal is the answer", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  const before = k.get("view_root")!.children.size;
  b.openViewRefusal = { error: "open_view: docs/a.md is not exposed", reason: "unauthorized" };
  const r = await call(k, { type: "open", path: "docs/a.md", x: 10, y: 20 });
  assert.equal(r["error"], "open_view: docs/a.md is not exposed", JSON.stringify(r));
  assert.equal(k.get("view_root")!.children.size, before, "no cell was placed");
});

test("a served path is released when its last cell lets go, and not before", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const first = await openViewAgent(k, "file", "docs/a.md");
  assert.ok(!isRefusal(first));
  const cellA = "cell-a";
  const cellB = "cell-b";
  const bindA = await bindCellToView(k, "file", "docs/a.md", first.view, () => {}, cellA);
  const bindB = await bindCellToView(k, "file", "docs/a.md", first.view, () => {}, cellB);
  assert.ok(bindA.ok && bindB.ok);
  assert.ok(payloadOf(routeOf(servedOf(k), { type: "read", path: "docs/a.md" })) !== undefined);
  release(k, "docs/a.md", cellA);
  assert.ok(payloadOf(routeOf(servedOf(k), { type: "read", path: "docs/a.md" })) !== undefined, "b still holds it");
  release(k, "docs/a.md", "a stranger");
  assert.ok(payloadOf(routeOf(servedOf(k), { type: "read", path: "docs/a.md" })) !== undefined, "a stranger releases nothing");
  release(k, "docs/a.md", cellB);
  assert.equal(routeOf(servedOf(k), { type: "read", path: "docs/a.md" }), null, "the last holder let go");
});

test("a view the state stream reports removed releases every path it serves", async () => {
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const file = await openViewAgent(k, "file", "docs/a.md");
  const folder = await openViewAgent(k, "folder", "docs");
  assert.ok(!isRefusal(file) && !isRefusal(folder));
  assert.ok((await bindCellToView(k, "file", "docs/a.md", file.view, () => {}, "c1")).ok);
  assert.ok((await bindCellToView(k, "folder", "docs", folder.view, () => {}, "c2")).ok);
  onStateFrame(k, { kind: "removed", agent_id: file.view });
  assert.equal(routeOf(servedOf(k), { type: "read", path: "docs/a.md" }), null, "the removed view's path is released");
  assert.ok(routeOf(servedOf(k), { type: "list", path: "docs" }) !== null, "another view's path is still served");
  onStateFrame(k, { kind: "added", agent_id: folder.view });
  assert.ok(routeOf(servedOf(k), { type: "list", path: "docs" }) !== null, "an added frame releases nothing");
});

test("releasing a path nothing holds is a no-op", () => {
  const k = bridgeKernel(new FakeBridge());
  release(k, "never/served.md", "nobody");
  assert.equal(routeOf(servedOf(k), { type: "read", path: "never/served.md" }), null);
});

// ─── A CELL'S UNMOUNT lets go of the path it held (#153) ─────────────────────

test("a preview and a tree bound to their views release their served paths when they unmount", async () => {
  installDom();
  setFilesDeps({
    hostAgents: async () => [],
    hostRecords: () => [],
    pageAgents: () => [],
    seenHere: () => false,
    onHost: () => false,
    cellsFor: () => [],
    isHidden: () => false,
    moveTo: () => {},
    view: () => {},
    viewed: () => "",
    mac: false,
    open: () => {},
    fresh: () => false,
    freshIds: () => [],
    deleteAgent: () => {},
    mountAgentView: async () => ({ unmount: () => {} }),
  });
  const b = new FakeBridge();
  project(b);
  const k = bridgeKernel(b);
  const file = await openViewAgent(k, "file", "docs/a.md");
  const folder = await openViewAgent(k, "folder", "src");
  assert.ok(!isRefusal(file) && !isRefusal(folder));
  const ctx = (selfId: string, record: Dict): ViewContext =>
    ({ kernel: k, mount: stub(), selfId, backend: "files", record, close: () => {} }) as unknown as ViewContext;
  const preview = (await previewView.mount(ctx("pv_1", { pinned: "docs/a.md", follow: false, view_agent: file.view }))) as ViewHandle;
  const tree = (await filesView.mount(ctx("tree_1", { root: "src", view_agent: folder.view }))) as ViewHandle;
  await settle();
  assert.equal(servedOf(k).get("docs/a.md")?.view, file.view, "the preview is its file's view");
  assert.equal(servedOf(k).get("src")?.view, folder.view, "the tree is its folder's view");
  preview.unmount();
  assert.equal(servedOf(k).has("docs/a.md"), false, "the preview let its file go");
  assert.equal(servedOf(k).get("src")?.view, folder.view, "the tree still holds its folder");
  tree.unmount();
  assert.equal(servedOf(k).size, 0, "nothing is served once both are gone");
});
