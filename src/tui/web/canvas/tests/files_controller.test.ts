import { test } from "node:test";
import assert from "node:assert/strict";
import { FilesController, controllers, filesAgent, registerFilesView } from "../src/bundles/files/files_controller.ts";
import { agentIndex, ensureWalker, filesDeps, getWalker, onRender, onSelection, renderFileTrees, rootOf, select, selection, setFilesDeps, drawAll, walkAgain } from "../src/bundles/files/files_deps.ts";
import type { Selection } from "../src/bundles/files/files_deps.ts";
import { settle, FakeBridge } from "./fake_bridge.ts";
import { project, rig } from "./rig.ts";

// THE FILE TREE'S CONTROLLER: what the cell does that is not drawing — the
// restore of a saved tree, the watch each open folder holds (and that a
// dispose ends), the refused listings, the verbs an AI drives it with, and the
// page-wide selection bus the previews follow.

test("start restores the open folders in order (a folder that went away is skipped) and the selection, and says it", async () => {
  const r = rig();
  project(r.bridge);
  r.index.set("src", { id: "src_agent", handler: "x.ts", displayName: "", hasParent: false, hasMemory: false });
  const ctl = new FilesController({ bridge: r.bridge, hub: r.hub, selfId: "t_restore", persist: () => {}, index: r.index, walk: () => {}, publish: (s) => r.published.push(s) });
  await ctl.start({ expanded: ["src/deep", "src", "gone", "../escape", "", 7 as unknown as string], selected: "src/deep/x.ts" });
  assert.equal(ctl.model.isOpen("src"), true);
  assert.equal(ctl.model.isOpen("src/deep"), true, "the parent is opened before the folder below it");
  assert.equal(ctl.model.isOpen("gone"), false);
  assert.equal(ctl.model.selected, "src/deep/x.ts");
  assert.equal(ctl.model.cursor, "src/deep/x.ts");
  assert.deepEqual(r.published.at(-1), { path: "src/deep/x.ts", agent: null });
  await ctl.dispose();
});

test("start with a selection the tree does not have selects nothing; with one it has, the agent there is named", async () => {
  const r = rig();
  project(r.bridge);
  r.index.set("src", { id: "src_agent", handler: "x.ts", displayName: "", hasParent: false, hasMemory: false });
  const ctl = new FilesController({ bridge: r.bridge, hub: r.hub, selfId: "t_sel", persist: () => {}, index: r.index, walk: () => {}, publish: (s) => r.published.push(s) });
  await ctl.start({ selected: "missing/one" });
  assert.equal(ctl.model.selected, null);
  assert.equal(r.published.length, 0);
  ctl.select("src");
  assert.deepEqual(r.published.at(-1), { path: "src", agent: "src_agent" }, "the folder is an agent: its id travels with the path");
  ctl.dispose();
});

test("toggle opens and shuts a folder; onChange is told after each change; setCursor moves only the cursor", async () => {
  const r = rig();
  project(r.bridge);
  const ctl = await r.mountTree("t_toggle");
  let told = 0;
  ctl.onChange(() => told++);
  await ctl.toggle("docs");
  assert.equal(ctl.model.isOpen("docs"), true);
  assert.ok(r.bridge.watched.has("docs"));
  await ctl.toggle("docs");
  assert.equal(ctl.model.isOpen("docs"), false);
  assert.equal(r.bridge.watched.has("docs"), false);
  const before = told;
  ctl.setCursor("top.txt");
  assert.equal(ctl.model.cursor, "top.txt");
  assert.equal(ctl.model.selected, null, "the cursor is not the selection");
  assert.equal(told, before + 1);
});

test("selectAgent selects by id alone: the previews are told no path; the selection is kept on the record", async () => {
  const r = rig();
  project(r.bridge);
  const saved: Array<{ selected: string | null }> = [];
  const ctl = new FilesController({ bridge: r.bridge, hub: r.hub, selfId: "t_agent", persist: (s) => saved.push(s), index: r.index, walk: () => {}, publish: (s) => r.published.push(s) });
  await ctl.start({});
  ctl.selectAgent("page_agent_7");
  assert.equal(ctl.model.selected, null);
  assert.deepEqual(r.published.at(-1), { path: null, agent: "page_agent_7" });
  assert.equal(saved.at(-1)!.selected, null);
  ctl.dispose();
});

