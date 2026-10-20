import { test } from "node:test";
import assert from "node:assert/strict";
import { TreeModel } from "../src/bundles/files/tree_model.ts";
import type { Entry, Row } from "../src/bundles/files/tree_model.ts";
import { WatchLedger } from "../src/bundles/files/watch_ledger.ts";
import { AgentIndex } from "../src/bundles/files/agent_index.ts";
import { WatchHub, bytesOf, readAll, readBytes } from "../src/bundles/files/file_bridge.ts";
import type { Change } from "../src/bundles/files/file_bridge.ts";
import { IGNORED, FANTASTIC, ignored, isFantastic, join, nameOf, under, wire } from "../src/bundles/files/paths.ts";
import { FakeBridge, settle } from "./fake_bridge.ts";

// THE TREE'S STATE, the WATCH LEDGER and the hub's books, the AGENT INDEX and the
// bridge's pure readers, each tested for the behaviour it promises (not for lines).

const dir = (path: string): Entry => ({ name: path.split("/").pop()!, path, type: "dir" });
const file = (path: string, size?: number): Entry => ({ name: path.split("/").pop()!, path, type: "file", ...(size !== undefined ? { size } : {}) });
const paths = (rows: Row[]): string[] => rows.map((r) => r.path);

/** root: docs, src (dirs), a.txt, and the hidden .git; src: deep (dir), one.ts. */
function tree(): TreeModel {
  const m = new TreeModel();
  m.expand("");
  m.listed("", [dir("src"), dir("docs"), file("a.txt", 3), dir(".git")]);
  return m;
}

// ─── the tree's state ──────────────────────────────────────────────────────

test("tree: rows are the root and the visible children; ignored folders hide until showIgnored", () => {
  const m = tree();
  assert.deepEqual(paths(m.rows()), ["", "docs", "src", "a.txt"], "dirs first, then by name; .git hidden");
  m.showIgnored = true;
  assert.ok(paths(m.rows()).includes(".git"));
  assert.ok(IGNORED.includes(".git") && ignored("node_modules") && !ignored("src"));
});

test("tree: the filter keeps matching names and the folders above them; a folder that matches by name alone stays shut", () => {
  const m = tree();
  m.expand("src");
  m.listed("src", [dir("src/deep"), file("src/one.ts")]);
  assert.deepEqual(paths(m.rows("one")), ["", "src", "src/one.ts"]);
  const byName = m.rows("SR");
  assert.deepEqual(paths(byName), ["", "src"], "src matches, but none of its children do, so it is not opened");
  assert.equal(byName[1]!.open, false);
  assert.deepEqual(paths(m.rows("zzz")), [""], "nothing matches: only the root row");
  assert.deepEqual(paths(m.rows("   ")), paths(m.rows()), "a blank filter is no filter");
});

