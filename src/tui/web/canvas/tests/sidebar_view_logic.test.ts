import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

// THE SIDEBAR VIEW'S PURE DATA SHAPING (reflect tree -> rows, the chat set, state, waiting rows,
// file rows). The view (`src/tui/src/kern/sidebar_view.html`) keeps it in one `// pure` script
// block: it is read here and run in a bare vm context, no DOM.

const HTML = fileURLToPath(new URL("../../../src/kern/sidebar_view.html", import.meta.url));
const src = readFileSync(HTML, "utf8");
const from = src.indexOf("// pure\n");
const to = src.indexOf("// end pure");
assert.ok(from >= 0 && to > from, "the view marks its pure block");

type Obj = Record<string, unknown>;
interface Pure {
  kernelStrip(root: Obj, peers: Obj[], waiting?: number): Obj[];
  askAges(inq: Obj[], now: number): Record<string, string>;
  toolRows(root: Obj, chats: Record<string, boolean>): Obj[];
  pinRows(root: Obj, chats: Record<string, boolean>, folded: Record<string, boolean>, status: Record<string, Obj>, asks: Record<string, boolean>, files: Obj, project: string, opts?: Obj): Obj[];
  projectName(p: unknown): string;
  chatSet(reply: Obj): Record<string, boolean>;
  stateOf(status: Obj | undefined, asks: boolean): { kind: string; label: string };
  treeRows(root: Obj, chats: Record<string, boolean>, folded: Record<string, boolean>, status: Record<string, Obj>, asks: Record<string, boolean>, files?: Obj, opts?: Obj): Obj[];
  filterRows(rows: Obj[], q: string): Obj[];
  keptState(st: Obj): Obj;
  restoredState(saved: unknown): Obj;
  wantedWatches(filesOn: boolean, open: Record<string, boolean>): string[];
  changedDirs(ev: Obj, lists: Record<string, Obj[]>): string[];
  askers(inq: Obj[]): Record<string, boolean>;
  waitingRows(inq: Obj[], names: Record<string, string>, now: number): Obj[];
  sortedListing(files: Obj[]): Obj[];
  fileRows(lists: Record<string, Obj[]>, open: Record<string, boolean>, path: string, depth: number): Obj[];
}
const ctx = vm.createContext({});
vm.runInContext(`${src.slice(from, to)}\nthis.api = { askAges, toolRows, pinRows, projectName, kernelStrip, chatSet, stateOf, treeRows, filterRows, keptState, restoredState, wantedWatches, changedDirs, askers, waitingRows, sortedListing, fileRows };`, ctx);
const raw = (ctx as unknown as { api: Pure }).api;
// the vm's objects are of another realm: compare them as plain JSON
const plain = <T>(x: T): T => JSON.parse(JSON.stringify(x)) as T;

const tree = {
  id: "kernel_state", display_name: "kernel_state", handler_module: null,
  children: [
    { id: "user", display_name: "user", handler_module: "user.tools", children: [
      { id: "files", display_name: "files", handler_module: "file_bridge.tools", children: [] },
      { id: "coder", display_name: "Coder", handler_module: "role_x.tools", children: [] },
    ] },
    { id: "chat1", display_name: "chat1", handler_module: "ai_chat.tools", children: [] },
  ],
};

test("the kernel strip: this kernel first and highlighted, then each peer by short guid", () => {
  const rows = plain(raw.kernelStrip({ id: "kernel_state", display_name: "demo", guid: "abcdef123456" }, [{ guid: "99aabbccdd", via: "ws" }, { guid: "" }, {} as Obj]));
  assert.equal(rows.length, 2);
  assert.deepEqual([rows[0]!["here"], rows[0]!["mark"], rows[0]!["name"]], [true, "de", "demo"]);
  assert.deepEqual([rows[1]!["here"], rows[1]!["mark"], rows[1]!["name"], rows[1]!["guid"]], [false, "99", "99aabb", "99aabbccdd"]);
  assert.equal(rows[0]!["waiting"], 0, "no badge without a question");
});

test("the current kernel carries the number of questions waiting in it", () => {
  const rows = plain(raw.kernelStrip({ id: "k", display_name: "demo", guid: "abcdef" }, [{ guid: "99aabbccdd" }], 3));
  assert.deepEqual(rows.map((r) => r["waiting"]), [3, undefined], "a peer has no source for it, so no badge");
});

