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

// ── FAILURE IS A STATUS: `errored` ───────────────────────────────────────
// python `kernel/_kernel.py` outcome vocabulary: a handler that RAN and said NO
// (a dict carrying a truthy `error`) settles `errored` — not `completed`, not
// retried, not a dead letter. Python truthiness decides what "truthy" means.

test("a handler that answers with a truthy `error` settles errored, reply verbatim", async () => {
  const k = kernelWith({
    vault: async (p) => (p["type"] === "open" ? { ok: true } : { error: "unknown verb", reason: "unhandled" }),
  });
  const [no, yes] = await k.send([
    { target: "vault", payload: { type: "privileged" } },
    { target: "vault", payload: { type: "open" } },
  ]);
  assert.equal(no!.status, "errored");
  assert.deepEqual(no!.result, { error: "unknown verb", reason: "unhandled" });
  assert.equal(typeof no!.pid, "number", "it reached the task table: the handler ran");
  assert.equal(yes!.status, "completed");
});

test("a falsy `error` (null, \"\", 0, false, [], {}) is not a failure", async () => {
  for (const e of [null, "", 0, false, [], {}]) {
    const k = kernelWith({ t: async () => ({ error: e, value: 1 }) });
    const [o] = await k.send([{ target: "t", payload: { type: "go" } }]);
    assert.equal(o!.status, "completed", JSON.stringify(e));
  }
  for (const e of ["no", 1, true, ["x"], { why: "x" }]) {
    const k = kernelWith({ t: async () => ({ error: e }) });
    const [o] = await k.send([{ target: "t", payload: { type: "go" } }]);
    assert.equal(o!.status, "errored", JSON.stringify(e));
  }
});

test("errored is not retried and is not a dead letter", async () => {
  let runs = 0;
  const k = kernelWith({ t: async () => { runs++; return { error: "no such path" }; } });
  const [o] = await k.send([{ target: "t", id: "job-1", retries: 3, payload: { type: "read" } }]);
  assert.equal(o!.status, "errored");
  assert.equal(o!.attempts, 1);
  assert.equal(runs, 1, "a semantic no is not transient — it is not retried");
  const led = (await k.send([{ target: "kernel", payload: { type: "ledger" } }]))[0]!.result as Payload;
  const row = (led["executions"] as Payload[]).find((r) => r["name"] === "job-1")!;
  assert.equal(row["status"], "errored");
  // ack_deadletter retires DEAD letters only
  const ack = (await k.send([{ target: "kernel", payload: { type: "ack_deadletter", id: "job-1" } }]))[0]!;
  assert.notEqual((ack.result as Payload)["acked"], true, JSON.stringify(ack.result));
});

// ── `ps` rows carry parent and started_at (python's task table) ─────────

test("ps rows are {id, agent, parent, verb, started_at}; a nested send parents to its caller", async () => {
  let release!: () => void;
  const gate = new Promise<void>((r) => { release = r; });
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  k.registerBundle("inner.tools", async () => { await gate; return { ok: true }; });
  k.registerBundle("outer.tools", async (_id, _p, kk) => (await kk.send([{ target: "inner", payload: { type: "work" } }]))[0]!.result);
  k.register(new Agent({ id: "inner", parentId: "kernel_state", handlerModule: "inner.tools", meta: { callable_by: { outer: "*" } } }));
  k.register(new Agent({ id: "outer", parentId: "kernel_state", handlerModule: "outer.tools", meta: { can_call: { inner: "*" } } }));
  const before = Date.now() / 1000;
  const running = k.send([{ target: "outer", payload: { type: "go" } }]);
  await sleep(10);
  const ps = (await k.send([{ target: "kernel", payload: { type: "ps" } }]))[0]!.result as Payload;
  const rows = ps["tasks"] as Payload[];
  const outer = rows.find((r) => r["agent"] === "outer")!;
  const inner = rows.find((r) => r["agent"] === "inner")!;
  for (const r of [outer, inner]) {
    assert.deepEqual(Object.keys(r).sort(), ["agent", "id", "parent", "started_at", "verb"]);
    assert.ok((r["started_at"] as number) >= before - 1 && (r["started_at"] as number) <= Date.now() / 1000 + 1);
  }
  assert.equal(outer["parent"], null, "an operator send is a root");
  assert.equal(inner["parent"], outer["id"], "`parent` chains the rows into the send tree");
  assert.equal(inner["verb"], "work");
  release();
  await running;
});

// ── python parity (divergence fix 2026-10-01) ─────────────────────────────