test("tree: keys move the cursor, clamp at the ends, and say what Enter, arrows and Shift+Enter do", () => {
  const m = tree();
  m.expand("src");
  m.listed("src", [dir("src/deep"), file("src/one.ts")]);
  const rows = m.rows();
  const k = (cursor: string | null, key: string, shift = false) => m.key(rows, cursor, key, shift);

  assert.equal(k("docs", "ArrowDown").cursor, "src");
  assert.equal(k("a.txt", "ArrowDown").cursor, "a.txt", "clamped at the last row");
  assert.equal(k("docs", "ArrowUp").cursor, "", "up from docs is the root");
  assert.equal(k("", "ArrowUp").cursor, "", "clamped at the first row");
  assert.equal(k(null, "ArrowUp").cursor, "", "no cursor counts as the first row");
  assert.equal(k("docs", "Home").cursor, "");
  assert.equal(k("docs", "End").cursor, "a.txt");
  assert.equal(k("zz-not-a-row", "ArrowDown").cursor, "docs", "a cursor not on screen starts from the top");

  assert.deepEqual(k("a.txt", "ArrowRight"), { cursor: "a.txt" }, "right on a file: nothing to open");
  assert.deepEqual(k("docs", "ArrowRight"), { cursor: "docs", effect: { kind: "expand", path: "docs" } }, "right on a shut folder opens it");
  assert.equal(k("src", "ArrowRight").cursor, "src/deep", "right on an open folder descends to its first child");
  assert.deepEqual(k("docs", "ArrowLeft"), { cursor: "" }, "left on a shut folder goes up to its parent");
  assert.deepEqual(k("src", "ArrowLeft"), { cursor: "src", effect: { kind: "collapse", path: "src" } }, "left on an open folder shuts it");
  assert.deepEqual(k("src/one.ts", "ArrowLeft"), { cursor: "src" }, "left on a file goes up to its folder");
  assert.deepEqual(k("", "ArrowLeft"), { cursor: "", effect: { kind: "collapse", path: "" } }, "left on the open root shuts it");

  assert.deepEqual(k("a.txt", "Enter"), { cursor: "a.txt", effect: { kind: "select", path: "a.txt" } });
  assert.deepEqual(k("docs", "Enter"), { cursor: "docs", effect: { kind: "expand", path: "docs" } });
  assert.deepEqual(k("src", "Enter"), { cursor: "src", effect: { kind: "collapse", path: "src" } });
  assert.deepEqual(k("src/one.ts", "Enter", true), { cursor: "src/one.ts", effect: { kind: "open", path: "src/one.ts" } });
  assert.deepEqual(k("docs", "x"), { cursor: "docs" }, "any other key leaves the cursor where it is");
  assert.deepEqual(m.key([], "keep", "ArrowDown"), { cursor: "keep" }, "no rows: nothing moves");
});

test("tree: right on an open folder with no children stays put (the next row is not below it)", () => {
  const m = tree();
  m.expand("docs");
  m.listed("docs", []);
  const rows = m.rows();
  assert.deepEqual(m.key(rows, "docs", "ArrowRight"), { cursor: "docs" });
});

test("tree: expanding is a no-op for a file, an unknown path and an open folder", () => {
  const m = tree();
  assert.deepEqual(m.expand("a.txt"), [], "a file does not open");
  assert.deepEqual(m.expand("nope"), []);
  m.expand("src");
  assert.deepEqual(m.expand("src"), [], "already open");
  assert.deepEqual(m.collapse("docs"), [], "collapsing a shut folder ends nothing");
});

test("tree: a listing that comes back for a folder no longer open is dropped", () => {
  const m = tree();
  m.expand("src");
  m.collapse("src");
  assert.deepEqual(m.listed("src", [file("src/x.ts")]), []);
  assert.equal(m.nodes.get("src")!.children, undefined);
  assert.deepEqual(m.listed("ghost", []), [], "an unknown folder");
});

test("tree: a re-listing updates sizes (a missing size keeps the old one), and drops children that are gone", () => {
  const m = tree();
  m.listed("", [file("a.txt", 42), dir("src"), dir("docs")]);
  assert.equal(m.nodes.get("a.txt")!.size, 42, "the new size");
  m.listed("", [file("a.txt"), dir("src"), dir("docs")]);
  assert.equal(m.nodes.get("a.txt")!.size, 42, "no size in the listing: the size is kept");
  m.expand("src");
  m.listed("src", [file("src/one.ts")]);
  const out = m.listed("", [dir("docs")]);
  assert.deepEqual(out, [{ kind: "unwatch", path: "src" }], "the folder that went away stops being watched");
  assert.equal(m.nodes.has("src"), false);
  assert.equal(m.nodes.has("src/one.ts"), false, "and what was below it is forgotten");
  assert.equal(m.nodes.has("a.txt"), false);
});

test("tree: a folder that becomes a file (and back) ends its watch and forgets what was below", () => {
  const m = tree();
  m.expand("src");
  m.listed("src", [dir("src/deep")]);
  m.expand("src/deep");
  const out = m.listed("", [file("src"), dir("docs"), file("a.txt", 3)]);
  assert.deepEqual(out.map((e) => e.kind + " " + e.path).sort(), ["unwatch src", "unwatch src/deep"]);
  assert.equal(m.nodes.get("src")!.type, "file");
  assert.equal(m.isOpen("src"), false);
  assert.equal(m.nodes.has("src/deep"), false);
});