test("a listing refused is kept for the row and the verbs; a later good listing clears it", async () => {
  const r = rig();
  project(r.bridge);
  const ctl = await r.mountTree("t_refused");
  const was = r.bridge.call.bind(r.bridge);
  r.bridge.call = async (p) => (p["type"] === "list" && p["path"] === "src" ? { error: "list: denied", reason: "denied" } : was(p));
  await ctl.expand("src");
  assert.equal(ctl.errors.get("src")?.["reason"], "denied");
  assert.equal(ctl.model.nodes.get("src")!.status, "error");
  r.bridge.call = was;
  await ctl.expand("src"); // already open: a fresh listing comes from a change
  r.bridge.put("src/again.txt", "x");
  await settle();
  assert.equal(ctl.errors.has("src"), false);
  assert.equal(ctl.model.nodes.get("src")!.status, "loaded");
  ctl.dispose();
});

test("a listing's entries are read defensively: no files list is empty, a non-dir type is a file, a size only when it is a number", async () => {
  const r = rig();
  project(r.bridge);
  r.bridge.put("odd/x.txt", "x");
  r.bridge.put("mixed/keep.txt", "k");
  const ctl = await r.mountTree("t_defensive");
  const was = r.bridge.call.bind(r.bridge);
  r.bridge.call = async (p) => {
    if (p["type"] === "list" && p["path"] === "odd") return { path: "odd", files: "not a list" };
    if (p["type"] === "list" && p["path"] === "mixed")
      return { path: "mixed", files: [{ name: "w", path: "./mixed/w", type: "weird", size: "12" }, { name: "h", path: "mixed/h", type: "dir" }, { name: "s", path: "mixed/s", type: "file", size: 9 }] };
    return was(p);
  };
  await ctl.expand("odd");
  assert.equal(ctl.model.nodes.get("odd")!.children?.length, 0, "a listing that is not a list has no children");
  await ctl.expand("mixed");
  const kids = ctl.model.nodes.get("mixed")!.children!;
  assert.deepEqual(kids, ["mixed/w", "mixed/h", "mixed/s"]);
  assert.equal(ctl.model.nodes.get("mixed/w")!.type, "file", "a type other than dir is a file");
  assert.equal(ctl.model.nodes.get("mixed/w")!.size, undefined, "a size that is not a number is not kept");
  assert.equal(ctl.model.nodes.get("mixed/s")!.size, 9);
  ctl.dispose();
});

test("dispose during a listing: the listing that returns after it changes nothing, and the watches end", async () => {
  const r = rig();
  project(r.bridge);
  const ctl = await r.mountTree("t_gate");
  let release!: () => void;
  const gate = new Promise<void>((res) => (release = res));
  const was = r.bridge.call.bind(r.bridge);
  r.bridge.call = async (p) => {
    if (p["type"] === "list" && p["path"] === "src") await gate;
    return was(p);
  };
  const pending = ctl.expand("src");
  ctl.dispose();
  release();
  await pending;
  await settle();
  assert.equal(ctl.model.nodes.get("src")!.status, "loading", "the listing that came back after dispose was dropped");
  assert.equal(r.bridge.watched.has("src"), false, "dispose ended the watch");
  assert.equal(r.bridge.watched.has("."), false);
  assert.equal(r.hub.held().length, 0);
});

test("the verbs: a folder's refusal is the reply; a refused expand of an unknown folder says not_found, from the parent's refusal when it has one", async () => {
  const r = rig();
  project(r.bridge);
  await r.mountTree("t_verbs2");
  const was = r.bridge.call.bind(r.bridge);
  r.bridge.call = async (p) => (p["type"] === "list" && p["path"] === "src" ? { error: "list: denied", reason: "denied" } : was(p));
  const x = await r.call("t_verbs2", { type: "expand", path: "src" });
  assert.equal(x["reason"], "denied", "the bridge's refusal, as the row says it");
  r.bridge.call = was;
  const y = await r.call("t_verbs2", { type: "reveal", path: "src/nothing/here" });
  assert.equal(y["reason"], "not_found");
});

test("the verbs: an agent with no folder, a path that is not a string, and a missing argument are each refused", async () => {
  const r = rig();
  project(r.bridge);
  await r.mountTree("t_verbs3");
  assert.equal((await r.call("t_verbs3", { type: "select", agent: "nobody" }))["reason"], "not_found");
  assert.equal((await r.call("t_verbs3", { type: "select" }))["reason"], "bad_arg");
  assert.equal((await r.call("t_verbs3", { type: "select", path: 4 }))["reason"], "bad_arg");
  assert.equal((await r.call("t_verbs3", { type: "" }))["reason"], "bad_arg", "a call with no verb");
  const ok = await r.call("t_verbs3", { type: "select", path: "src/one.txt" });
  assert.equal(ok["kind"], "file");
  assert.equal(ok["agent"], undefined, "a file that is no agent carries no agent id");
});

