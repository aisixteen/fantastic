import { test } from "node:test";
import assert from "node:assert/strict";
import { WatchLedger } from "../src/bundles/files/watch_ledger.ts";

test("two owners on one path: one watch on the first acquire, one unwatch on the last release", () => {
  const l = new WatchLedger();
  assert.equal(l.acquire("src", "tree"), "watch");
  assert.equal(l.acquire("src", "prev"), null);
  assert.equal(l.release("src", "tree"), null, "the preview still holds it");
  assert.equal(l.release("src", "prev"), "unwatch");
  assert.deepEqual(l.held(), []);
});

test("a release by one who never held is nothing; a double acquire is one hold", () => {
  const l = new WatchLedger();
  assert.equal(l.release("x", "a"), null);
  assert.equal(l.acquire("x", "a"), "watch");
  assert.equal(l.acquire("x", "a"), null);
  assert.equal(l.release("x", "a"), "unwatch");
});

test("releaseAll gives back exactly the paths no one else holds", () => {
  const l = new WatchLedger();
  l.acquire("", "tree");
  l.acquire("src", "tree");
  l.acquire("src", "prev");
  l.acquire("src/one.txt", "prev");
  assert.deepEqual(l.releaseAll("tree").sort(), [""]);
  assert.deepEqual(l.held(), ["src", "src/one.txt"]);
  assert.deepEqual(l.releaseAll("prev").sort(), ["src", "src/one.txt"]);
  assert.deepEqual(l.held(), []);
});