test("tree: a change to a folder re-lists its parent; to a file it does nothing; a deleted open folder unwatches itself too", () => {
  const m = tree();
  assert.deepEqual(m.changed({ path: "a.txt", kind: "modified" }), [], "a modified file: the previews follow it");
  assert.deepEqual(m.changed({ path: "src", kind: "modified" }), [{ kind: "list", path: "" }], "a folder changed: its parent (the open root) is listed again");
  assert.deepEqual(m.changed({ path: "./docs/", kind: "added" }), [], "the root is still listing from the change before: it is marked stale, not listed twice");
  assert.deepEqual(m.listed("", [dir("src"), dir("docs"), file("a.txt", 3), dir(".git")]), [{ kind: "list", path: "" }], "and it is listed once more when it returns");
  assert.deepEqual(m.changed({ path: ".", kind: "modified" }), [], "the root itself has no parent to list");
  const d = tree();
  d.expand("src");
  d.listed("src", [dir("src/deep")]);
  d.expand("src/deep");
  const fx = d.changed({ path: "src", kind: "deleted" });
  assert.deepEqual(fx, [{ kind: "unwatch", path: "src" }, { kind: "unwatch", path: "src/deep" }, { kind: "list", path: "" }]);
  assert.equal(d.isOpen("src"), false);
  assert.equal(d.isOpen("src/deep"), false);
});

test("tree: a change while the parent is listing makes one re-list when it returns, then no more", () => {
  const m = new TreeModel();
  m.expand("");
  assert.equal(m.nodes.get("")!.status, "loading");
  assert.deepEqual(m.changed({ path: "x.txt", kind: "added" }), [], "the listing is in flight: it is marked stale");
  assert.equal(m.nodes.get("")!.stale, true);
  assert.deepEqual(m.listed("", [file("x.txt")]), [{ kind: "list", path: "" }]);
  assert.equal(m.nodes.get("")!.status, "loading");
  assert.deepEqual(m.listed("", [file("x.txt")]), [], "the second listing settles it");
  assert.equal(m.nodes.get("")!.status, "loaded");
});

test("tree: a refused listing says so on its node, and a later good listing clears it", () => {
  const m = tree();
  m.expand("src");
  assert.deepEqual(m.listFailed("src", "no access"), []);
  assert.equal(m.nodes.get("src")!.status, "error");
  assert.equal(m.nodes.get("src")!.error, "no access");
  m.listed("src", [file("src/one.ts")]);
  assert.equal(m.nodes.get("src")!.error, undefined);
  assert.equal(m.nodes.get("src")!.status, "loaded");
  assert.deepEqual(m.listFailed("ghost", "x"), [], "an unknown node has nothing to say it on");
});

test("tree: unmount ends every watch and forgets every openness", () => {
  const m = tree();
  m.expand("src");
  m.listed("src", [dir("src/deep")]);
  m.expand("src/deep");
  const fx = m.unmount();
  assert.deepEqual(fx.map((e) => e.path).sort(), ["", "src", "src/deep"]);
  assert.ok(fx.every((e) => e.kind === "unwatch"));
  assert.equal(m.expanded.size, 0);
});

// ─── the watch ledger ──────────────────────────────────────────────────────

test("ledger: owners and held paths are listed (held sorted); a release by one who is not a holder keeps the path", () => {
  const l = new WatchLedger();
  l.acquire("b", "x");
  l.acquire("a", "x");
  l.acquire("a", "y");
  assert.deepEqual(l.held(), ["a", "b"]);
  assert.deepEqual(l.ownersOf("a").sort(), ["x", "y"]);
  assert.deepEqual(l.ownersOf("missing"), []);
  assert.equal(l.release("a", "stranger"), null, "not a holder: nothing");
  assert.deepEqual(l.ownersOf("a").sort(), ["x", "y"], "the holders are untouched");
  assert.equal(l.release("a", "x"), null);
  assert.equal(l.release("a", "y"), "unwatch", "the last holder lets go");
  assert.deepEqual(l.releaseAll("nobody"), [], "a releaseAll for one who holds nothing ends nothing");
});

