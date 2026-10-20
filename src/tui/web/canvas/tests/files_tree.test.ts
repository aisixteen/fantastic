import { test } from "node:test";
import assert from "node:assert/strict";
import { TreeModel } from "../src/bundles/files/tree_model.ts";
import type { Effect, Entry } from "../src/bundles/files/tree_model.ts";
import { ancestors, inRoot, isFantastic, norm, parentOf } from "../src/bundles/files/paths.ts";

const dir = (path: string): Entry => ({ name: path.split("/").pop()!, path, type: "dir" });
const file = (path: string, size = 1): Entry => ({ name: path.split("/").pop()!, path, type: "file", size });

function opened(): TreeModel {
  const m = new TreeModel();
  m.expand("");
  m.listed("", [dir("src"), dir("docs"), file("a.txt"), dir(".git"), dir("node_modules"), dir(".fantastic")]);
  return m;
}

test("paths: norm, parent, ancestors, inRoot, isFantastic", () => {
  assert.equal(norm("./src//a/"), "src/a");
  assert.equal(norm("."), "");
  assert.equal(parentOf("a/b/c"), "a/b");
  assert.equal(parentOf("a"), "");
  assert.deepEqual(ancestors("a/b/c"), ["", "a", "a/b"]);
  assert.deepEqual(ancestors(""), []);
  assert.equal(inRoot("a/b"), true);
  assert.equal(inRoot("/etc/passwd"), false);
  assert.equal(inRoot("a/../../b"), false);
  assert.equal(inRoot("C:\\x"), false);
  assert.equal(isFantastic(".fantastic"), true);
  assert.equal(isFantastic(".fantastic/agents"), true);
  assert.equal(isFantastic(".fantastics"), false);
});

test("expand lists and watches; a second expand is a no-op", () => {
  const m = new TreeModel();
  assert.deepEqual(m.expand(""), [{ kind: "list", path: "" }, { kind: "watch", path: "" }]);
  assert.deepEqual(m.expand(""), []);
  assert.deepEqual(m.expand("nope"), [], "an unknown path");
});

test("collapse unwatches the folder and every open folder below it, and forgets their openness", () => {
  const m = opened();
  m.expand("src");
  m.listed("src", [dir("src/a"), file("src/x.ts")]);
  m.expand("src/a");
  m.listed("src/a", []);
  const fx = m.collapse("src");
  assert.deepEqual(fx.map((e) => e.path).sort(), ["src", "src/a"]);
  assert.ok(fx.every((e) => e.kind === "unwatch"));
  assert.equal(m.isOpen("src/a"), false);
  assert.deepEqual([...m.expanded], [""]);
});

test("changed re-lists only the parent; a modified file changes nothing", () => {
  const m = opened();
  m.expand("src");
  m.listed("src", [file("src/one.txt")]);
  assert.deepEqual(m.changed({ path: "src/new.txt", kind: "added" }), [{ kind: "list", path: "src" }]);
  m.listed("src", [file("src/one.txt"), file("src/new.txt")]);
  assert.deepEqual(m.changed({ path: "src/one.txt", kind: "modified" }), []);
  assert.deepEqual(m.changed({ path: "docs/x", kind: "added" }), [], "docs is not open");
  assert.deepEqual(m.changed({ path: "top.txt", kind: "added" }), [{ kind: "list", path: "" }]);
});

test("a changed during a list leads to one stale re-list", () => {
  const m = opened();
  m.expand("src");
  assert.deepEqual(m.changed({ path: "src/a", kind: "added" }), [], "loading: marked stale, no second list");
  assert.deepEqual(m.changed({ path: "src/b", kind: "added" }), []);
  assert.deepEqual(m.listed("src", [file("src/a")]), [{ kind: "list", path: "src" }]);
  assert.deepEqual(m.listed("src", [file("src/a"), file("src/b")]), []);
  assert.equal(m.nodes.get("src")!.status, "loaded");
});

test("a deleted open folder unwatches its subtree and leaves the tree", () => {
  const m = opened();
  m.expand("src");
  m.listed("src", [dir("src/deep")]);
  m.expand("src/deep");
  m.listed("src/deep", [file("src/deep/f")]);
  const fx = m.changed({ path: "src/deep", kind: "deleted" });
  assert.deepEqual(fx.filter((e) => e.kind === "unwatch"), [{ kind: "unwatch", path: "src/deep" }]);
  assert.ok(fx.some((e) => e.kind === "list" && e.path === "src"));
  m.listed("src", []);
  assert.equal(m.nodes.has("src/deep"), false);
  assert.equal(m.nodes.has("src/deep/f"), false);
  assert.deepEqual([...m.expanded].sort(), ["", "src"]);
});

