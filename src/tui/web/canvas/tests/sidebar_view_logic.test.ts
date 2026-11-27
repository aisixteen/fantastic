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
  pinRows(root: Obj, chats: Record<string, boolean>, folded: Record<string, boolean>, status: Record<string, string>, asks: Record<string, boolean>, files: Obj, project: string, opts?: Obj): Obj[];
  projectName(p: unknown): string;
  chatSet(reply: Obj): Record<string, boolean>;
  stateOf(phase: string | undefined, asks: boolean): { kind: string; label: string; glyph: string };
  outlineOf(kind: string): string;
  treeRows(root: Obj, chats: Record<string, boolean>, folded: Record<string, boolean>, status: Record<string, string>, asks: Record<string, boolean>, files?: Obj, opts?: Obj): Obj[];
  filterRows(rows: Obj[], q: string): Obj[];
  keptState(st: Obj): Obj;
  restoredState(saved: unknown): Obj;
  wantedWatches(filesOn: boolean, open: Record<string, boolean>): string[];
  changedDirs(ev: Obj, lists: Record<string, Obj[]>): string[];
  heardOf(ev: unknown): string;
  withPhase(phases: Record<string, string>, agent: string, phase: string): Record<string, string>;
  askers(inq: Obj[]): Record<string, boolean>;
  waitingRows(inq: Obj[], names: Record<string, string>, now: number): Obj[];
  sortedListing(files: Obj[]): Obj[];
  fileRows(lists: Record<string, Obj[]>, open: Record<string, boolean>, path: string, depth: number): Obj[];
}
const ctx = vm.createContext({});
vm.runInContext(`${src.slice(from, to)}\nthis.api = { askAges, toolRows, pinRows, projectName, kernelStrip, chatSet, stateOf, outlineOf, treeRows, filterRows, keptState, restoredState, wantedWatches, changedDirs, heardOf, withPhase, askers, waitingRows, sortedListing, fileRows };`, ctx);
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
  assert.equal(plain(raw.kernelStrip({ id: "k", display_name: "launch-week", guid: "ab" }, []))[0]!["mark"], "lw", "a name of several words: its initials");
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