// ─── the agent index ───────────────────────────────────────────────────────

test("agent index: folder and id both ways; a re-set id points at the new folder; drop only forgets what is still its own", () => {
  const ix = new AgentIndex();
  const info = (id: string) => ({ id, handler: "x.ts", displayName: "", hasParent: false, hasMemory: false });
  ix.set(".fantastic/a", info("one"));
  ix.set(".fantastic/b", info("one"));
  assert.equal(ix.folderOf("one"), ".fantastic/b", "the id now names the later folder");
  assert.equal(ix.agentAt(".fantastic/a")?.id, "one");
  assert.equal(ix.agentAt("nowhere"), undefined);
  ix.drop(".fantastic/a");
  assert.equal(ix.agentAt(".fantastic/a"), undefined);
  assert.equal(ix.folderOf("one"), ".fantastic/b", "dropping the old folder does not forget the id that moved");
  ix.drop(".fantastic");
  assert.deepEqual(ix.paths(), []);
  assert.deepEqual(ix.ids(), []);
  ix.set(".fantastic/c", info("two"));
  ix.clear();
  assert.deepEqual(ix.paths(), []);
  assert.deepEqual(ix.ids(), []);
});

// ─── the paths ─────────────────────────────────────────────────────────────

test("paths: wire, join, nameOf, under, fantastic", () => {
  assert.equal(wire(""), ".");
  assert.equal(wire("a"), "a");
  assert.equal(join("", "a"), "a");
  assert.equal(join("a", "b"), "a/b");
  assert.equal(nameOf("a/b/c.txt"), "c.txt");
  assert.equal(nameOf("top"), "top");
  assert.equal(under("a/b", "a"), true);
  assert.equal(under("a", "a"), true);
  assert.equal(under("ab", "a"), false, "a prefix is a folder, not a string prefix");
  assert.equal(under("anything", ""), true);
  assert.equal(FANTASTIC, ".fantastic");
  assert.equal(isFantastic("src"), false);
});

// ─── the hub's books ───────────────────────────────────────────────────────

test("hub: held() lists what this page holds; a drop by one who does not hold is nothing on the bridge", async () => {
  const b = new FakeBridge();
  b.put("src/one.txt", "1");
  const hub = new WatchHub(b, b.subscribe);
  await hub.hold("src", "a", () => {});
  await hub.hold("", "a", () => {});
  assert.deepEqual(hub.held(), ["", "src"].sort());
  assert.ok(b.watched.has("."), "the root is watched as '.'");
  hub.drop("src", "stranger");
  assert.ok(b.watched.has("src"), "the stranger's drop did not end the hold");
  assert.equal(b.calls.filter((c) => c["type"] === "unwatch_path").length, 0);
  hub.drop("/src/", "a");
  assert.deepEqual(hub.held(), [""], "a drop names its path as the hub does (normalised)");
});

test("hub: dropAll ends only the holds nobody else has, and the subscription ends with the last hold", async () => {
  const b = new FakeBridge();
  b.put("src/one.txt", "1");
  const hub = new WatchHub(b, b.subscribe);
  await hub.hold("src", "tree", () => {});
  await hub.hold("src", "prev", () => {});
  await hub.hold("src/one.txt", "prev", () => {});
  hub.dropAll("prev");
  await settle();
  assert.deepEqual(hub.held(), ["src"], "src is still the tree's");
  assert.ok(b.watched.has("src"));
  assert.ok(!b.watched.has("src/one.txt"), "the file only the preview held is unwatched");
  assert.equal(b.subscribed, true);
  hub.dropAll("tree");
  await settle();
  assert.equal(b.subscribed, false, "no holds: the events source is let go");
  hub.dropAll("never-held");
  assert.deepEqual(hub.held(), []);
});

