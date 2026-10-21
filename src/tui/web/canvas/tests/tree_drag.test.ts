import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { grantViewLists, registerCanvas, VIEW_VERBS } from "../src/bundles/canvas/canvas_agent.ts";
import { hiddenOf, prune, rectsOf, registerLayout, tabWidth, themeOf } from "../src/bundles/canvas/layout.ts";
import { setCanvasNode, setHostRecords, setHostSync } from "../src/bundles/canvas/host_views.ts";
import { DRAG_MIME, decodeDrag, encodeDrag, layoutAround, rangeOf, resolveTarget, toggled } from "../src/bundles/files/drag.ts";
import type { Look } from "../src/bundles/files/drag.ts";
import { agentIndex } from "../src/bundles/files/files_deps.ts";
import { FilesController } from "../src/bundles/files/files_controller.ts";
import { TreeModel } from "../src/bundles/files/tree_model.ts";
import { FakeBridge } from "./fake_bridge.ts";
import { project, rig } from "./rig.ts";

// #134 and #135: the tree's drag payload, the rows marked together, the spots a drop
// takes, what a target resolves to, the view's `open` verb, and a tree rooted at a
// folder. The browser side (the drop handler, the rows' events) is the e2e probe's.

// ─── THE PAYLOAD ────────────────────────────────────────────────────────────

test("the drag payload round-trips its items and is the tree's own MIME type", () => {
  assert.equal(DRAG_MIME, "application/x-fantastic-tree");
  const items = [{ kind: "agent" as const, id: "ea" }, { kind: "file" as const, path: "docs/a.md" }, { kind: "dir" as const, path: "src" }];
  assert.deepEqual(decodeDrag(encodeDrag(items)), { items });
});

test("a payload that is not a tree drag is refused, never read as part of one", () => {
  assert.match((decodeDrag("not json") as { error: string }).error, /not JSON/);
  assert.match((decodeDrag("{}") as { error: string }).error, /no items/);
  assert.match((decodeDrag(JSON.stringify({ items: "x" })) as { error: string }).error, /no items/);
  assert.match((decodeDrag(JSON.stringify({ items: [{ kind: "agent", id: "" }] })) as { error: string }).error, /not a file tree item/);
  assert.match((decodeDrag(JSON.stringify({ items: [{ kind: "file" }] })) as { error: string }).error, /not a file tree item/);
  // one bad item refuses the whole drop: no partial list
  assert.match((decodeDrag(JSON.stringify({ items: [{ kind: "dir", path: "a" }, { kind: "bogus" }] })) as { error: string }).error, /bogus/);
});

// ─── SEVERAL ROWS ───────────────────────────────────────────────────────────

test("a Shift range runs from the anchor to the row, both included, in the order shown", () => {
  const order = ["", "docs", "docs/a.md", "src", "top.txt"];
  assert.deepEqual(rangeOf(order, "docs", "src"), ["docs", "docs/a.md", "src"]);
  assert.deepEqual(rangeOf(order, "src", "docs"), ["docs", "docs/a.md", "src"], "backwards is the same range");
  assert.deepEqual(rangeOf(order, "src", "src"), ["src"]);
});

test("a range with a missing end is the row alone (never a guessed range)", () => {
  assert.deepEqual(rangeOf(["a", "b"], "gone", "b"), ["b"]);
  assert.deepEqual(rangeOf(["a", "b"], "a", "gone"), ["gone"]);
});

test("a ⌘/Ctrl-click toggles a row in or out of the marks, keeping their order", () => {
  assert.deepEqual(toggled([], "a"), ["a"]);
  assert.deepEqual(toggled(["a", "b"], "c"), ["a", "b", "c"]);
  assert.deepEqual(toggled(["a", "b", "c"], "b"), ["a", "c"]);
  assert.deepEqual(toggled(["a"], "a"), []);
});

// ─── WHERE A DROP LANDS ─────────────────────────────────────────────────────