test("a handler that throws UNDER a timeout settles failed at once, not timed_out", async () => {
  // python: only a task still pending at the deadline times out; a raise is a
  // raise. The text names the error's class first (`<Type>: <msg>`).
  const k = kernelWith({ boom: () => { throw new TypeError("bad input"); } });
  const t0 = Date.now();
  const [out] = await k.send([{ target: "boom", payload: { type: "go" }, timeout: 5 }]);
  assert.ok(Date.now() - t0 < 1000, "waited out the deadline");
  assert.equal(out!.status, "failed");
  assert.deepEqual(out!.result, { error: "TypeError: bad input", raised: true });
  // nothing was cancelled on its behalf: no task row survives, no kill recorded
  assert.equal(k.tasks.size(), 0);
});

test("the `kernel` reflect is a pure identity read: no pid, no row, no ledger entry", async () => {
  const k = kernelWith({});
  const rowsBefore = k.store.rows.size;
  const [out] = await k.send([{ target: "kernel", payload: { type: "reflect", tree: "none" } }]);
  assert.equal(out!.pid, null);
  assert.equal(out!.status, "completed");
  assert.equal("attempts" in out!, false);
  assert.equal((out!.result as Payload)["id"], "kernel_state");
  assert.equal(k.store.rows.size, rowsBefore, "a call/settle row was written");
  assert.equal(k.ledger.size(), 0);
  // NAMED, it is ledgered (dedup + join) under a minted id, but still has no task
  const [named] = await k.send([{ target: "kernel", payload: { type: "reflect", tree: "none" }, id: "who" }]);
  assert.equal(typeof named!.pid, "number");
  assert.equal(named!.name, "who");
  assert.equal(named!.attempts, 1);
  const kinds = [...k.store.rows.values()].map((r) => r.kind);
  assert.deepEqual(kinds, ["call", "settle"]);
  const [again] = await k.send([{ target: "kernel", payload: { type: "reflect" }, id: "who" }]);
  assert.equal(again!.pid, named!.pid, "a re-send did not return the recorded outcome");
  // by its own id it is reflect of the root like any send: a dispatch with a pid
  const [byId] = await k.send([{ target: "kernel_state", payload: { type: "reflect", tree: "none" } }]);
  assert.equal(typeof byId!.pid, "number");
});

test("outcomes that ran nothing carry no `attempts` (python `outcome()`)", async () => {
  const k = kernelWith({ slow: async () => { await sleep(80); return { ok: true }; } });
  const [ref] = await k.send([{ target: "nobody", payload: { type: "go" } }]);
  assert.deepEqual(Object.keys(ref!), ["pid", "status", "result"]);
  const owner = k.send([{ target: "slow", payload: { type: "go" }, id: "job" }]);
  await sleep(5);
  const [joined] = await k.send([{ target: "slow", payload: { type: "go" }, id: "job", timeout: 0.01 }]);
  assert.equal(joined!.status, "timed_out");
  assert.equal("attempts" in joined!, false);
  const [own] = await owner;
  assert.equal(own!.attempts, 1, "an owned execution's final outcome counts its runs");
});

test("timeout refusals carry python's text and the task id (owned and joined)", async () => {
  const k = kernelWith({ slow: async () => { await sleep(200); return { ok: true }; } });
  const owner = k.send([{ target: "slow", payload: { type: "go" }, id: "j", timeout: 0.05 }]);
  await sleep(5);
  const [joined] = await k.send([{ target: "slow", payload: { type: "go" }, id: "j", timeout: 0.01 }]);
  const [own] = await owner;
  assert.deepEqual(own!.result, {
    error: "send: slow 'go' exceeded its 0.05s timeout; task cancelled",
    reason: "timeout",
    task_id: own!.pid,
  });
  assert.deepEqual(joined!.result, {
    error: `send: joined execution ${own!.pid} did not settle within this caller's 0.01s wait; the execution itself keeps running`,
    reason: "timeout",
    task_id: own!.pid,
  });
});