test("hub: a refusal on unwatch is said on the console and does not stop the books", async () => {
  const b = new FakeBridge();
  const refusing = { call: async (p: Record<string, unknown>) => (p["type"] === "unwatch_path" ? { error: "unwatch refused", reason: "denied" } : b.call(p)) };
  const hub = new WatchHub(refusing, b.subscribe);
  const seen: unknown[] = [];
  const was = console.error;
  console.error = (...a: unknown[]) => seen.push(a.join(" "));
  try {
    await hub.hold("src", "tree", () => {});
    hub.drop("src", "tree");
    await settle();
  } finally {
    console.error = was;
  }
  assert.deepEqual(hub.held(), []);
  assert.ok(seen.some((s) => /unwatch src: unwatch refused/.test(String(s))));
});

test("hub: an event with no path is ignored; a missing kind is a modification; a handler that throws is said and does not stop the others", async () => {
  const b = new FakeBridge();
  const hub = new WatchHub(b, b.subscribe);
  const got: Change[] = [];
  await hub.hold("src", "boom", () => {
    throw new Error("handler broke");
  });
  await hub.hold("src", "ok", (c) => got.push(c));
  const deliver = (ev: Record<string, unknown>): void => (b as unknown as { listener: (e: Record<string, unknown>) => void }).listener(ev);
  const was = console.error;
  const logged: unknown[][] = [];
  console.error = (...a: unknown[]) => logged.push(a);
  try {
    deliver({ type: "changed", kind: "added" }); // no path: ignored
    deliver({ type: "changed", path: "src/x.txt" }); // no kind: a modification
    await settle();
  } finally {
    console.error = was;
  }
  assert.deepEqual(got, [{ path: "src/x.txt", kind: "modified" }]);
  assert.equal(logged.length, 1, "the throwing handler is said once");
  assert.equal(logged[0]![0], "[files] a change handler threw:");
});

test("hub: the events source is subscribed on the first hold and let go with the last", async () => {
  const b = new FakeBridge();
  const hub = new WatchHub(b, b.subscribe);
  assert.equal(b.subscribed, false);
  await hub.hold("src", "a", () => {});
  assert.equal(b.subscribed, true);
  hub.drop("src", "a");
  assert.equal(b.subscribed, false);
  await hub.hold("src", "a", () => {});
  assert.equal(b.subscribed, true, "a later hold subscribes again");
});

// ─── the bridge's pure readers ─────────────────────────────────────────────

test("bytesOf: bytes, an ArrayBuffer, a view and text are bytes; anything else is not", () => {
  const u = new Uint8Array([1, 2]);
  assert.equal(bytesOf(u), u);
  assert.deepEqual([...bytesOf(new Uint8Array([9, 8]).buffer)!], [9, 8]);
  assert.deepEqual([...bytesOf(new DataView(new Uint8Array([5, 6, 7]).buffer, 1, 2))!], [6, 7], "a view keeps its offset and length");
  assert.deepEqual([...bytesOf("é")!], [0xc3, 0xa9]);
  assert.equal(bytesOf(42), null);
  assert.equal(bytesOf(undefined), null);
});

test("readBytes: a refusal is returned; a chunk that is not bytes is said; a file that grows past the cap while read is refused", async () => {
  const b = new FakeBridge();
  const none = await readBytes(b, "nope", 10);
  assert.ok(!none.ok && none.refusal["reason"] === "not_found");

  b.put("t.txt", "abc");
  const was = b.call.bind(b);
  b.call = async (p) => (p["type"] === "read_stream" ? { size: 3, bytes: 12, eof: true } : was(p));
  const bad = await readBytes(b, "t.txt", 10);
  assert.ok(!bad.ok && bad.refusal["reason"] === "bad_reply");

  b.call = async (p) => (p["type"] === "read_stream" ? { size: 11, bytes: new Uint8Array(0), eof: true } : was(p));
  const grew = await readBytes(b, "t.txt", 10);
  assert.ok(!grew.ok && grew.refusal["reason"] === "too_large");
});

test("readBytes: a reply that ends with an empty chunk stops the read even without eof", async () => {
  const b = new FakeBridge();
  b.call = async () => ({ size: 5, bytes: new Uint8Array(0), next_offset: 0 });
  const r = await readBytes(b, "x", 10);
  assert.ok(r.ok && r.bytes.length === 0);
});