function boxes(at: { x: number; y: number }, n: number, slot: { width: number; height: number }, gap = 24) {
  return layoutAround(at, n, slot, gap).map((p) => ({ ...p, width: slot.width, height: slot.height }));
}

const overlap = (a: { x: number; y: number; width: number; height: number }, b: typeof a): boolean =>
  a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height;

test("one dropped item is centred on the drop point", () => {
  assert.deepEqual(layoutAround({ x: 500, y: 300 }, 1, { width: 480, height: 320 }), [{ x: 260, y: 140 }]);
});

test("no spots at all for nothing dropped", () => {
  assert.deepEqual(layoutAround({ x: 0, y: 0 }, 0, { width: 10, height: 10 }), []);
});

test("several dropped items never overlap, and sit in a grid centred on the point", () => {
  for (let n = 2; n <= 9; n++) {
    const bs = boxes({ x: 400, y: 300 }, n, { width: 200, height: 120 });
    assert.equal(bs.length, n);
    for (let i = 0; i < bs.length; i++) for (let j = i + 1; j < bs.length; j++) assert.ok(!overlap(bs[i]!, bs[j]!), `n=${n}: ${i} and ${j} overlap`);
    // the grid's bounding box is centred on the point (to the pixel: rounding is the only slack)
    const minX = Math.min(...bs.map((b) => b.x));
    const maxX = Math.max(...bs.map((b) => b.x + b.width));
    const minY = Math.min(...bs.map((b) => b.y));
    const maxY = Math.max(...bs.map((b) => b.y + b.height));
    assert.ok(Math.abs((minX + maxX) / 2 - 400) <= 1, `n=${n} centred in x`);
    assert.ok(Math.abs((minY + maxY) / 2 - 300) <= 1, `n=${n} centred in y`);
  }
});

test("the layout is deterministic: the same drop gives the same spots", () => {
  assert.deepEqual(layoutAround({ x: 10, y: 20 }, 5, { width: 100, height: 50 }), layoutAround({ x: 10, y: 20 }, 5, { width: 100, height: 50 }));
});

// ─── WHAT A TARGET IS ───────────────────────────────────────────────────────

/** A look at a fake project: `files` and `dirs` are the paths on disk; `agents` the agent folders and the host's ids. */
function lookAt(files: string[], dirs: string[], agentFolders: Record<string, string>, hosts: string[]): Look {
  return {
    kindOf: async (p) => (files.includes(p) ? "file" : dirs.includes(p) ? "dir" : undefined),
    agentAt: (p) => agentFolders[p],
    hasAgent: async (id) => hosts.includes(id),
  };
}

const look = lookAt(["docs/a.md", "top.txt"], ["docs", "src"], { ".fantastic/agents/user/agents/ea": "ea", "ghost/dir": "gone" }, ["ea"]);

test("an agent id is that agent; an agent with no host record is refused, never guessed", async () => {
  assert.deepEqual(await resolveTarget({ kind: "agent", id: "ea" }, look), { kind: "agent", agent: "ea" });
  assert.deepEqual(await resolveTarget({ kind: "agent", id: "ghost" }, look), {
    error: "no host agent 'ghost'",
    reason: "not_found",
    arg: "agent",
    hint: "the agents are the host's: drag one from the tree",
  });
});

test("an agent's folder is the agent, whatever the item says it is", async () => {
  const folder = ".fantastic/agents/user/agents/ea";
  assert.deepEqual(await resolveTarget({ kind: "dir", path: folder }, look), { kind: "agent", agent: "ea" });
  assert.deepEqual(await resolveTarget({ kind: "any", path: folder }, look), { kind: "agent", agent: "ea" });
});

test("a file item on an agent's folder is a file, never the agent: a folder on disk is refused as a file", async () => {
  const folder = ".fantastic/agents/user/agents/ea";
  // the folder is on disk as a folder: a file item there is refused, and the agent is not read into it
  const asFolder = lookAt([], [folder], { [folder]: "ea" }, ["ea"]);
  const r = await resolveTarget({ kind: "file", path: folder }, asFolder);
  assert.equal("error" in r && r.reason, "bad_arg");
  assert.equal("error" in r && r.arg, "path");
  // a file on disk under that path is a file, whatever the agent folder map says
  const asFile = lookAt([folder], [], { [folder]: "ea" }, ["ea"]);
  assert.deepEqual(await resolveTarget({ kind: "file", path: folder }, asFile), { kind: "file", path: folder });
});

