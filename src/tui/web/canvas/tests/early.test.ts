import { test } from "node:test";
import assert from "node:assert/strict";
import { earlyPlan } from "../src/bundles/terminal_view/early.ts";

const out = (job_id: number, data: string) => ({ job_id, data });

test("a held frame the ring replay covered is skipped: not drawn, not acked", () => {
  const a = out(7, "aaaa");
  const b = out(7, "bb");
  assert.deepEqual(earlyPlan([a, b], 2, 7), [{ op: "skip" }, { op: "skip" }], "both are in the replay");
});

test("frames after the replay are drawn (and acked once, by the draw)", () => {
  const a = out(7, "aaaa");
  const c = out(7, "ccc");
  assert.deepEqual(earlyPlan([a, c], 1, 7), [{ op: "skip" }, { op: "draw", frame: c }], "only the first is covered");
});

test("a covered index of another job is drawn, so the draw drops it", () => {
  const other = out(9, "xx");
  assert.deepEqual(earlyPlan([other], 1, 7), [{ op: "draw", frame: other }]);
});

test("job_done ends the terminal wherever it sits, covered or not", () => {
  const done = { type: "job_done", job_id: 7 };
  const a = out(7, "aa");
  assert.deepEqual(earlyPlan([done, a], 2, 7), [{ op: "end", frame: done }, { op: "skip" }]);
});

test("no double count across batches: a later drain with nothing covered draws every frame", () => {
  const a = out(7, "aaaa");
  const b = out(7, "bbbb");
  assert.deepEqual(earlyPlan([a], 1, 7), [{ op: "skip" }], "the replay's frame is skipped once");
  assert.deepEqual(earlyPlan([b], 0, 7), [{ op: "draw", frame: b }], "the next batch is drawn and acked once, not skipped again");
});

test("no frames, no steps", () => {
  assert.deepEqual(earlyPlan([], 0, undefined), []);
});