test("the verbs: collapse says what was open; select of an agent's folder carries the agent id", async () => {
  const r = rig();
  project(r.bridge);
  await r.mountTree("t_verbs4");
  r.index.set("src", { id: "agent_src", handler: "x.ts", displayName: "", hasParent: false, hasMemory: false });
  const s = await r.call("t_verbs4", { type: "select", path: "src" });
  assert.deepEqual(s, { selected: "src", kind: "dir", agent: "agent_src" });
  assert.deepEqual(r.published.at(-1), { path: "src", agent: "agent_src" }, "the previews are told the same");
});

test("filesAgent is the handler the kernel calls: it needs a cell on screen (registered by the file tree view)", async () => {
  assert.deepEqual(await filesAgent("nobody", { type: "select", path: "a" }), { error: "files: the cell is not on screen", reason: "not_mounted", hint: "the cell is not on screen" });
});

test("registerFilesView: the tree's name and the old sidebar's name both register the handler, with the readme", () => {
  const calls: string[] = [];
  registerFilesView({
    registerBundle: (hm, _h, verbs) => calls.push(`bundle ${hm} ${Object.keys(verbs ?? {}).join(",")}`),
    setBundleReadme: (hm, readme) => calls.push(`readme ${hm} ${readme.slice(0, 11)}`),
  });
  assert.deepEqual(calls, [
    "bundle files_view.ts select,expand,collapse,reveal,reflect",
    "readme files_view.ts A FILE TREE",
    "bundle sidebar_view.ts select,expand,collapse,reveal,reflect",
    "readme sidebar_view.ts legacy name",
  ]);
  controllers.clear();
});

// ─── THE PAGE'S DEPS AND BUS (files_deps.ts) ──────────────────────────────

test("filesDeps refuses until the canvas hands its wiring over; the wiring is what it was given", () => {
  assert.throws(() => filesDeps("the test"), /the canvas is not mounted/);
  const deps = { hostRecords: () => [], pageAgents: () => [] } as unknown as Parameters<typeof setFilesDeps>[0];
  setFilesDeps(deps);
  assert.equal(filesDeps("the test"), deps);
});

test("the selection bus: a selection reaches every listener with who made it; an unsubscribed one hears nothing more", () => {
  const seen: Array<[Selection | null, string]> = [];
  const off = onSelection((s, from) => seen.push([s, from]));
  const other: string[] = [];
  onSelection(() => other.push("x"));
  select({ path: "a", agent: null }, "tree");
  assert.deepEqual(seen, [[{ path: "a", agent: null }, "tree"]]);
  assert.deepEqual(selection(), { path: "a", agent: null });
  off();
  select(null, "preview");
  assert.equal(seen.length, 1, "unsubscribed");
  assert.equal(selection(), null);
  assert.equal(other.length, 2);
});

test("the draw bus: every drawer is called; an unsubscribed one is not; rootOf is the first record with no parent in the set", () => {
  let a = 0;
  let b = 0;
  const offA = onRender(() => a++);
  onRender(() => b++);
  drawAll();
  offA();
  drawAll();
  assert.equal(a, 1);
  assert.equal(b, 2);
  assert.equal(rootOf([{ id: "c", parent_id: "p" }, { id: "b", parent_id: "c" }, { id: "a" }]), "a", "the first by id whose parent is not in the set");
  assert.equal(rootOf([{ id: "x", parent_id: "y" }]), "x", "a parent outside the set makes the record a root");
  assert.equal(rootOf([]), "");
});

test("renderFileTrees: the agent folders are read again when the set of agents changed, and only then", async () => {
  const b = new FakeBridge();
  b.put(".fantastic/agents/a/agent.yaml", "id: a\nhandler_module: x.ts\n");
  const w = ensureWalker(b);
  assert.equal(ensureWalker(b), w, "one walker per page");
  await w.request();
  const lists = () => b.calls.filter((c) => c["type"] === "list" && c["path"] === ".fantastic").length;
  const start = lists();
  let deps = {
    hostRecords: () => [{ id: "h1" }],
    pageAgents: () => [{ id: "p1" }],
  } as unknown as Parameters<typeof setFilesDeps>[0];
  setFilesDeps(deps);
  let drawn = 0;
  const off = onRender(() => drawn++);
  renderFileTrees();
  await settle();
  assert.equal(lists(), start + 1, "a new set of agents: the folders are read");
  renderFileTrees();
  await settle();
  assert.equal(lists(), start + 1, "the same set: no read");
  assert.equal(drawn, 3, "each call draws every tree; the walk the first call started draws once more when it lands");
  deps = { hostRecords: () => [], pageAgents: () => [] } as unknown as Parameters<typeof setFilesDeps>[0];
  setFilesDeps(deps);
  renderFileTrees();
  await settle();
  assert.equal(lists(), start + 2, "the set changed again");
  off();
  walkAgain();
  await settle();
  assert.ok(agentIndex.ids().includes("a"), "the index holds what the walk read");
});

