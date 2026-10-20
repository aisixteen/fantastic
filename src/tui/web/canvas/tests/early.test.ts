import { test } from "node:test";
import assert from "node:assert/strict";
import { earlyPlan, runEarly } from "../src/bundles/terminal_view/early.ts";
import type { Frame } from "../src/bundles/terminal_view/early.ts";

const out = (job_id: number, data: string) => ({ job_id, data });

// The view's draw acks each chunk it writes: the counter stands for the job's
// unacked count (ackChars), so `acked` is what the host would take off.
function ackCounter() {
  const counter = { acked: 0 };
  const draw = (f: Frame): void => {
    counter.acked += (f["data"] as string).length;
  };
  return { counter, draw };
}

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

test("runEarly: the replay resets the ack counter to 0, and a covered frame leaves it there", () => {
  const { counter, draw } = ackCounter();
  counter.acked = 0; // the replay's reset of the job's unacked count
  runEarly(earlyPlan([out(7, "aaaa")], 1, 7), draw, () => assert.fail("no job_done"));
  assert.equal(counter.acked, 0, "a covered frame is skipped, so nothing is acked");
});

test("runEarly: a drawn frame acks its data.length", () => {
  const { counter, draw } = ackCounter();
  runEarly(earlyPlan([out(7, "ccc")], 0, 7), draw, () => assert.fail("no job_done"));
  assert.equal(counter.acked, 3);
});

test("runEarly: covered frames are not acked, only the frames after the replay are", () => {
  const { counter, draw } = ackCounter();
  runEarly(earlyPlan([out(7, "aaaa"), out(7, "bb"), out(7, "ccc")], 2, 7), draw, () => assert.fail("no job_done"));
  assert.equal(counter.acked, 3, "only ccc arrived after the reset");
});

test("runEarly: acked chars equal the chars that arrived after the reset, across several batches", () => {
  const { counter, draw } = ackCounter();
  const batches: Array<{ frames: Frame[]; covered: number }> = [
    { frames: [out(7, "aaaa"), out(7, "bb"), out(7, "ccc")], covered: 2 },
    { frames: [out(7, "dddd")], covered: 0 },
    { frames: [out(7, "ee"), { type: "job_done", job_id: 7 }], covered: 0 },
    { frames: [out(7, "fffff"), out(7, "gg")], covered: 1 },
  ];
  let arrivedAfterReset = 0;
  let ended = 0;
  counter.acked = 0; // the replay reset the count before the first batch
  for (const { frames, covered } of batches) {
    runEarly(earlyPlan(frames, covered, 7), draw, () => ended++);
    frames.forEach((f, i) => {
      if (f["type"] !== "job_done" && i >= covered) arrivedAfterReset += (f["data"] as string).length;
    });
  }
  assert.equal(counter.acked, arrivedAfterReset, "every char that arrived after the reset is acked once, none of the covered ones");
  assert.equal(counter.acked, 3 + 4 + 2 + 2, "ccc, dddd, ee and gg; fffff is covered");
  assert.equal(ended, 1, "the job_done ends the terminal once");
});

test("runEarly: a job_done is passed to end and acks nothing", () => {
  const { counter, draw } = ackCounter();
  const done = { type: "job_done", job_id: 7 };
  const seen: Frame[] = [];
  runEarly(earlyPlan([done], 1, 7), draw, (f) => seen.push(f));
  assert.deepEqual(seen, [done]);
  assert.equal(counter.acked, 0);
});