test("a listing that drops an open folder unwatches it", () => {
  const m = opened();
  m.expand("src");
  m.listed("src", []);
  const fx = m.listed("", [file("a.txt")]);
  assert.deepEqual(fx, [{ kind: "unwatch", path: "src" }]);
  assert.equal(m.isOpen("src"), false);
});

test("a failed list keeps the refusal on the node", () => {
  const m = opened();
  m.expand("src");
  m.listFailed("src", "no such folder");
  assert.equal(m.nodes.get("src")!.status, "error");
  assert.equal(m.nodes.get("src")!.error, "no such folder");
});

test("ignored folders hide until showIgnored; folders sort first, then by name", () => {
  const m = opened();
  assert.deepEqual(m.rows().map((r) => r.name), ["", ".fantastic", "docs", "src", "a.txt"]);
  m.showIgnored = true;
  assert.deepEqual(m.rows().map((r) => r.name), ["", ".fantastic", ".git", "docs", "node_modules", "src", "a.txt"]);
});

test("the filter keeps matches and their folders", () => {
  const m = opened();
  m.expand("src");
  m.listed("src", [file("src/alpha.ts"), file("src/beta.ts")]);
  m.collapse("src");
  const rows = m.rows("alpha");
  assert.deepEqual(rows.map((r) => r.path), ["", "src", "src/alpha.ts"]);
  assert.equal(rows[1]!.open, true);
});

test("keys: arrows, right expands then descends, left collapses then ascends, Enter, Home/End", () => {
  const m = opened();
  const r0 = m.rows();
  assert.equal(m.key(r0, "", "ArrowDown").cursor, ".fantastic");
  assert.equal(m.key(r0, "src", "Home").cursor, "");
  assert.equal(m.key(r0, "", "End").cursor, "a.txt");
  assert.deepEqual(m.key(r0, "src", "ArrowRight"), { cursor: "src", effect: { kind: "expand", path: "src" } });
  m.expand("src");
  m.listed("src", [file("src/x")]);
  const r1 = m.rows();
  assert.equal(m.key(r1, "src", "ArrowRight").cursor, "src/x");
  assert.equal(m.key(r1, "src/x", "ArrowLeft").cursor, "src");
  assert.deepEqual(m.key(r1, "src", "ArrowLeft"), { cursor: "src", effect: { kind: "collapse", path: "src" } });
  assert.deepEqual(m.key(r1, "src/x", "Enter"), { cursor: "src/x", effect: { kind: "select", path: "src/x" } });
  assert.deepEqual(m.key(r1, "src", "Enter"), { cursor: "src", effect: { kind: "collapse", path: "src" } });
  assert.deepEqual(m.key(r1, "src/x", "Enter", true), { cursor: "src/x", effect: { kind: "open", path: "src/x" } });
});

test("property: 200 random operations keep watched == expanded, and unmount leaves none", () => {
  let seed = 7;
  const rnd = (n: number): number => {
    seed = (seed * 1103515245 + 12345) & 0x7fffffff;
    return seed % n;
  };
  const m = new TreeModel();
  const watched = new Set<string>();
  const apply = (fx: Effect[]): void => {
    for (const e of fx) {
      if (e.kind === "watch") watched.add(e.path);
      else if (e.kind === "unwatch") assert.ok(watched.delete(e.path), `unwatch of ${e.path} that was not watched`);
    }
  };
  const names = ["a", "b", "c"];
  const dirs = (): string[] => [...m.nodes.values()].filter((n) => n.type === "dir").map((n) => n.path);
  const listing = (p: string): Entry[] =>
    names.filter(() => rnd(3) > 0).map((n) => (rnd(2) === 0 ? dir(p === "" ? n : `${p}/${n}`) : file(p === "" ? n : `${p}/${n}`)));
  apply(m.expand(""));
  apply(m.listed("", listing("")));
  for (let i = 0; i < 200; i++) {
    const ds = dirs();
    const p = ds[rnd(ds.length)]!;
    switch (rnd(5)) {
      case 0:
        apply(m.expand(p));
        break;
      case 1:
        apply(m.collapse(p));
        break;
      case 2:
        if (m.isOpen(p)) apply(m.listed(p, listing(p)));
        break;
      case 3:
        apply(m.changed({ path: p === "" ? "x" : `${p}/x`, kind: ["added", "deleted", "modified"][rnd(3)]! }));
        break;
      default:
        apply(m.changed({ path: p, kind: "deleted" }));
    }
    assert.deepEqual([...watched].sort(), [...m.expanded].sort(), `after op ${i}`);
  }
  apply(m.unmount());
  assert.equal(watched.size, 0);
});