test("a folder that is an agent's folder but not a host agent is refused loudly", async () => {
  const r = await resolveTarget({ kind: "dir", path: "ghost/dir" }, look);
  assert.equal("error" in r && r.reason, "not_found");
});

test("a file opens as a file, a folder as a folder, and the root is a folder", async () => {
  assert.deepEqual(await resolveTarget({ kind: "file", path: "docs/a.md" }, look), { kind: "file", path: "docs/a.md" });
  assert.deepEqual(await resolveTarget({ kind: "dir", path: "docs" }, look), { kind: "dir", path: "docs" });
  assert.deepEqual(await resolveTarget({ kind: "any", path: "src" }, look), { kind: "dir", path: "src" });
  assert.deepEqual(await resolveTarget({ kind: "any", path: "" }, look), { kind: "dir", path: "" });
  assert.deepEqual(await resolveTarget({ kind: "any", path: "./docs/a.md" }, look), { kind: "file", path: "docs/a.md" }, "a path is normalised");
});

test("an unknown path is refused loudly, not opened as something", async () => {
  const r = await resolveTarget({ kind: "any", path: "nope/x.txt" }, look);
  assert.deepEqual(r, { error: "'nope/x.txt' is not in the project", reason: "not_found", arg: "path" });
});

test("a path outside the project is refused before anything is looked up", async () => {
  let asked = false;
  const probe: Look = { ...look, kindOf: async () => ((asked = true), undefined) };
  for (const p of ["/etc/passwd", "../x", "docs/../../x", "C:/x"]) {
    const r = await resolveTarget({ kind: "any", path: p }, probe);
    assert.equal("error" in r && r.reason, "outside_root", p);
  }
  assert.equal(asked, false);
});

test("the wrong kind is refused: a folder dropped as a file, a file dropped as a folder", async () => {
  const folderAsFile = await resolveTarget({ kind: "file", path: "docs" }, look);
  assert.equal("error" in folderAsFile && folderAsFile.reason, "bad_arg");
  const fileAsFolder = await resolveTarget({ kind: "dir", path: "top.txt" }, look);
  assert.equal("error" in fileAsFolder && fileAsFolder.reason, "bad_arg");
  assert.equal("error" in fileAsFolder && fileAsFolder.arg, "path");
});

test("a look that cannot read the disk throws: the caller says why, nothing is opened", async () => {
  const broken: Look = { ...look, kindOf: async () => Promise.reject(new Error("listing refused")) };
  await assert.rejects(resolveTarget({ kind: "any", path: "docs" }, broken), /listing refused/);
});

// ─── THE VIEW'S `open` VERB ─────────────────────────────────────────────────

function page(files: FakeBridge): Kernel {
  setCanvasNode("canvas_root");
  const k = new Kernel();
  registerCanvas(k);
  registerLayout(k);
  // the file bridge answers the kernel's host calls, as the real one does
  // a view agent's target (`view:…`, made by open_view) answers as the view; the bridge the rest
  k.bridge = {
    forward: async (t: string, p: Payload) => (await (t.startsWith("view:") ? files.viewCall(t, p as Record<string, unknown>) : files.call(p as Record<string, unknown>))) as never,
    subscribeState: files.subscribeState,
  } as unknown as Kernel["bridge"];
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k.register(new Agent({ id: "view_root", parentId: "canvas", handlerModule: "view.ts" }));
  grantViewLists(k); // what the canvas does at mount
  return k;
}
const call = async (k: Kernel, payload: Record<string, unknown>): Promise<Record<string, unknown>> =>
  (await k.send([{ target: "view_root", payload: payload as Payload }]))[0]!.result as Record<string, unknown>;

