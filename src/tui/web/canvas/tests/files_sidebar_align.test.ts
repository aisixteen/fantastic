import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import { IGNORED } from "../src/bundles/files/paths.ts";
import { TreeModel } from "../src/bundles/files/tree_model.ts";

// THE SIDEBAR'S FILES AND THE CANVAS TREE CELL ARE ONE LISTING: which folders are hidden and the
// order of a folder's entries are the cell's (`paths.ts`, `tree_model.ts`); the sidebar view
// (a null-origin html page, no imports) carries its own copy of the two rules, pinned here.

const src = readFileSync(fileURLToPath(new URL("../../../src/kern/sidebar_view.html", import.meta.url)), "utf8");
const ctx = vm.createContext({});
vm.runInContext(`${src.slice(src.indexOf("// pure\n"), src.indexOf("// end pure"))}\nthis.api = { sortedListing, IGNORED };`, ctx);
const view = (ctx as unknown as { api: { sortedListing(f: unknown[]): { name: string }[]; IGNORED: string[] } }).api;

test("the sidebar hides the folders the tree cell hides", () => {
  assert.deepEqual([...view.IGNORED], [...IGNORED]);
});

test("the sidebar orders a folder as the tree cell does", () => {
  const entries = [
    { name: "b.txt", path: "b.txt", type: "file" },
    { name: "Zed", path: "Zed", type: "dir" },
    { name: "a.txt", path: "a.txt", type: "file" },
    { name: "alpha", path: "alpha", type: "dir" },
    { name: "node_modules", path: "node_modules", type: "dir" },
    { name: "Readme.md", path: "Readme.md", type: "file" },
  ];
  const m = new TreeModel();
  m.expand("");
  m.listed("", entries.map((e) => ({ ...e, type: e.type as "dir" | "file" })));
  const cell = m.rows().filter((r) => r.path !== "").map((r) => r.name);
  assert.deepEqual(view.sortedListing(entries).map((n) => n.name), cell);
});
