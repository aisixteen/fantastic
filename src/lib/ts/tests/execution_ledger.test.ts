// THE EXECUTION LEDGER (#34/#35/#36) — ensure-semantics for sends.
//
// The property under test is the one that makes a double-click safe: reusing an
// id DECLARES "same intent", so the work runs ONCE and every later caller gets
// the recorded outcome. Measured against the real python kernel 2026-08-27; the
// ts kernel must agree, because the same ids cross the bridge between them.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Payload } from "../src/kernel/json.ts";
import { kin } from "./kit.ts";
import { sendAs } from "./kit.ts";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

function kernelWith(handlers: { [id: string]: (p: Payload) => unknown }): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  for (const [id, fn] of Object.entries(handlers)) {
    k.registerBundle(`${id}.tools`, async (_id, payload) => (await fn(payload)) as never);
    k.register(new Agent({ id, parentId: "kernel_state", handlerModule: `${id}.tools` }));
  }
  return k;
}

test("a NAMED execution runs once; re-sending returns the RECORDED outcome", async () => {
  let runs = 0;
  const k = kernelWith({ till: () => { runs += 1; return { charged: runs }; } });
  const first = (await k.send([{ target: "till", payload: { type: "buy" }, id: "order-1" }]))[0]!;
  const again = (await k.send([{ target: "till", payload: { type: "buy" }, id: "order-1" }]))[0]!;
  assert.equal(runs, 1, "the handler ran twice for one intent");
  assert.deepEqual(again.result, first.result);
  // The NAME is the caller's; the execution's id is the kernel's counter.
  assert.equal(typeof first.pid, "number");
  assert.equal(again.pid, first.pid);
  assert.equal(again.name, "order-1");
});

test("a re-send while still IN FLIGHT joins — it does not start a second run", async () => {
  let runs = 0;
  const k = kernelWith({
    slow: async () => { runs += 1; await sleep(60); return { n: runs }; },
  });
  const a = k.send([{ target: "slow", payload: { type: "go" }, id: "job" }]);
  await sleep(10);
  const b = k.send([{ target: "slow", payload: { type: "go" }, id: "job" }]);
  const [ra, rb] = await Promise.all([a, b]);
  assert.equal(runs, 1, "the in-flight execution was duplicated");
  assert.deepEqual(ra[0]!.result, rb[0]!.result);
});

test("an id is BOUND to its first target — aiming it elsewhere is refused", async () => {
  const k = kernelWith({ alpha: () => ({ ok: "a" }), beta: () => ({ ok: "b" }) });
  await k.send([{ target: "alpha", payload: { type: "go" }, id: "1" }]);
  const [out] = await k.send([{ target: "beta", payload: { type: "go" }, id: "1" }]);
  assert.equal(out!.status, "failed");
  assert.equal((out!.result as Payload)["reason"], "bad_arg");
  assert.match(String((out!.result as Payload)["error"]), /bound to its first execution/);
});

test("an INT id REFERENCES a minted execution and never creates one", async () => {
  let runs = 0;
  const k = kernelWith({ a: () => ({ ok: ++runs }) });
  const [made] = await k.send([{ target: "a", payload: { type: "go" } }]);
  const pid = made!.pid as number;
  assert.ok(Number.isInteger(pid) && pid > 0, `unexpected pid: ${String(pid)}`);

  // Re-fetch by the minted id: the recorded outcome, no new run.
  const [refetched] = await k.send([{ target: "a", payload: { type: "go" }, id: pid }]);
  assert.deepEqual(refetched!.result, made!.result);
  assert.equal(runs, 1);

  // An INVENTED int id names nothing and must not create — the kernel is the
  // only allocator.
  const [bad] = await k.send([{ target: "a", payload: { type: "go" }, id: 999_999 }]);
  assert.equal(bad!.status, "failed");
  assert.equal(bad!.pid, null);
  assert.equal((bad!.result as Payload)["reason"], "bad_arg");
  assert.match(String((bad!.result as Payload)["error"]), /never created/);
});

test("`retries` re-dispatches a failure under the SAME id; only the final outcome settles", async () => {
  let attempt = 0;
  const k = kernelWith({
    flaky: () => {
      attempt += 1;
      if (attempt < 3) throw new Error(`attempt ${attempt} failed`);
      return { ok: true, attempt };
    },
  });
  const [out] = await k.send([
    { target: "flaky", payload: { type: "go" }, id: "j", retries: 3 },
  ]);
  assert.equal(out!.status, "completed");
  assert.equal(out!.attempts, 3);
  assert.equal((out!.result as Payload)["attempt"], 3);

  // A joiner sees ONLY the final outcome — never an attempt retried away.
  const [joined] = await k.send([{ target: "flaky", payload: { type: "go" }, id: "j" }]);
  assert.equal(joined!.status, "completed");
});