const HOST: Record<string, Record<string, unknown>> = {
  ea: { id: "ea", handler_module: "exec_bridge.tools", display_name: "ea" },
};

/** The cells of the view that are not its own layout children: the ones a drop opened. */
const cellsOf = (k: Kernel): string[] => [...k.get("view_root")!.children.values()].filter((c) => c.record()["handler_module"] !== "layout.ts").map((c) => c.id);

test("open is a view verb", () => {
  assert.ok((VIEW_VERBS as readonly string[]).includes("open"));
});

test("open a file: a preview pinned to it, placed at x, y, not following the tree", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  const r = await call(k, { type: "open", path: "docs/a.md", x: 10, y: 20 });
  assert.equal(r["kind"], "file");
  const cell = k.get(String(r["cell"]));
  assert.equal(cell?.record().handler_module, "preview_view.ts");
  assert.equal(cell?.meta["pinned"], "docs/a.md");
  assert.equal(cell?.meta["follow"], false);
  assert.equal(cell?.parentId, "view_root");
});

test("open a folder: a file tree cell rooted at it", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  const r = await call(k, { type: "open", path: "docs" });
  assert.equal(r["kind"], "dir");
  assert.equal(k.get(String(r["cell"]))?.record().handler_module, "files_view.ts");
  assert.equal(k.get(String(r["cell"]))?.meta["root"], "docs");
});

test("open an agent: its cell is placed there, and again it is moved, never a second cell", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  setHostRecords((id) => HOST[id]);
  setHostSync(async () => {});
  const first = await call(k, { type: "open", agent: "ea", x: 5, y: 6 });
  assert.equal(first["kind"], "agent");
  const cell = String(first["cell"]);
  assert.equal(k.get(cell)?.meta["backend_id"], "ea");
  const again = await call(k, { type: "open", agent: "ea", x: 300, y: 40 });
  assert.equal(again["cell"], cell, "the same cell");
  assert.equal(again["moved"], true);
  assert.deepEqual(cellsOf(k), [cell], "one cell for the agent, never a second");
  assert.equal(k.get(cell)?.meta["x"], undefined, "the rect is the view's, not the cell's");
});

test("open an agent that is hidden: its cell is shown again where asked", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  setHostRecords((id) => HOST[id]);
  setHostSync(async () => {});
  const cell = String((await call(k, { type: "open", agent: "ea", x: 5, y: 6 }))["cell"]);
  await call(k, { type: "hide", id: cell });
  assert.ok(hiddenOf(k, "view_root").has(cell));
  const r = await call(k, { type: "open", agent: "ea", x: 70, y: 80 });
  assert.equal(r["shown"], true);
  assert.equal(hiddenOf(k, "view_root").has(cell), false);
});

test("open an agent's folder opens the agent", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  setHostRecords((id) => HOST[id]);
  setHostSync(async () => {});
  agentIndex.set(".fantastic/agents/user/agents/ea", { id: "ea", handler: "exec_bridge.tools", displayName: "", hasParent: true, hasMemory: false });
  const r = await call(k, { type: "open", path: ".fantastic/agents/user/agents/ea" });
  assert.equal(r["kind"], "agent");
  assert.equal(k.get(String(r["cell"]))?.meta["backend_id"], "ea");
});

test("open refuses: no target, both targets, a bad x, an unknown path, an outside path, an unknown agent", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  setHostRecords((id) => HOST[id]);
  setHostSync(async () => {});
  assert.equal((await call(k, { type: "open" }))["reason"], "bad_arg");
  assert.equal((await call(k, { type: "open", path: "docs", agent: "ea" }))["reason"], "bad_arg");
  assert.equal((await call(k, { type: "open", path: "docs", x: 1 }))["reason"], "bad_arg");
  assert.equal((await call(k, { type: "open", path: "nope/x" }))["reason"], "not_found");
  assert.equal((await call(k, { type: "open", path: "../x" }))["reason"], "outside_root");
  assert.equal((await call(k, { type: "open", agent: "ghost" }))["reason"], "not_found");
  assert.deepEqual(cellsOf(k), [], "nothing was opened");
});

