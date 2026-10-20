import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { WatchHub } from "../src/bundles/files/file_bridge.ts";
import { AgentIndex } from "../src/bundles/files/agent_index.ts";
import { FilesController, controllers, registerFilesView } from "../src/bundles/files/files_controller.ts";
import type { Selection } from "../src/bundles/files/files_deps.ts";
import { FakeBridge, settle } from "./fake_bridge.ts";

type Dict = Record<string, unknown>;

// THE VERBS of the file tree and the previewer, driven through a Kernel as an AI
// drives them, over an in-memory file bridge.

interface Rig {
  k: Kernel;
  bridge: FakeBridge;
  hub: WatchHub;
  index: AgentIndex;
  published: Array<Selection>;
  saved: Dict[];
  mountTree(id: string): Promise<FilesController>;
  call(target: string, p: Dict): Promise<Dict>;
}

function rig(): Rig {
  const bridge = new FakeBridge();
  const hub = new WatchHub(bridge, bridge.subscribe);
  const index = new AgentIndex();
  const k = new Kernel();
  registerFilesView(k);
  k.registerBundle("canvas.ts", () => null);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  const published: Selection[] = [];
  const saved: Dict[] = [];
  const mountTree = async (id: string): Promise<FilesController> => {
    k.register(new Agent({ id, parentId: "canvas", handlerModule: "files_view.ts" }));
    const ctl = new FilesController({
      bridge,
      hub,
      selfId: id,
      persist: (s) => saved.push({ ...s }),
      index,
      walk: () => {},
      publish: (s) => published.push(s),
    });
    controllers.set(id, ctl);
    await ctl.start({});
    return ctl;
  };
  const call = async (target: string, p: Dict): Promise<Dict> => ((await k.send([{ target, payload: p as Payload }]))[0]!.result ?? {}) as Dict;
  return { k, bridge, hub, index, published, saved, mountTree, call };
}

function project(b: FakeBridge): void {
  b.put("docs/a.md", "# Hello\n");
  b.put("src/one.txt", "one");
  b.put("src/deep/x.ts", "export {};\n");
  b.put("top.txt", "t");
  b.put(".fantastic/agents/user/agents/files/agent.yaml", "id: files\nhandler_module: file_bridge.tools\n");
}

test("the tree opens the root: listed and watched; folders and files in the model", async () => {
  const r = rig();
  project(r.bridge);
  const ctl = await r.mountTree("tree_1");
  assert.deepEqual([...r.bridge.watched], ["."]);
  assert.deepEqual(ctl.model.rows().map((x) => x.path), ["", ".fantastic", "docs", "src", "top.txt"]);
});

test("select: opens the folders above, selects, publishes to the previews; replies kind (and agent)", async () => {
  const r = rig();
  project(r.bridge);
  await r.mountTree("tree_1");
  r.index.set(".fantastic/agents/user/agents/files", { id: "files", handler: "file_bridge.tools", displayName: "", hasParent: true, hasMemory: false });
  const a = await r.call("tree_1", { type: "select", path: "docs/a.md" });
  assert.deepEqual(a, { selected: "docs/a.md", kind: "file" });
  assert.deepEqual(r.published.at(-1), { path: "docs/a.md", agent: null });
  assert.ok(r.bridge.watched.has("docs"), "the folder above is open, so watched");
  const b = await r.call("tree_1", { type: "select", agent: "files" });
  assert.deepEqual(b, { selected: ".fantastic/agents/user/agents/files", kind: "dir", agent: "files" });
  assert.deepEqual(r.published.at(-1), { path: ".fantastic/agents/user/agents/files", agent: "files" });
  assert.deepEqual(r.saved.at(-1)!["selected"], ".fantastic/agents/user/agents/files");
});

test("select, expand, collapse and reveal refuse: bad_arg, outside_root, not_found, not_a_dir", async () => {
  const r = rig();
  project(r.bridge);
  await r.mountTree("tree_1");
  for (const verb of ["select", "expand", "collapse", "reveal"]) {
    assert.equal((await r.call("tree_1", { type: verb }))["reason"], "bad_arg", verb);
    assert.equal((await r.call("tree_1", { type: verb, path: "/etc/passwd" }))["reason"], "outside_root", verb);
    assert.equal((await r.call("tree_1", { type: verb, path: "a/../../b" }))["reason"], "outside_root", verb);
  }
  assert.equal((await r.call("tree_1", { type: "select", path: "nope/x" }))["reason"], "not_found");
  assert.equal((await r.call("tree_1", { type: "expand", path: "nope" }))["reason"], "not_found");
  assert.equal((await r.call("tree_1", { type: "select", agent: "ghost" }))["reason"], "not_found");
  const f = await r.call("tree_1", { type: "expand", path: "top.txt" });
  assert.equal(f["reason"], "not_a_dir");
  const u = await r.call("tree_1", { type: "frobnicate" });
  assert.equal(u["reason"], "unhandled");
  assert.equal(u["verb"], "frobnicate");
});