test("a killed task's later sends are refused cancelled and start no work", async () => {
  // JS cannot abort the handler, so it runs on after the cancel; what it sends
  // from then on belongs to dead work (python: a dying task registers no new work).
  const later: { pid: number | null; status: string }[] = [];
  let reached = 0;
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  k.registerBundle("worker.tools", async (_id, _p, view) => {
    await sleep(30);
    const [o] = await view.send([{ target: "sink", payload: { type: "go" } }]);
    later.push({ pid: o!.pid, status: o!.status });
    return { done: true };
  });
  k.registerBundle("sink.tools", () => { reached += 1; return null; });
  k.register(new Agent({ id: "worker", parentId: "kernel_state", handlerModule: "worker.tools" }));
  k.register(new Agent({ id: "sink", parentId: "worker", handlerModule: "sink.tools" }));
  const run = k.send([{ target: "worker", payload: { type: "go" } }]);
  await sleep(5);
  const pid = k.tasks.ids()[0]!;
  await k.send([{ target: "kernel", payload: { type: "cancel", task_id: pid } }]);
  const [out] = await run;
  assert.equal(out!.status, "cancelled");
  await sleep(60);
  assert.deepEqual(later, [{ pid: null, status: "cancelled" }]);
  assert.equal(reached, 0, "the dead handler's send ran");
});

test("send validation: python's texts, and null read the way python reads None", async () => {
  const k = kernelWith({ a: () => ({ ok: true }) });
  const r = async (item: unknown): Promise<Payload> =>
    (await k.send([item as never]))[0]!.result as Payload;
  assert.equal((await r(7))["error"], "send: item must be a dict");
  assert.equal(
    (await r({ target: "a", payload: { type: 5 } }))["error"],
    "send: payload.type must be a non-empty string, got 5",
  );
  assert.equal((await r({ target: "a", payload: { type: "go" }, id: "" }))["error"], "send: id must be a non-empty name");
  assert.equal(
    (await r({ target: "a", payload: { type: "go" }, id: 1.5 }))["error"],
    "send: id must be a name (non-empty string) or a message id (int), got 1.5",
  );
  assert.equal(
    (await r({ target: "a", payload: { type: "go" }, id: true }))["error"],
    "send: id must be a name (non-empty string) or a message id (int), got True",
  );
  // `retries: null` is NOT absent (python `item.get("retries", 0)` is None)
  assert.equal(
    (await r({ target: "a", payload: { type: "go" }, retries: null }))["error"],
    "send: retries must be a non-negative int, got None",
  );
  // `timeout: null` and `id: null` ARE absent — the item dispatches
  const [t] = await k.send([{ target: "a", payload: { type: "go" }, timeout: null, id: null } as never]);
  assert.equal(t!.status, "completed");
  assert.equal(
    (await r({ target: "a", payload: { type: "go" }, timeout: "1" }))["error"],
    "send: timeout must be a positive number of seconds, got '1'",
  );
});

test("unknown_agent is python's shape: agent_count, the first 40 ids, a placeholder hint", async () => {
  const k = kernelWith({});
  for (let i = 0; i < 45; i++) k.register(new Agent({ id: `a${String(i).padStart(2, "0")}`, parentId: "kernel_state" }));
  const [out] = await k.send([{ target: "nobody", payload: { type: "go" } }]);
  const res = out!.result as Payload;
  assert.deepEqual(Object.keys(res), ["error", "reason", "agent_count", "agents"]);
  assert.equal(res["error"], "no agent 'nobody'");
  assert.equal(res["agent_count"], 46);
  assert.equal((res["agents"] as string[]).length, 40);
  const [slot] = await k.send([{ target: "<id>", payload: { type: "go" } }]);
  assert.equal(
    (slot!.result as Payload)["hint"],
    "'<id>' is a PLACEHOLDER from an example, not an id — substitute one of `agents`.",
  );
});

test("`<own guid>|<agent>` names the LOCAL agent; `<own guid>|kernel` the root", async () => {
  const k = kernelWith({ a: () => ({ ok: "local" }) });
  k.updateMeta("kernel_state", { guid: "g1" });
  const forwarded: string[] = [];
  k.bridge = {
    forward: (t) => { forwarded.push(t); return Promise.resolve({ ok: "host" }); },
    forwardBinary: () => Promise.resolve(null),
    emitRemote: () => {},
    watchRemote: () => {},
    unwatchRemote: () => {},
    onLifecycle: () => () => {},
    subscribeState: () => () => {},
  };
  const [a] = await k.send([{ target: "g1|a", payload: { type: "go" } }]);
  assert.deepEqual(a!.result, { ok: "local" });
  const [root] = await k.send([{ target: "g1|kernel", payload: { type: "reflect", tree: "none" } }]);
  assert.equal((root!.result as Payload)["id"], "kernel_state");
  // a local name this kernel lacks is unknown HERE, never forwarded to the host
  const [miss] = await k.send([{ target: "g1|nobody", payload: { type: "go" } }]);
  assert.equal((miss!.result as Payload)["reason"], "unknown_agent");
  assert.equal((miss!.result as Payload)["error"], "no agent 'g1|nobody'");
  assert.deepEqual(forwarded, []);
});