test("a call after dispose starts nothing: no listing, no watch, no hold", async () => {
  const r = rig();
  project(r.bridge);
  const ctl = await r.mountTree("t_after_dispose");
  ctl.dispose();
  const before = r.bridge.calls.length;
  await ctl.expand("src");
  await ctl.reveal("src/deep");
  assert.equal(r.bridge.calls.slice(before).filter((c) => c["type"] === "list" || c["type"] === "watch_path").length, 0);
  assert.deepEqual(r.hub.held(), []);
});

test("the verbs: a reveal through a folder that refused says so from that folder's row; a filesAgent call with no verb is refused", async () => {
  const r = rig();
  project(r.bridge);
  await r.mountTree("t_reveal_refused");
  const was = r.bridge.call.bind(r.bridge);
  r.bridge.call = async (p) => (p["type"] === "list" && p["path"] === "src" ? { error: "list: denied", reason: "denied" } : was(p));
  const x = await r.call("t_reveal_refused", { type: "reveal", path: "src/nothing" });
  assert.equal(x["reason"], "denied", "the parent's refusal, as its row shows it");
  r.bridge.call = was;
  assert.equal((await filesAgent("t_reveal_refused", {}))["reason"], "bad_arg");
});

test("a change under an open folder of .fantastic asks the agent folders to be read again; a change of content does not", async () => {
  const r = rig();
  project(r.bridge);
  let walks = 0;
  const ctl = new FilesController({ bridge: r.bridge, hub: r.hub, selfId: "t_walk_on", persist: () => {}, index: r.index, walk: () => walks++, publish: () => {} });
  controllers.set("t_walk_on", ctl);
  await ctl.expand("");
  await ctl.expand(".fantastic");
  r.bridge.put(".fantastic/new.yaml", "id: n\n");
  await settle();
  assert.equal(walks, 1, "an added entry under .fantastic is an agent folder that may have come");
  r.bridge.put(".fantastic/new.yaml", "id: changed\n");
  await settle();
  assert.equal(walks, 1, "a modified file under it is not");
  r.bridge.remove(".fantastic/new.yaml");
  await settle();
  assert.equal(walks, 2, "a removed one is");
  ctl.dispose();
});

test("start does not overwrite the saved folders with an empty list", async () => {
  const r = rig();
  project(r.bridge);
  const saved: Array<{ expanded: string[]; selected: string | null }> = [];
  const ctl = new FilesController({ bridge: r.bridge, hub: r.hub, selfId: "t_overwrite", persist: (s) => saved.push({ ...s }), index: r.index, walk: () => {}, publish: () => {} });
  await ctl.start({ expanded: ["src", "src/deep"] });
  assert.ok(saved.length > 0, "the restore saves the tree");
  for (const s of saved) assert.notDeepEqual(s.expanded, [], "a restore never saves an empty open set");
  assert.deepEqual(saved.at(-1)!.expanded, ["src", "src/deep"], "the last save lists both folders");
  ctl.dispose();
});

test("start saves once, not once per folder", async () => {
  const r = rig();
  project(r.bridge);
  const saved: Array<{ expanded: string[]; selected: string | null }> = [];
  const ctl = new FilesController({ bridge: r.bridge, hub: r.hub, selfId: "t_once", persist: (s) => saved.push({ ...s }), index: r.index, walk: () => {}, publish: () => {} });
  await ctl.start({ expanded: ["src", "src/deep"] });
  assert.equal(saved.length, 1, "the whole restore is one save");
  assert.deepEqual(saved[0].expanded, ["src", "src/deep"]);
  ctl.dispose();
});

test("a folder that disappears is saved closed", async () => {
  const r = rig();
  project(r.bridge);
  const saved: Array<{ expanded: string[]; selected: string | null }> = [];
  const ctl = new FilesController({ bridge: r.bridge, hub: r.hub, selfId: "t_gone", persist: (s) => saved.push({ ...s }), index: r.index, walk: () => {}, publish: () => {} });
  await ctl.start({ expanded: ["src", "src/deep"] });
  assert.equal(ctl.model.isOpen("src/deep"), true);
  r.bridge.remove("src/deep"); // the tree closes it on its own
  await settle();
  assert.equal(ctl.model.isOpen("src/deep"), false);
  assert.ok(saved.length > 1, "the closing is saved too");
  assert.deepEqual(saved.at(-1)!.expanded, ["src"], "the record no longer lists the folder that went away");
  ctl.dispose();
});

test("the page's walker is the one ensureWalker made, and getWalker says so", async () => {
  const b = new FakeBridge();
  const w = ensureWalker(b);
  assert.equal(getWalker(), w);
  await w.request();
});