test("a spent retry budget PARKS the failure — the dead-letter queue is a query", async () => {
  const k = kernelWith({ dead: () => { throw new Error("always fails"); } });
  const [out] = await k.send([
    { target: "dead", payload: { type: "go" }, id: "dl", retries: 1 },
  ]);
  assert.equal(out!.status, "failed");
  assert.equal(out!.attempts, 2, "the retry budget was not spent");

  const led = ((await k.send([{ target: "kernel", payload: { type: "ledger" } }]))[0]!.result) as Payload;
  const rows = led["executions"] as { name?: string; status: string; attempts: number }[];
  const parked = rows.filter((r) => r.status !== "completed");
  assert.ok(parked.some((r) => r.name === "dl" && r.attempts === 2), JSON.stringify(rows));

  // Parked, NOT silently retried: re-sending the id returns the recorded failure.
  const [again] = await k.send([{ target: "dead", payload: { type: "go" }, id: "dl" }]);
  assert.equal(again!.status, "failed");
});

test("the ledger verb is ORIGIN-SCOPED — an agent sees only what it originated", async () => {
  const k = kernelWith({
    worker: () => ({ ok: true }),
    // `nosy` reads the ledger as ITSELF.
    nosy: () => ({ ok: true }),
  });
  await k.send([{ target: "worker", payload: { type: "private" }, id: "operator-job" }]);

  // As the operator: everything is visible.
  const all = ((await k.send([{ target: "kernel", payload: { type: "ledger" } }]))[0]!.result) as Payload;
  const allRows = all["executions"] as { name?: string }[];
  assert.ok(allRows.some((r) => r.name === "operator-job"));

  // As an AGENT: only its own rows. Unscoped, this verb would hand any agent
  // the result BODIES of everything every other agent ran. (`ledger` is
  // two-sided, so both ends name it.)
  k.updateMeta("nosy", { can_call: { kernel_state: ["ledger"] } });
  k.updateMeta("kernel_state", { callable_by: { nosy: ["ledger"] } });
  const mine = ((await sendAs(k, "nosy", [{ target: "kernel", payload: { type: "ledger" } }]))[0]!.result) as Payload;
  const mineRows = mine["executions"] as { name?: string }[];
  assert.ok(
    !mineRows.some((r) => r.name === "operator-job"),
    "an agent read another origin's executions",
  );
});

test("the retention ring evicts settled rows, unnamed before named, in-flight never", async () => {
  const k = kernelWith({ a: () => ({ ok: true }) });
  k.ledger.cap = 3;
  await k.send([{ target: "a", payload: { type: "go" }, id: "keep-me" }]);
  for (let i = 0; i < 8; i++) {
    await k.send([{ target: "a", payload: { type: "go" } }]); // unnamed
  }
  assert.ok(k.ledger.size() <= 3, `ring not enforced: ${k.ledger.size()}`);
  assert.ok(
    k.ledger.resolveName("keep-me") !== undefined,
    "a NAMED row was evicted before unnamed ones — a named row is a dedup contract",
  );
});

test("kernel verbs are root-only: a non-root agent refuses with not_here", async () => {
  const k = kernelWith({ a: () => ({ ok: true }) });
  const [out] = await k.send([{ target: "a", payload: { type: "ps" } }]);
  // python `_dispatch`'s NOT_HERE, verbatim
  assert.deepEqual(out!.result, {
    error: "ps is a kernel verb — a does not answer it",
    reason: "not_here",
    hint: "address the kernel root ('kernel'); name the agent you mean in `parent_id` / `id`",
    verb: "ps",
  });
});

// python `_verb_cancel`: absent, "", a bool, or anything not an int / str (a
// float, a list, a dict) is a MISSING task id.
test("cancel: a task_id that is not an int or a string is missing_arg", async () => {
  const k = kernelWith({});
  for (const bad of [null, "", true, 3.5, [1], { id: 1 }]) {
    const [out] = await k.send([{ target: "kernel", payload: { type: "cancel", task_id: bad } }]);
    assert.deepEqual(out!.result, { error: "cancel: task_id required", reason: "missing_arg", arg: "task_id" }, JSON.stringify(bad));
  }
  const [stale] = await k.send([{ target: "kernel", payload: { type: "cancel", task_id: 999 } }]);
  assert.deepEqual(stale!.result, { cancelled: false, killed: [] });
});

// python `task_subtree`: POST-ORDER — children die before the parent awaiting them.
test("cancel kills the subtree leaves first", async () => {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  const hold = new Promise<never>(() => undefined);
  k.registerBundle("chain.tools", async (id, _p, view) => {
    if (id === "c3") return hold;
    const next = id === "c1" ? "c2" : "c3";
    return (await view.send([{ target: next, payload: { type: "go" } }]))[0]!.result;
  });
  k.register(new Agent({ id: "c1", parentId: "kernel_state", handlerModule: "chain.tools", meta: kin() }));
  k.register(new Agent({ id: "c2", parentId: "c1", handlerModule: "chain.tools", meta: kin() }));
  k.register(new Agent({ id: "c3", parentId: "c2", handlerModule: "chain.tools", meta: kin() }));
  const run = k.send([{ target: "c1", payload: { type: "go" } }]);
  while (k.tasks.size() < 3) await sleep(2);
  const [top, mid, leaf] = k.tasks.ids();
  const [out] = await k.send([{ target: "kernel", payload: { type: "cancel", task_id: top } }]);
  assert.deepEqual(out!.result, { cancelled: true, killed: [leaf, mid, top] });
  assert.equal((await run)[0]!.status, "cancelled");
});