test("expand replies the entries; collapse replies was_open and unwatches the subtree; reveal leaves the selection", async () => {
  const r = rig();
  project(r.bridge);
  const ctl = await r.mountTree("tree_1");
  assert.deepEqual(await r.call("tree_1", { type: "expand", path: "src" }), { expanded: "src", entries: 2 });
  await r.call("tree_1", { type: "expand", path: "src/deep" });
  assert.ok(r.bridge.watched.has("src/deep"));
  assert.deepEqual(await r.call("tree_1", { type: "collapse", path: "src" }), { collapsed: "src", was_open: true });
  assert.ok(!r.bridge.watched.has("src") && !r.bridge.watched.has("src/deep"));
  assert.deepEqual(await r.call("tree_1", { type: "collapse", path: "src" }), { collapsed: "src", was_open: false });
  assert.deepEqual(await r.call("tree_1", { type: "reveal", path: "src/deep/x.ts" }), { revealed: "src/deep/x.ts" });
  assert.equal(ctl.model.selected, null);
});

test("a file made, changed and removed on disk shows in the open folder without a reload", async () => {
  const r = rig();
  project(r.bridge);
  const ctl = await r.mountTree("tree_1");
  await r.call("tree_1", { type: "expand", path: "src" });
  r.bridge.put("src/new.txt", "n");
  await settle();
  assert.ok(ctl.model.nodes.has("src/new.txt"));
  r.bridge.remove("src/new.txt");
  await settle();
  assert.equal(ctl.model.nodes.has("src/new.txt"), false);
  r.bridge.put("closed/also.txt", "x"); // a folder nobody opened: nothing to re-list
  await settle();
  assert.equal(ctl.model.nodes.has("closed"), true, "the root was re-listed and shows the new folder");
  assert.equal(ctl.model.nodes.has("closed/also.txt"), false);
});

test("a verb to a cell that is not on screen is refused not_mounted", async () => {
  const r = rig();
  r.k.register(new Agent({ id: "tree_ghost", parentId: "canvas", handlerModule: "files_view.ts" }));
  const x = await r.call("tree_ghost", { type: "select", path: "a" });
  assert.equal(x["reason"], "not_mounted");
});

test("two trees on one folder share one hold; closing one keeps the other live; closing both leaves none", async () => {
  const r = rig();
  project(r.bridge);
  const a = await r.mountTree("tree_a");
  const b = await r.mountTree("tree_b");
  await r.call("tree_a", { type: "expand", path: "src" });
  await r.call("tree_b", { type: "expand", path: "src" });
  a.dispose();
  controllers.delete("tree_a");
  assert.ok(r.bridge.watched.has("src") && r.bridge.watched.has("."), "tree_b still holds them");
  r.bridge.put("src/live.txt", "x");
  await settle();
  assert.ok(b.model.nodes.has("src/live.txt"), "tree_b still follows the disk");
  b.dispose();
  controllers.delete("tree_b");
  await settle();
  assert.deepEqual([...r.bridge.watched], [], "no watch leaked");
  assert.equal(r.bridge.subscribed, false, "the event source ended with the last hold");
});

test("a watch the bridge refuses closes the folder and says why on its row", async () => {
  const r = rig();
  project(r.bridge);
  const ctl = await r.mountTree("tree_1");
  const was = r.bridge.call.bind(r.bridge);
  r.bridge.call = async (p) => (p["type"] === "watch_path" && p["path"] === "src" ? { error: "watch_path: denied", reason: "denied" } : was(p));
  const x = await r.call("tree_1", { type: "expand", path: "src" });
  assert.equal(x["reason"], "watch_refused");
  assert.equal(ctl.model.isOpen("src"), false);
  assert.match(ctl.model.nodes.get("src")!.error ?? "", /denied/);
  assert.ok(!r.bridge.watched.has("src"));
});