test("readAll: an empty page ends the file; a read refused in the middle is the refusal", async () => {
  const b = new FakeBridge();
  b.put("p.txt", "a\nb\n");
  const was = b.call.bind(b);
  b.call = async (p) => (p["type"] === "read" ? { version: "v", lines: [], eof: false } : was(p));
  const empty = await readAll(b, "p.txt");
  assert.ok(empty.ok && empty.text === "", "a page with no lines is the end: nothing read");

  let n = 0;
  b.call = async (p) => {
    if (p["type"] === "read" && ++n === 2) return { error: "read: io error", reason: "io" };
    return was(p);
  };
  b.put("big.txt", Array.from({ length: 2500 }, (_, i) => `l${i}`).join("\n"));
  const mid = await readAll(b, "big.txt");
  assert.ok(!mid.ok && mid.refusal["reason"] === "io");
});

test("readAll: the trailing newline check refused is the refusal", async () => {
  const b = new FakeBridge();
  b.put("n.txt", "a\n");
  const was = b.call.bind(b);
  b.call = async (p) => (p["type"] === "read_stream" ? { error: "stream refused", reason: "denied" } : was(p));
  const r = await readAll(b, "n.txt");
  assert.ok(!r.ok && r.refusal["reason"] === "denied");
});

test("tree: children sort folders first, then by name, whatever order the listing came in", () => {
  const m = new TreeModel();
  m.expand("");
  m.listed("", [dir("src"), file("b.txt"), dir("docs")]);
  assert.deepEqual(paths(m.rows()), ["", "docs", "src", "b.txt"]);
});

test("tree: left on a row whose folder is not among the rows stays where it is", () => {
  const m = tree();
  const rows: Row[] = [{ path: "src/one.ts", depth: 2, type: "file", open: false, name: "one.ts" }];
  assert.deepEqual(m.key(rows, "src/one.ts", "ArrowLeft"), { cursor: "src/one.ts" });
});

test("file bridge: a file whose first byte cannot be read is not said to end in a newline; a tail refusal is the refusal", async () => {
  const b = new FakeBridge();
  b.put("t.txt", "a\n");
  const was = b.call.bind(b);
  b.call = async (p) => (p["type"] === "read_stream" ? { size: 0, bytes: new Uint8Array(0), eof: true } : was(p));
  const zero = await readAll(b, "t.txt");
  assert.ok(zero.ok && zero.text === "a", "a head that says the file is empty: no newline is added");

  let streams = 0;
  b.call = async (p) => {
    if (p["type"] === "read_stream" && ++streams === 2) return { error: "tail: denied", reason: "denied" };
    return was(p);
  };
  const tail = await readAll(b, "t.txt");
  assert.ok(!tail.ok && tail.refusal["reason"] === "denied");
});

test("file bridge: a page with no version, no lines list, or a line with no text still reads (as empty text, no version)", async () => {
  const b = new FakeBridge();
  const was = b.call.bind(b);
  b.call = async (p) => (p["type"] === "read" ? { lines: "not a list", eof: true } : was(p));
  b.put("odd.txt", "x");
  const odd = await readAll(b, "odd.txt");
  assert.ok(odd.ok && odd.text === "" && odd.version === "");
  b.call = async (p) => (p["type"] === "read" ? { version: "v1", lines: [{ n: 1 }], eof: true } : was(p));
  const notext = await readAll(b, "odd.txt");
  assert.ok(notext.ok && notext.text === "" && notext.version === "v1", "a line with no text is an empty line");
});

test("the page's hub: one per kernel; its bridge is the kernel's files host, and a hold it cannot arm is refused and holds nothing", async () => {
  const { sharedHub } = await import("../src/bundles/files/file_bridge.ts");
  const { Kernel } = await import("../src/lib.ts");
  const k = new Kernel();
  const h = sharedHub(k);
  assert.equal(sharedHub(k), h, "the same kernel, the same hub");
  assert.notEqual(sharedHub(new Kernel()), h, "another kernel has its own");
  await assert.rejects(h.hold("src", "tree", () => {}));
  assert.deepEqual(h.held(), [], "nothing is held after the refusal");
});