test("with the tools group, the tree holds the AI agents only, each under its nearest AI ancestor", () => {
  const rows = plain(raw.treeRows(tree, { chat1: true, coder: true }, {}, {}, {}, undefined, { tools: true }));
  assert.deepEqual(rows.map((r) => [r["id"], r["depth"]]), [["coder", 0], ["chat1", 0]], "no root row; user and files are not AI agents");
  const t2 = { id: "r", display_name: "r", children: [
    { id: "u", display_name: "u", children: [{ id: "lead", display_name: "Lead", children: [
      { id: "tasks", display_name: "tasks", handler_module: "task.tools", children: [] },
      { id: "w", display_name: "w", children: [{ id: "c", display_name: "c", children: [] }] }] }] },
    { id: "files", display_name: "files", children: [{ id: "v", display_name: "v", children: [] }] }] };
  const chats = { lead: true, c: true };
  const tr = plain(raw.treeRows(t2, chats, {}, {}, {}, undefined, { tools: true, pinned: true }));
  assert.deepEqual(tr.map((r) => [r["id"], r["depth"], r["kids"]]), [["lead", 0, 1], ["c", 1, 0]], "c sits under Lead, past the agent between them");
  const folded = plain(raw.treeRows(t2, chats, { lead: true }, {}, { c: true }, undefined, { tools: true }));
  assert.deepEqual(folded.map((r) => [r["id"], r["below"], r["asksBelow"]]), [["lead", 1, 1]]);
  assert.deepEqual(plain(raw.toolRows(t2, chats)).map((r) => [r["id"], r["tag"], r["depth"]]), [["u", "tool", 0], ["tasks", "tool", 0], ["w", "tool", 0]],
    "every other agent, in tree order, tagged tool; not the root, not the files agent or what is under it");
  assert.deepEqual(plain(raw.treeRows(t2, chats, {}, {}, {})).map((r) => r["id"]), ["r", "u", "lead", "tasks", "w", "c", "files", "v"], "without the option the tree keeps them all");
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

test("state: asks > the last phase > idle, each phase the design's badge", () => {
  const S = (phase?: string): string | undefined => phase;
  // asks you: ? whatever the phase
  for (const p of ["thinking", "done", "failed", "paused", undefined]) assert.deepEqual(plain(raw.stateOf(S(p), true)), { kind: "ask", label: "asks you", glyph: "?" }, String(p));
  // Working: the spinner, no glyph, the phase's word
  const working: [string, string][] = [["thinking", "thinking"], ["streaming", "writing"], ["retrying", "retrying"]];
  for (const [p, label] of working) assert.deepEqual(plain(raw.stateOf(S(p), false)), { kind: "active", label, glyph: "" }, p);
  // a call's end and a pause keep their badge
  assert.deepEqual(plain(raw.stateOf(S("stalled"), false)), { kind: "stalled", label: "stalled", glyph: "!" });
  assert.deepEqual(plain(raw.stateOf(S("failed"), false)), { kind: "failed", label: "failed", glyph: "\u00d7" });
  assert.deepEqual(plain(raw.stateOf(S("done"), false)), { kind: "done", label: "done", glyph: "\u2713" });
  assert.deepEqual(plain(raw.stateOf(S("paused"), false)), { kind: "paused", label: "paused", glyph: "II" });
  // at rest: no badge; the dead tool_calling is no phase any more
  for (const p of ["idle", "tool_calling", "", "toString", undefined]) assert.deepEqual(plain(raw.stateOf(S(p), false)), { kind: "idle", label: "idle", glyph: "" }, String(p));
  const rows = plain(raw.treeRows(tree, { chat1: true }, {}, { chat1: "streaming" }, { chat1: true }));
  assert.equal((rows.find((r) => r["id"] === "chat1")!["state"] as Obj)["kind"], "ask");
});

test("outline: every state's ring round the tile, Working's a ray, Stalled's dashed, none at rest", () => {
  const of = (phase: string | undefined, asks = false): string => raw.outlineOf(raw.stateOf(phase, asks).kind);
  for (const p of ["thinking", "streaming", "retrying"]) assert.equal(of(p), "ol ray", p);
  assert.equal(of("done", true), "ol", "asks you");
  assert.equal(of("stalled"), "ol dash");
  for (const p of ["failed", "paused", "done"]) assert.equal(of(p), "ol", p);
  for (const p of ["idle", "", "toString", undefined]) assert.equal(of(p), "", String(p));
  for (const k of ["idle", "toString", "constructor", ""]) assert.equal(raw.outlineOf(k), "", k);
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

test("what the view keeps across a reload: folded branches, open folders, the files and full switches", () => {
  const kept = plain(raw.keptState({ folded: { user: true }, open: { src: true }, filesOn: true, lists: { ".": [] }, selected: "x" }));
  assert.deepEqual(kept, { folded: { user: true }, open: { src: true }, filesOn: true, toolsOpen: false, full: false });
  assert.deepEqual(plain(raw.restoredState(kept)), kept);
  assert.deepEqual(plain(raw.restoredState({ folded: { a: true, b: false, c: "yes" }, open: 3, filesOn: "true" })), { folded: { a: true }, open: {}, filesOn: false, toolsOpen: false, full: false }, "only what is exactly true comes back");
  assert.deepEqual(plain(raw.restoredState(null)), { folded: {}, open: {}, filesOn: false, toolsOpen: false, full: false });
  assert.equal(plain(raw.restoredState({ toolsOpen: true }))["toolsOpen"], true);
  assert.equal(plain(raw.restoredState({ full: true }))["full"], true, "the full switch comes back");
  assert.equal(plain(raw.keptState({ folded: {}, open: {}, full: true }))["full"], true);
});

test("a watch is held on the root and every open folder while files show, none otherwise", () => {
  assert.deepEqual(plain(raw.wantedWatches(true, { src: true, docs: false })), [".", "src"]);
  assert.deepEqual(plain(raw.wantedWatches(false, { src: true })), []);
});

test("an entry that came or went reads again the folder that holds it, when listed", () => {
  const held = { ".": [], src: [] };
  assert.deepEqual(plain(raw.changedDirs({ type: "files_changed", path: "src/a.rs", change: "added" }, held)), ["src"]);
  assert.deepEqual(plain(raw.changedDirs({ type: "files_changed", path: "src", change: "removed" }, held)), ["."]);
  assert.deepEqual(plain(raw.changedDirs({ type: "files_changed", path: "other/x", change: "added" }, held)), [], "a folder the view does not list is not read");
  assert.deepEqual(plain(raw.changedDirs({ type: "watching" }, held)), []);
});

test("the view acts only on what its sidebar agent publishes; a copy of a call is nothing", () => {
  assert.equal(raw.heardOf({ type: "tree_changed" }), "tree");
  assert.equal(raw.heardOf({ type: "status_changed", agent: "c1", phase: "thinking" }), "status");
  assert.equal(raw.heardOf({ type: "inquiries_changed", open: 1 }), "inquiries");
  assert.equal(raw.heardOf({ type: "files_changed", path: "a", change: "added" }), "files");
  assert.equal(raw.heardOf({ type: "files_changed", path: "a", change: "modified" }), "", "a write in place is not published, and is nothing");
  for (const copy of [{ type: "reflect" }, { type: "status" }, { type: "status_changed" }, { type: "watch" }, { type: "agent_created", id: "x" }, { type: "changed", path: "a", kind: "added" }])
    assert.equal(raw.heardOf(copy), "", JSON.stringify(copy));
  assert.equal(raw.heardOf({ type: "tree_changed", error: "denied" }), "");
});

test("a chat's phase follows its status_changed; its last phase stays, idle is at rest", () => {
  assert.deepEqual(plain(raw.withPhase({}, "c1", "thinking")), { c1: "thinking" });
  assert.deepEqual(plain(raw.withPhase({ c1: "thinking", c2: "retrying" }, "c1", "streaming")), { c1: "streaming", c2: "retrying" });
  for (const end of ["done", "failed", "stalled", "paused"])
    assert.deepEqual(plain(raw.withPhase({ c1: "thinking", c2: "done" }, "c1", end)), { c1: end, c2: "done" }, end);
  assert.deepEqual(plain(raw.withPhase({ c1: "paused", c2: "done" }, "c1", "idle")), { c2: "done" });
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
  const deep = { id: "r", children: [{ id: "user", children: [root.children[0]] }] };
  assert.deepEqual(plain(raw.pinRows(deep, {}, {}, {}, {}, { on: false, lists: {}, open: {} }, "")).map((r) => r["name"]), ["files", "view1"], "the files agent is found below the root too");
  assert.deepEqual(plain(raw.treeRows(root, { c: true }, {}, {}, {}, undefined, { pinned: true })).map((r) => r["id"]), ["r", "c"], "pinned: the files agent leaves the tree");
  assert.deepEqual(plain(raw.treeRows(root, { c: true }, {}, {}, {}, undefined, {})).map((r) => r["id"]), ["r", "files", "view1", "c"]);
});

test("the project name is the last part of the files root", () => {
  assert.deepEqual(["/a/b/launch-week", "/a/b/launch-week/", "x", "", null].map((p) => raw.projectName(p)), ["launch-week", "launch-week", "x", "", ""]);
});
