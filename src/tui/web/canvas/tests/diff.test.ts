import { test } from "node:test";
import assert from "node:assert/strict";
import { MAX_LINES, lineDiff } from "../src/bundles/preview/diff.ts";

test("lineDiff: same, changed, added and removed lines", () => {
  assert.deepEqual(lineDiff("a\nb\nc", "a\nb\nc"), [
    { op: "same", text: "a" },
    { op: "same", text: "b" },
    { op: "same", text: "c" },
  ]);
  assert.deepEqual(lineDiff("a\nb\nc", "a\nB\nc"), [
    { op: "same", text: "a" },
    { op: "del", text: "b" },
    { op: "add", text: "B" },
    { op: "same", text: "c" },
  ]);
  assert.deepEqual(lineDiff("a\nc", "a\nb\nc"), [
    { op: "same", text: "a" },
    { op: "add", text: "b" },
    { op: "same", text: "c" },
  ]);
  assert.deepEqual(lineDiff("one two", "disk wins"), [
    { op: "del", text: "one two" },
    { op: "add", text: "disk wins" },
  ]);
  assert.deepEqual(lineDiff("", "x"), [{ op: "add", text: "x" }]);
  assert.deepEqual(lineDiff("x", ""), [{ op: "del", text: "x" }]);
});

test("lineDiff: replaying the ops over the old text gives the new one", () => {
  const a = ["p", "q", "r", "s", "t", "u"].join("\n");
  const b = ["p", "r", "x", "s", "u", "v"].join("\n");
  const d = lineDiff(a, b);
  assert.ok(Array.isArray(d));
  assert.equal((d as { op: string; text: string }[]).filter((l) => l.op !== "del").map((l) => l.text).join("\n"), b);
  assert.equal((d as { op: string; text: string }[]).filter((l) => l.op !== "add").map((l) => l.text).join("\n"), a);
});

test("lineDiff: over the cap it says so", () => {
  const big = Array.from({ length: MAX_LINES + 1 }, (_, i) => `l${i}`).join("\n");
  assert.deepEqual(lineDiff(big, "x"), { tooLarge: true });
  assert.deepEqual(lineDiff("x", big), { tooLarge: true });
});