test("open: a folder that is not there is not in the project; a refused listing is said loudly", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  // `missing` is not on disk: the path is not in the project
  assert.equal((await call(k, { type: "open", path: "missing/x" }))["reason"], "not_found");
  // `top.txt` is a file: its listing is refused, and the refusal is said, never guessed past
  const r = await call(k, { type: "open", path: "top.txt/x" });
  assert.match(String(r["error"]), /^view: open — /);
  assert.equal(r["reason"], "refused");
  assert.deepEqual(cellsOf(k), []);
});

test("open is refused, not run, when the view was never granted the listing", async () => {
  const b = new FakeBridge();
  project(b);
  setCanvasNode("canvas_root");
  const k = new Kernel();
  registerCanvas(k);
  registerLayout(k);
  k.bridge = { forward: async (_t: string, p: Payload) => (await b.call(p as Record<string, unknown>)) as never, subscribeState: b.subscribeState } as unknown as Kernel["bridge"];
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k.register(new Agent({ id: "view_root", parentId: "canvas", handlerModule: "view.ts" }));
  const r = await call(k, { type: "open", path: "docs/a.md" });
  assert.equal(r["reason"], "refused");
  assert.match(String(r["error"]), /may not call 'list'/);
});

test("open with no x, y: a new cell lands at the corner; an agent's cell keeps its place, and a hidden one comes back where it was", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  setHostRecords((id) => HOST[id]);
  setHostSync(async () => {});
  const rect = (cell: string) => rectsOf(k, "view_root", themeOf(k, "view_root", tabWidth()))[cell];
  // a new folder cell with no x, y: the corner
  const dir = String((await call(k, { type: "open", path: "docs" }))["cell"]);
  assert.deepEqual({ x: rect(dir)?.x, y: rect(dir)?.y }, { x: 0, y: 0 });
  // an agent placed at a spot, then opened again with no spot: it stays put, and is not moved
  const cell = String((await call(k, { type: "open", agent: "ea", x: 300, y: 40 }))["cell"]);
  const before = rect(cell);
  assert.deepEqual({ x: before?.x, y: before?.y }, { x: 300, y: 40 });
  const kept = await call(k, { type: "open", agent: "ea" });
  assert.equal(kept["moved"], false);
  assert.equal(kept["cell"], cell, "the same cell");
  assert.deepEqual(rect(cell), before, "no x, y: the rect is kept");
  // hidden, then opened with no spot: shown where it was
  await call(k, { type: "hide", id: cell });
  const back = await call(k, { type: "open", agent: "ea" });
  assert.equal(back["shown"], true);
  assert.equal(hiddenOf(k, "view_root").has(cell), false);
  assert.deepEqual(rect(cell), before, "a hidden cell comes back where it was, not at 0, 0");
});

test("open on a nested view: a view is granted the listing when it is placed, so a drop there opens what it names", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  k.register(new Agent({ id: "nested", parentId: "view_root", handlerModule: "view.ts" }));
  const nested = async (payload: Record<string, unknown>): Promise<Record<string, unknown>> =>
    (await k.send([{ target: "nested", payload: payload as Payload }]))[0]!.result as Record<string, unknown>;
  // not granted yet: refused, as the root view was before its grant
  assert.match(String((await nested({ type: "open", path: "docs/a.md" }))["error"]), /may not call 'list'/);
  grantViewLists(k); // what the canvas does when the view is added
  const r = await nested({ type: "open", path: "docs/a.md", x: 10, y: 20 });
  assert.equal(r["kind"], "file");
  assert.equal(k.get(String(r["cell"]))?.parentId, "nested", "the cell is the nested view's");
  // the grant is written once: a second pass leaves the view's grants as they are
  const before = k.get("nested")!.meta["can_call"];
  grantViewLists(k);
  assert.deepEqual(k.get("nested")!.meta["can_call"], before);
});

// ─── A TREE ROOTED AT A FOLDER (#135) ───────────────────────────────────────