test("a folded branch counts the questions below it; an asking row carries its age", () => {
  const t = { id: "r", display_name: "r", children: [{ id: "a", display_name: "a", children: [{ id: "b", display_name: "b", children: [] }, { id: "c", display_name: "c", children: [] }] }] };
  const rows = plain(raw.treeRows(t, {}, { a: true }, {}, { b: true, a: true }, undefined, { ages: { a: "12m", b: "3m" } }));
  assert.deepEqual(rows.map((r) => [r["id"], r["asksBelow"], r["age"]]), [["r", 0, ""], ["a", 1, "12m"]], "the branch's own question is not counted below it");
  const open = plain(raw.treeRows(t, {}, {}, {}, { b: true }, undefined, { ages: { b: "3m" } }));
  assert.deepEqual(open.map((r) => [r["id"], r["asksBelow"], r["age"]]), [["r", 0, ""], ["a", 0, ""], ["b", 0, "3m"], ["c", 0, ""]], "an open branch counts nothing");
});

test("ask ages: the oldest question of each asker", () => {
  assert.deepEqual(plain(raw.askAges([{ asker: "a", ts: 400 }, { asker: "a", ts: 100 }, { asker: "b", ts: 940 }, { ts: 1 }, { asker: "c" }], 1000)), { a: "15m", b: "1m" });
});

test("leaf tool agents under the root go to the tools group, not the tree", () => {
  const rows = plain(raw.treeRows(tree, { chat1: true }, {}, {}, {}, undefined, { tools: true }));
  assert.deepEqual(rows.map((r) => r["id"]), ["kernel_state", "user", "files", "coder", "chat1"], "user has children, files is the files agent: both stay; only direct leaf tools leave");
  const t2 = { id: "r", display_name: "r", children: [{ id: "tasks", display_name: "tasks", handler_module: "task.tools", children: [] }, { id: "c", display_name: "c", handler_module: "ai_chat.tools", children: [] }, { id: "files", display_name: "files", children: [] }] };
  const tr = plain(raw.treeRows(t2, { c: true }, {}, {}, {}, undefined, { tools: true }));
  assert.deepEqual(tr.map((r) => r["id"]), ["r", "c", "files"]);
  assert.deepEqual(plain(raw.toolRows(t2, { c: true })).map((r) => [r["id"], r["tag"], r["depth"]]), [["tasks", "task", 0]]);
  assert.deepEqual(plain(raw.treeRows(t2, { c: true }, {}, {}, {})).map((r) => r["id"]), ["r", "tasks", "c", "files"], "without the option the tree keeps them");
});

test("the tree rows keep parent order, nested depth, and the order reflect gives", () => {
  const rows = plain(raw.treeRows(tree, {}, {}, {}, {}));
  assert.deepEqual(rows.map((r) => [r["id"], r["depth"]]), [["kernel_state", 0], ["user", 1], ["files", 2], ["coder", 2], ["chat1", 1]]);
  assert.equal(rows[1]!["tag"], "user", "the handler is the small tag, `.tools` cut");
  assert.equal(rows[3]!["name"], "Coder", "the display name");
});

test("a role over ai_chat.tools is a chat when the handler_modules reply lists it", () => {
  const chats = plain(raw.chatSet({ tree: ["coder", "chat1", 7] }));
  assert.deepEqual(chats, { coder: true, chat1: true });
  const rows = plain(raw.treeRows(tree, chats, {}, {}, {}));
  assert.deepEqual(rows.map((r) => [r["id"], r["chat"]]), [["kernel_state", false], ["user", false], ["files", false], ["coder", true], ["chat1", true]]);
  assert.equal(rows.find((r) => r["id"] === "files")!["state"], null, "only a chat has a state");
  assert.deepEqual(plain(raw.chatSet({ error: "x" })), {}, "no list: no chats");
});

test("a folded branch shows its child count and hides what is below", () => {
  const rows = plain(raw.treeRows(tree, {}, { user: true }, {}, {}));
  assert.deepEqual(rows.map((r) => r["id"]), ["kernel_state", "user", "chat1"]);
  assert.equal(rows[1]!["folded"], true);
  assert.equal(rows[1]!["below"], 2);
  assert.equal(rows[1]!["kids"], 2);
});

test("state: asks > active phases > queued > idle", () => {
  const S = (phase?: string): Obj => ({ current: phase === undefined ? null : { phase } });
  assert.deepEqual(plain(raw.stateOf(S("thinking"), true)).kind, "ask");
  for (const p of ["thinking", "streaming", "tool_calling"]) assert.equal(plain(raw.stateOf(S(p), false)).kind, "active", p);
  assert.equal(plain(raw.stateOf(S("queued"), false)).kind, "queued");
  assert.equal(plain(raw.stateOf(S("done"), false)).kind, "idle");
  assert.equal(plain(raw.stateOf(S(), false)).kind, "idle");
  assert.equal(plain(raw.stateOf(undefined, false)).kind, "idle");
  const rows = plain(raw.treeRows(tree, { chat1: true }, {}, { chat1: S("streaming") }, { chat1: true }));
  assert.equal((rows.find((r) => r["id"] === "chat1")!["state"] as Obj)["kind"], "ask");
});

