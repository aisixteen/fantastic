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
  kernelStrip(root: Obj, peers: Obj[]): Obj[];
  chatSet(reply: Obj): Record<string, boolean>;
  stateOf(status: Obj | undefined, asks: boolean): { kind: string; label: string };
  treeRows(root: Obj, chats: Record<string, boolean>, folded: Record<string, boolean>, status: Record<string, Obj>, asks: Record<string, boolean>): Obj[];
  askers(inq: Obj[]): Record<string, boolean>;
  waitingRows(inq: Obj[], names: Record<string, string>, now: number): Obj[];
  sortedListing(files: Obj[]): Obj[];
  fileRows(lists: Record<string, Obj[]>, open: Record<string, boolean>, path: string, depth: number): Obj[];
  withFiles(rows: Obj[], files: { on: boolean; lists: Record<string, Obj[]>; open: Record<string, boolean> }): Obj[];
  filterRows(rows: Obj[], q: string): Obj[];
  visibleRows(tree: Obj, chats: Record<string, boolean>, folded: Record<string, boolean>, status: Record<string, Obj>, asks: Record<string, boolean>, files: { on: boolean; lists: Record<string, Obj[]>; open: Record<string, boolean> }, q: string): Obj[];
  agentCount(tree: Obj | null): number;
  titleOf(root: Obj | null): string;
  wantedDirs(on: boolean, open: Record<string, boolean>): string[];
  affectedDirs(wanted: string[], evPath: string): string[];
  cleanState(s: unknown): { folded: Record<string, boolean>; open: Record<string, boolean>; filesOpen: boolean };
  droppedOutside(x: number, y: number, w: number, h: number): boolean;
  foldLabel(name: string, folded: boolean): string;
  IGNORED: string[];
}
const ctx = vm.createContext({});
vm.runInContext(`${src.slice(from, to)}\nthis.api = { kernelStrip, chatSet, stateOf, treeRows, askers, waitingRows, sortedListing, fileRows, withFiles, filterRows, visibleRows, agentCount, titleOf, wantedDirs, affectedDirs, cleanState, droppedOutside, foldLabel, IGNORED };`, ctx);
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
  assert.deepEqual(rows.map((r) => r["kind"]), ["dir", "dir", "dir", "file", "file"]);
});

const lists = { ".": [{ name: "src", path: "src", type: "dir" }, { name: "a.md", path: "a.md", type: "file" }, { name: "node_modules", path: "node_modules", type: "dir" }], src: [{ name: "main.rs", path: "src/main.rs", type: "file" }] };

test("the files nest inside the tree, under the files agent's row, only while the switch is on", () => {
  const rows = plain(raw.treeRows(tree, {}, {}, {}, {}));
  assert.deepEqual(plain(raw.withFiles(rows, { on: false, lists, open: {} })).map((r) => r["id"]), rows.map((r) => r["id"]), "switch off: the agent tree alone");
  const on = plain(raw.withFiles(rows, { on: true, lists, open: { src: true } }));
  assert.deepEqual(on.map((r) => [r["id"], r["depth"]]), [["kernel_state", 0], ["user", 1], ["files", 2], ["src", 3], ["src/main.rs", 4], ["a.md", 3], ["coder", 2], ["chat1", 1]]);
  const waiting = plain(raw.withFiles(rows, { on: true, lists: {}, open: {} }));
  assert.deepEqual(waiting.find((r) => r["kind"] === "note")!["depth"], 3, "a note while the folder is read");
  const kids = { ...tree, children: [{ ...(tree.children as Obj[])[0], children: [{ id: "files", display_name: "files", handler_module: "file_bridge.tools", children: [{ id: "f1", display_name: "f1", handler_module: "file_bridge.tools", children: [] }] }] }] };
  const f2 = plain(raw.visibleRows(kids, {}, { files: true }, {}, {}, { on: true, lists, open: {} }, ""));
  assert.equal(f2.some((r) => r["kind"] === "dir"), false, "folded files agent: no file rows");
});

test("the filter keeps a match and what holds it; a typed filter ignores the folds", () => {
  const rows = plain(raw.visibleRows(tree, {}, {}, {}, {}, { on: true, lists, open: { src: true } }, "main"));
  assert.deepEqual(rows.map((r) => r["id"]), ["kernel_state", "user", "files", "src", "src/main.rs"]);
  assert.deepEqual(plain(raw.visibleRows(tree, {}, { user: true }, {}, {}, { on: false, lists, open: {} }, "coder")).map((r) => r["id"]), ["kernel_state", "user", "coder"]);
  assert.equal(plain(raw.visibleRows(tree, {}, {}, {}, {}, { on: false, lists, open: {} }, "  ")).length, 5, "blank: all");
});

test("the title block: the kernel's name and its agent count", () => {
  assert.equal(raw.titleOf({ id: "kernel_state", display_name: "demo" }), "demo");
  assert.equal(raw.titleOf(null), "this kernel");
  assert.equal(raw.agentCount(tree), 4);
  assert.equal(raw.agentCount(null), 0);
  assert.equal(raw.foldLabel("Coder", false), "Fold Coder");
  assert.equal(raw.foldLabel("Coder", true), "Unfold Coder");
});

test("watches: the project folder and each open folder while the switch is on; a change reads the folder and its parent again", () => {
  assert.deepEqual(plain(raw.wantedDirs(false, { src: true })), []);
  assert.deepEqual(plain(raw.wantedDirs(true, { src: true, docs: false, "src/kern": true })), [".", "src", "src/kern"]);
  const w = [".", "src", "src/kern"];
  assert.deepEqual(plain(raw.affectedDirs(w, "src/kern/a.rs")), ["src/kern"]);
  assert.deepEqual(plain(raw.affectedDirs(w, "src/new.rs")), ["src"]);
  assert.deepEqual(plain(raw.affectedDirs(w, "src")), [".", "src"]);
  assert.deepEqual(plain(raw.affectedDirs(w, "top.md")), ["."]);
  assert.deepEqual(plain(raw.affectedDirs(w, "")), ["."]);
  assert.deepEqual(plain(raw.affectedDirs(w, "other/x")), []);
});

test("what a reload keeps is only ids and paths set true; anything else is dropped", () => {
  assert.deepEqual(plain(raw.cleanState({ folded: { a: true, b: false, c: "x" }, open: { "src/kern": true }, filesOpen: true, evil: 1 })), { folded: { a: true }, open: { "src/kern": true }, filesOpen: true });
  assert.deepEqual(plain(raw.cleanState(null)), { folded: {}, open: {}, filesOpen: false });
  assert.deepEqual(plain(raw.cleanState({ filesOpen: "yes" })), { folded: {}, open: {}, filesOpen: false });
});

test("a drag that ends outside the sidebar's frame is a drop on the canvas", () => {
  assert.equal(raw.droppedOutside(100, 100, 360, 800), false);
  assert.equal(raw.droppedOutside(400, 100, 360, 800), true);
  assert.equal(raw.droppedOutside(-3, 100, 360, 800), true);
  assert.equal(raw.droppedOutside(100, 900, 360, 800), true);
});