test("a tree rooted at a folder starts there: only what is under it is rows", async () => {
  const r = rig();
  project(r.bridge);
  const ctl = await r.mountTree("tree_root", "docs");
  assert.deepEqual(ctl.model.rows().map((x) => x.path), ["docs", "docs/a.md"]);
  assert.equal(ctl.root, "docs");
});

test("a rooted tree answers for what is under its root, and refuses the rest", async () => {
  const r = rig();
  project(r.bridge);
  await r.mountTree("tree_root", "docs");
  assert.equal((await r.call("tree_root", { type: "select", path: "top.txt" }))["reason"], "outside_root");
  assert.deepEqual(await r.call("tree_root", { type: "select", path: "docs/a.md" }), { selected: "docs/a.md", kind: "file" });
});

test("a rooted tree restores only what is under its root: a saved path above it is not opened or selected", async () => {
  const r = rig();
  project(r.bridge);
  const ctl = new FilesController({ bridge: r.bridge, hub: r.hub, selfId: "t_rooted_restore", persist: () => {}, index: r.index, walk: () => {}, publish: (s) => r.published.push(s), root: "docs" });
  await ctl.start({ expanded: ["docs", "src", "../escape", ""], selected: "src/one.txt" });
  assert.equal(ctl.model.isOpen("src"), false, "above the root: not opened");
  assert.equal(ctl.model.isOpen("docs"), true, "the root is open from the start");
  assert.equal(ctl.model.selected, null, "a selection above the root is not restored");
  assert.equal(r.published.length, 0);
  ctl.dispose();
});

test("a rooted tree's watch events: a change re-lists the folder it is in, the root is never a child, a removed folder is forgotten and the root stays", () => {
  const m = new TreeModel("docs");
  m.expand("docs");
  m.listed("docs", [
    { name: "sub", path: "docs/sub", type: "dir" },
    { name: "a.md", path: "docs/a.md", type: "file" },
  ]);
  m.expand("docs/sub");
  m.listed("docs/sub", [{ name: "x.txt", path: "docs/sub/x.txt", type: "file" }]);
  // a file added under the root lists the folder it is in
  assert.deepEqual(m.changed({ path: "docs/sub/y.txt", kind: "added" }), [{ kind: "list", path: "docs/sub" }]);
  // the root itself is never listed as somebody's child
  assert.deepEqual(m.changed({ path: "docs", kind: "modified" }), []);
  // a folder deleted: its watch ends, and the root's listing is asked again
  assert.deepEqual(m.changed({ path: "docs/sub", kind: "deleted" }), [{ kind: "unwatch", path: "docs/sub" }, { kind: "list", path: "docs" }]);
  // the listing without it forgets it and what is below it; the root is kept
  m.listed("docs", [{ name: "a.md", path: "docs/a.md", type: "file" }]);
  assert.equal(m.nodes.has("docs/sub"), false);
  assert.equal(m.nodes.has("docs"), true);
  assert.deepEqual(m.rows().map((x) => x.path), ["docs", "docs/a.md"]);
  // the root deleted: its own watch ends
  assert.deepEqual(m.changed({ path: "docs", kind: "deleted" }), [{ kind: "unwatch", path: "docs" }]);
});

test("a file dropped at x, y is laid out there: its rect is the drop's spot, not a free one", async () => {
  const b = new FakeBridge();
  project(b);
  const k = page(b);
  // the page's refresh runs on every state change, and prunes the rects of cells it does not hold yet
  // (canvas.ts refreshView: `prune` with the live cells): a write to the layout must not lose the new cell's rect
  k.addStateSubscriber(() => prune(k, "view_root", new Set([...k.get("view_root")!.children.values()].map((c) => c.id))));
  const r = await call(k, { type: "open", path: "docs/a.md", x: 2140, y: 1118 });
  const cell = String(r["cell"]);
  const theme = themeOf(k, "view_root", tabWidth());
  assert.deepEqual(rectsOf(k, "view_root", theme)[cell], { x: 2140, y: 1118, width: 520, height: 360 });
});