test("waiting rows: one per inquiry, the oldest first, one line, an age", () => {
  const inq = [
    { asker: "b", text: "second\nline  here", ts: 900, re: 2 },
    { asker: "a", text: "first", ts: 100, re: 1 },
    { text: "no asker", ts: 1 },
  ];
  const rows = plain(raw.waitingRows(inq, { a: "Alpha" }, 1000));
  assert.deepEqual(rows.map((r) => r["asker"]), ["a", "b"]);
  assert.deepEqual(rows.map((r) => r["who"]), ["Alpha", "b"], "the asker's name when known");
  assert.equal(rows[1]!["text"], "second line here");
  assert.deepEqual(rows.map((r) => r["age"]), ["15m", "2m"]);
  assert.deepEqual(plain(raw.askers(inq)), { a: true, b: true });
});

test("a folder listing puts directories first, then names", () => {
  const files = [
    { name: "b.txt", path: "b.txt", type: "file" },
    { name: "Zed", path: "Zed", type: "dir" },
    { name: "a.txt", path: "a.txt", type: "file" },
    { name: "alpha", path: "alpha", type: "dir" },
  ];
  assert.deepEqual(plain(raw.sortedListing(files)).map((f) => f["name"]), ["alpha", "Zed", "a.txt", "b.txt"]);
});

test("file rows nest the open folders, listed sorted", () => {
  const lists = {
    ".": [{ name: "f.md", path: "f.md", type: "file" }, { name: "src", path: "src", type: "dir" }, { name: "docs", path: "docs", type: "dir" }],
    src: [{ name: "main.rs", path: "src/main.rs", type: "file" }, { name: "kern", path: "src/kern", type: "dir" }],
  };
  const rows = plain(raw.fileRows(lists, { src: true }, ".", 2));
  assert.deepEqual(rows.map((r) => [r["path"], r["depth"], r["open"]]), [["docs", 2, false], ["src", 2, true], ["src/kern", 3, false], ["src/main.rs", 3, false], ["f.md", 2, false]]);
});

const lists = {
  ".": [{ name: "f.md", path: "f.md", type: "file" }, { name: "src", path: "src", type: "dir" }, { name: ".git", path: ".git", type: "dir" }, { name: "node_modules", path: "node_modules", type: "dir" }],
  src: [{ name: "main.rs", path: "src/main.rs", type: "file" }],
};

test("file rows leave out the folders a tree hides (.git, node_modules ...)", () => {
  const rows = plain(raw.fileRows(lists, {}, ".", 0));
  assert.deepEqual(rows.map((r) => r["path"]), ["src", "f.md"]);
  assert.ok(rows.every((r) => r["kind"] === "file"));
});

test("files show INSIDE the tree, under the files agent, only while the toggle is on", () => {
  const off = plain(raw.treeRows(tree, {}, {}, {}, {}, { on: false, lists, open: {} }));
  assert.deepEqual(off.map((r) => r["id"]), ["kernel_state", "user", "files", "coder", "chat1"], "no file rows with the toggle off");
  const on = plain(raw.treeRows(tree, {}, {}, {}, {}, { on: true, lists, open: { src: true } }));
  assert.deepEqual(on.map((r) => r["id"] ?? r["path"]), ["kernel_state", "user", "files", "src", "src/main.rs", "f.md", "coder", "chat1"]);
  assert.deepEqual(on.filter((r) => r["kind"] === "file").map((r) => r["depth"]), [3, 4, 3], "one level below the files agent");
  assert.equal(on.find((r) => r["id"] === "files")!["foldable"], true, "the files agent folds over its rows though it has no agent children");
});

test("folding the files agent hides its file rows and counts them", () => {
  const rows = plain(raw.treeRows(tree, {}, { files: true }, {}, {}, { on: true, lists, open: {} }));
  assert.deepEqual(rows.map((r) => r["id"] ?? r["path"]), ["kernel_state", "user", "files", "coder", "chat1"]);
  assert.equal(rows.find((r) => r["id"] === "files")!["below"], 2);
});

test("the filter keeps the matches and the branch that leads to them", () => {
  const rows = plain(raw.treeRows(tree, {}, {}, {}, {}, { on: true, lists, open: { src: true } }));
  assert.deepEqual(plain(raw.filterRows(rows, "main")).map((r) => r["id"] ?? r["path"]), ["kernel_state", "user", "files", "src", "src/main.rs"], "a match keeps the rows above it, not its siblings");
  assert.equal(raw.filterRows(rows, "  ").length, rows.length, "a blank filter keeps all");
  assert.deepEqual(plain(raw.filterRows(rows, "CODER")).map((r) => r["id"]), ["kernel_state", "user", "coder"], "case does not matter");
});

