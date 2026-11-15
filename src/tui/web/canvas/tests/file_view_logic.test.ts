import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import { lineCount, linesText, metaOf, trail } from "../../../../lib/rust/crates/bundles/fantastic-file/src/file_head.mjs";

// THE FILE VIEW (`fantastic-file/src/file_view.html`): its pure block — what a read reply means,
// the pages added up, a folder's entries, the agent lookup of the breadcrumb — run in a bare vm
// context with the shared header logic (`file_head.mjs`, also the canvas previewer's) spliced
// in the way `views.rs` splices it. No DOM.

const dir = "../../../../lib/rust/crates/bundles/fantastic-file/src/";
const html = readFileSync(fileURLToPath(new URL(`${dir}file_view.html`, import.meta.url)), "utf8");
const head = readFileSync(fileURLToPath(new URL(`${dir}file_head.mjs`, import.meta.url)), "utf8").replaceAll("export ", "");
const from = html.indexOf("// pure\n");
const to = html.indexOf("// end pure");
assert.ok(html.includes("/*{{head}}*/"), "the page names where the header logic goes");
assert.ok(from >= 0 && to > from, "the view marks its pure block");

type Obj = Record<string, unknown>;
interface Pure {
  readState(r: unknown): Obj;
  nextState(held: Obj[], offset: number, r: unknown): Obj;
  sortedEntries(files: unknown): Obj[];
  agentsOf(tree: unknown): Record<string, Obj>;
  lookupFor(path: string, agents: Record<string, Obj>): { paths(): string[]; agentAt(p: string): Obj | undefined; folderOf(id: string): string | undefined };
  trail(path: string, ix: unknown): string[];
  metaOf(o: Obj): string;
}
const ctx = vm.createContext({});
vm.runInContext(`${head}\n${html.slice(from, to)}\nthis.api = { readState, nextState, sortedEntries, agentsOf, lookupFor, trail, metaOf };`, ctx);
const raw = (ctx as unknown as { api: Pure }).api;
const plain = <T>(x: T): T => JSON.parse(JSON.stringify(x)) as T;

test("the spliced header logic is the module's: same answers", () => {
  assert.equal(raw.metaOf({ name: "api.yaml", lines: 32 }), metaOf({ name: "api.yaml", lines: 32 }));
  assert.equal(metaOf({ name: "api.yaml", lines: 32 }), "32 lines · yaml");
  assert.equal(metaOf({ name: "a.yaml", size: 612, text: "a\nb\n" }), "2 lines · 612 bytes · yaml");
  assert.equal(linesText(1), "1 line");
  assert.equal(lineCount("a\nb\n"), 2);
});

test("readState: text, empty, not text, gone, error", () => {
  assert.deepEqual(plain(raw.readState({ lines: [{ n: 1, text: "a" }], total: 1, eof: true })), { kind: "text", lines: [{ n: 1, text: "a" }], total: 1 });
  assert.deepEqual(plain(raw.readState({ lines: [], total: 0, eof: true })), { kind: "empty" });
  assert.deepEqual(plain(raw.readState({ error: "x", reason: "bad_arg", arg: "path" })), { kind: "binary" });
  assert.deepEqual(plain(raw.readState({ error: "no such file", reason: "not_found" })), { kind: "gone" });
  assert.deepEqual(plain(raw.readState({ error: "boom", reason: "transport_error" })), { kind: "error", error: "boom" });
  assert.deepEqual(plain(raw.readState({ error: "bad_arg elsewhere", reason: "bad_arg", arg: "limit" })), { kind: "error", error: "bad_arg elsewhere" });
  assert.deepEqual(plain(raw.readState(null)), { kind: "error", error: "no answer" });
});

test("nextState: a first page replaces, a later page adds by line number, eof once all lines are held", () => {
  const page = (from: number, n: number, total: number): Obj => ({ lines: Array.from({ length: n }, (_, i) => ({ n: from + i + 1, text: `l${from + i + 1}` })), total });
  const first = plain(raw.nextState([], 0, page(0, 2, 3)));
  assert.equal(first["eof"], false);
  assert.deepEqual((first["lines"] as Obj[]).map((l) => l["n"]), [1, 2]);
  const held = first["lines"] as Obj[];
  const second = plain(raw.nextState(held, 2, page(2, 1, 3)));
  assert.equal(second["eof"], true);
  assert.deepEqual((second["lines"] as Obj[]).map((l) => l["n"]), [1, 2, 3]);
  const again = plain(raw.nextState(second["lines"] as Obj[], 0, page(0, 3, 3)));
  assert.deepEqual((again["lines"] as Obj[]).map((l) => l["n"]), [1, 2, 3], "a re-read from the top replaces what was held");
  assert.equal(plain(raw.nextState(held, 0, { error: "x", reason: "not_found" }))["kind"], "gone");
});

test("sortedEntries: folders first, then by name; anything unnamed is dropped", () => {
  const out = plain(raw.sortedEntries([{ name: "b.txt", type: "file" }, { name: "z", type: "dir" }, { name: "a.txt", type: "file" }, { type: "file" }, null]));
  assert.deepEqual(out.map((e) => e["name"]), ["z", "a.txt", "b.txt"]);
  assert.deepEqual(plain(raw.sortedEntries(undefined)), []);
});

test("the breadcrumb: the agent chain from the kernel tree, then the path below the agent's folder", () => {
  const tree = { id: "root", display_name: "team", children: [
    { id: "lead", display_name: "Lead", children: [
      { id: "ops", display_name: "cluster ops", children: [{ id: "k-rollout", display_name: "", children: [] }] },
    ] },
  ] };
  const agents = raw.agentsOf(tree);
  assert.equal(agents["ops"]!["parentId"], "lead");
  const at = ".fantastic/agents/lead/agents/ops/agents/k-rollout/deploy/api/api.yaml";
  assert.deepEqual(plain(raw.trail(at, raw.lookupFor(at, agents))), ["Lead", "cluster ops", "k-rollout", "deploy", "api", "api.yaml"], "the root is no step");
  assert.deepEqual(plain(raw.trail("src/a.ts", raw.lookupFor("src/a.ts", agents))), ["src", "a.ts"], "outside an agent folder: the plain path");
  assert.deepEqual(plain(raw.trail("", raw.lookupFor("", agents))), ["project"]);
  // the module's own trail, on the same lookup, agrees
  assert.deepEqual(trail(at, raw.lookupFor(at, agents) as never), ["Lead", "cluster ops", "k-rollout", "deploy", "api", "api.yaml"]);
});
