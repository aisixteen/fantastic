// THE BATCH SEND (#29) — array-only, `[Item] -> [Outcome]`.
//
// The in-process invariants: input order regardless of completion order,
// concurrency (wall ≈ max, not sum), timeouts that the kernel never defaults, a
// handler throw settling as `failed` instead of killing its siblings, and
// `pid === null` ⟺ the item never reached the task table.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Payload } from "../src/kernel/json.ts";

function kernelWith(handlers: { [id: string]: (p: Payload) => unknown }): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  for (const [id, fn] of Object.entries(handlers)) {
    k.registerBundle(`${id}.tools`, async (_id, payload) => {
      return (await fn(payload)) as never;
    });
    k.register(new Agent({ id, parentId: "kernel_state", handlerModule: `${id}.tools` }));
  }
  return k;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

test("the returned array follows INPUT order, not completion order", async () => {
  const k = kernelWith({
    slow: async (p) => { await sleep(40); return { who: "slow", n: p["n"] }; },
    fast: async (p) => ({ who: "fast", n: p["n"] }),
  });
  const outs = await k.send([
    { target: "slow", payload: { type: "go", n: 1 } },
    { target: "fast", payload: { type: "go", n: 2 } },
  ]);
  assert.equal(outs.length, 2);
  assert.equal((outs[0]!.result as Payload)["who"], "slow");
  assert.equal((outs[1]!.result as Payload)["who"], "fast");
});

test("a batch dispatches CONCURRENTLY — wall time is the max, not the sum", async () => {
  const k = kernelWith({
    a: async () => { await sleep(60); return { ok: true }; },
    b: async () => { await sleep(60); return { ok: true }; },
    c: async () => { await sleep(60); return { ok: true }; },
  });
  const t0 = Date.now();
  const outs = await k.send([
    { target: "a", payload: { type: "go" } },
    { target: "b", payload: { type: "go" } },
    { target: "c", payload: { type: "go" } },
  ]);
  const wall = Date.now() - t0;
  assert.ok(outs.every((o) => o.status === "completed"));
  assert.ok(wall < 150, `serial dispatch: ${wall}ms for 3x60ms`);
});

test("a handler that THROWS settles failed and never kills its siblings", async () => {
  const k = kernelWith({
    boom: () => { throw new Error("handler exploded"); },
    fine: () => ({ ok: true }),
  });
  const outs = await k.send([
    { target: "boom", payload: { type: "go" } },
    { target: "fine", payload: { type: "go" } },
  ]);
  assert.equal(outs[0]!.status, "failed");
  assert.equal((outs[0]!.result as Payload)["raised"], true);
  assert.match(String((outs[0]!.result as Payload)["error"]), /exploded/);
  assert.equal(outs[1]!.status, "completed", "one bad handler killed the batch");
});

test("a refusal never reaches the task table, so it carries no pid", async () => {
  const k = kernelWith({ real: () => ({ ok: true }) });
  const outs = await k.send([
    { target: "", payload: { type: "go" } },
    { target: "real", payload: null as unknown as Payload },
    { target: "real", payload: { type: "go" }, timeout: -1 },
    { target: "real", payload: { type: "go" } },
  ]);
  for (const bad of outs.slice(0, 3)) {
    assert.equal(bad.status, "failed");
    assert.equal(bad.pid, null);
    assert.ok(typeof (bad.result as Payload)["reason"] === "string");
  }
  assert.equal(outs[0]!.result && (outs[0]!.result as Payload)["reason"], "missing_arg");
  assert.equal((outs[2]!.result as Payload)["reason"], "bad_arg");
  assert.equal(outs[3]!.status, "completed");
  assert.ok(typeof outs[3]!.pid === "number", "a dispatched item carries its int message id");
});

test("a timeout settles timed_out — and the kernel never defaults one", async () => {
  const k = kernelWith({ slow: async () => { await sleep(300); return { ok: true }; } });
  const [bounded] = await k.send([
    { target: "slow", payload: { type: "go" }, timeout: 0.05 },
  ]);
  assert.equal(bounded!.status, "timed_out");
  assert.equal((bounded!.result as Payload)["reason"], "timeout");

  // No timeout named ⇒ it waits. (Short handler, so the test does not.)
  const k2 = kernelWith({ quick: async () => { await sleep(80); return { ok: true }; } });
  const [unbounded] = await k2.send([{ target: "quick", payload: { type: "go" } }]);
  assert.equal(unbounded!.status, "completed");
});

test("the whole batch is visible in `ps` before any of it settles", async () => {
  const k = kernelWith({
    a: async () => { await sleep(60); return { ok: true }; },
    b: async () => { await sleep(60); return { ok: true }; },
  });
  const inflight = k.send([
    { target: "a", payload: { type: "go" } },
    { target: "b", payload: { type: "go" } },
  ]);
  await sleep(10);
  const ps = ((await k.send([{ target: "kernel", payload: { type: "ps" } }]))[0]!.result) as Payload;
  // `ps` is itself a dispatch and holds a row while it answers, so count the
  // two the batch launched rather than the whole table.
  const tasks = (ps["tasks"] as { agent: string }[]).filter((t) =>
    t.agent === "a" || t.agent === "b",
  );
  assert.equal(tasks.length, 2, "launch was not synchronous for the whole batch");
  await inflight;
});