test("what the view keeps across a reload: folded branches, open folders, the files switch", () => {
  const kept = plain(raw.keptState({ folded: { user: true }, open: { src: true }, filesOn: true, lists: { ".": [] }, selected: "x" }));
  assert.deepEqual(kept, { folded: { user: true }, open: { src: true }, filesOn: true, toolsOpen: false });
  assert.deepEqual(plain(raw.restoredState(kept)), kept);
  assert.deepEqual(plain(raw.restoredState({ folded: { a: true, b: false, c: "yes" }, open: 3, filesOn: "true" })), { folded: { a: true }, open: {}, filesOn: false, toolsOpen: false }, "only what is exactly true comes back");
  assert.deepEqual(plain(raw.restoredState(null)), { folded: {}, open: {}, filesOn: false, toolsOpen: false });
  assert.equal(plain(raw.restoredState({ toolsOpen: true }))["toolsOpen"], true);
});

test("a watch is held on the root and every open folder while files show, none otherwise", () => {
  assert.deepEqual(plain(raw.wantedWatches(true, { src: true, docs: false })), [".", "src"]);
  assert.deepEqual(plain(raw.wantedWatches(false, { src: true })), []);
});

test("a change reads again the folder that holds it, and the folder itself when listed", () => {
  const held = { ".": [], src: [] };
  assert.deepEqual(plain(raw.changedDirs({ type: "changed", path: "src/a.rs", kind: "added" }, held)), ["src"]);
  assert.deepEqual(plain(raw.changedDirs({ type: "changed", path: "src", kind: "modified" }, held)), [".", "src"]);
  assert.deepEqual(plain(raw.changedDirs({ type: "changed", path: "other/x", kind: "added" }, held)), [], "a folder the view does not list is not read");
  assert.deepEqual(plain(raw.changedDirs({ type: "watching" }, held)), []);
});

test("the pinned rows: the files agent, the project folder under it, its files while they show, then its file views", () => {
  const root = { id: "r", display_name: "r", children: [
    { id: "files", display_name: "files", handler_module: "file_bridge.tools", children: [{ id: "view1", display_name: "view1", handler_module: "file_view.tools", children: [] }] },
    { id: "c", display_name: "c", handler_module: "ai_chat.tools", children: [] },
  ] };
  const lists = { ".": [{ name: "src", path: "src", type: "dir" }, { name: "a.txt", path: "a.txt", type: "file" }] };
  const off = plain(raw.pinRows(root, { c: true }, {}, {}, {}, { on: false, lists, open: {} }, "demo"));
  assert.deepEqual(off.map((r) => [r["kind"], r["id"] ?? r["name"], r["depth"]]), [["agent", "files", 0], ["project", "demo", 1], ["agent", "view1", 1]]);
  assert.equal(off[1]!["open"], false);
  assert.equal(off[0]!["tag"], "tool", "the pinned files row carries the tool tag, not its handler's");
  assert.equal(off[2]!["tag"], "file_view", "other pinned agent rows keep their handler tag");
  const on = plain(raw.pinRows(root, { c: true }, {}, {}, {}, { on: true, lists, open: {} }, "demo"));
  assert.deepEqual(on.map((r) => r["name"]), ["files", "demo", "src", "a.txt", "view1"]);
  assert.deepEqual(on.map((r) => r["depth"]), [0, 1, 2, 2, 1]);
  assert.deepEqual(plain(raw.pinRows(root, {}, {}, {}, {}, { on: false, lists: {}, open: {} }, "")).map((r) => r["name"]), ["files", "view1"], "no project name: no folder row");
  assert.deepEqual(plain(raw.pinRows({ id: "r", children: [] }, {}, {}, {}, {}, { on: true, lists, open: {} }, "demo")), [], "no files agent: nothing pinned");
  assert.deepEqual(plain(raw.treeRows(root, { c: true }, {}, {}, {}, undefined, { pinned: true })).map((r) => r["id"]), ["r", "c"], "pinned: the files agent leaves the tree");
  assert.deepEqual(plain(raw.treeRows(root, { c: true }, {}, {}, {}, undefined, {})).map((r) => r["id"]), ["r", "files", "view1", "c"]);
});

test("the project name is the last part of the files root", () => {
  assert.deepEqual(["/a/b/launch-week", "/a/b/launch-week/", "x", "", null].map((p) => raw.projectName(p)), ["launch-week", "launch-week", "x", "", ""]);
});
