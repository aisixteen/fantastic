// The kernel's memory-store link when the STORE says no — and the start sequence
// when a boot says no. Port targets: python `Kernel.start` / `flush_executions` /
// `prune_executions` / `_store_read` / `rows_put` (kernel/_kernel.py).
//
// The store here is a real bundle in a real kernel (`spy_store.ts`): it answers
// the four verbs the kernel sends it and can be told to refuse any of them. What
// is pinned is what the kernel DOES with a refusal: what it returns, what stays
// in RAM, which agents count as booted, and the head of the line it logs. The
// tail of each log line (the refused reply as text) is not pinned: python prints
// its `repr`, this kernel JSON — see the step-4 report.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import type { Outcome } from "../src/kernel/outcome.ts";
import { startKernel, stopKernel } from "../src/kernel/lifecycle.ts";

const ROOT = "kernel_state";

interface Spy {
  /** verb → the reply to give instead of the default (absent = default) */
  reply: { [verb: string]: Json };
  /** every batch an `append` carried */
  appended: Payload[][];
  /** what a `read` answers when not overridden */
  rows: Payload[];
}

function kernel(rootModule: string | null = null): { k: Kernel; spy: Spy; boots: Map<string, () => Json> } {
  const spy: Spy = { reply: {}, appended: [], rows: [] };
  const boots = new Map<string, () => Json>();
  const k = new Kernel();
  k.registerBundle("spy_store.ts", (_id, payload) => {
    const verb = String(payload["type"]);
    if (verb === "append") spy.appended.push(payload["rows"] as Payload[]);
    if (verb in spy.reply) return spy.reply[verb]!;
    if (verb === "boot") return null;
    if (verb === "read") return { rows: spy.rows as Json, next: null, total: spy.rows.length };
    if (verb === "append") return { ids: [], appended: (payload["rows"] as Json[]).length };
    if (verb === "prune") return { pruned: 0, grants_removed: 0 };
    return { error: `spy_store: no verb ${verb}`, reason: "unhandled" };
  });
  k.registerBundle("probe.ts", (id, payload) => {
    const verb = payload["type"];
    if (verb === "boot") return (boots.get(id) ?? (() => null))();
    if (verb === "say") return payload["reply"] ?? null;
    return { ok: true };
  });
  k.setRoot(new Agent({ id: ROOT, handlerModule: rootModule }));
  k.register(new Agent({ id: "memory", parentId: ROOT, handlerModule: "spy_store.ts" }));
  k.updateMeta(ROOT, { memory_store_id: "memory" });
  return { k, spy, boots };
}

/** Started, with the background flusher switched off: every flush below is the
 *  test's. (`cancel` refuses the flusher — a kernel service task — so the test
 *  drops its row and its handle itself.) */
async function started(k: Kernel): Promise<void> {
  const out = (await startKernel(k, false)) as Payload;
  assert.equal(out["started"], true, JSON.stringify(out));
  const fid = k.store.flusherTask as number;
  k.store.flusherTask = null;
  k.tasks.done(fid);
}

async function say(k: Kernel, id: string, reply: Json): Promise<Outcome> {
  return (await k.send([{ target: id, payload: { type: "say", reply } }]))[0]!;
}

/** Run `fn` with console.warn captured. */
async function warns(fn: () => Promise<unknown>): Promise<string[]> {
  const lines: string[] = [];
  const orig = console.warn;
  console.warn = (m: string) => lines.push(String(m));
  try {
    await fn();
  } finally {
    console.warn = orig;
  }
  return lines;
}

// ── the flush ──────────────────────────────────────────────────────────

test("a refused flush keeps every row in RAM, logs it, and the next flush lands them — never its own rows", async () => {
  const { k, spy } = kernel();
  k.register(new Agent({ id: "probe", parentId: ROOT, handlerModule: "probe.ts" }));
  await started(k);
  await say(k, "probe", { ok: 1 });
  const pending = [...k.store.rows.keys()].sort((a, b) => a - b);
  assert.ok(pending.length > 0);
  spy.reply["append"] = { error: "disk full" };
  let out: Payload = {};
  const lines = await warns(async () => {
    out = await k.store.flush();
  });
  assert.deepEqual(out, { flushed: 0, error: { error: "disk full" } });
  assert.equal(lines.length, 1, JSON.stringify(lines));
  assert.ok(
    lines[0]!.startsWith(`  [kernel] flush of ${pending.length} execution row(s) to 'memory' failed: errored `),
    lines[0],
  );
  for (const id of pending) assert.ok(k.store.rows.has(id), `row ${id} left RAM on a refused flush`);
  // the refused flush's own call row is bookkeeping of the flush itself
  const ownCall = [...k.store.rows.values()].find(
    (r) => r.kind === "call" && r["agent"] === "memory" && (r["payload"] as Payload)["type"] === "append",
  )!;
  assert.ok(ownCall !== undefined);
  delete spy.reply["append"];
  const second = await k.store.flush();
  assert.equal(typeof second["flush"], "number");
  for (const id of pending) assert.equal(k.store.rows.has(id), false, `row ${id} stayed after the flush landed`);
  const landed = spy.appended[1]!.map((r) => r["id"]);
  for (const id of pending) assert.ok(landed.includes(id), `row ${id} never reached the store`);
  assert.equal(landed.includes(ownCall.id), false, "a flush re-recorded the refused flush's own call row");
  assert.equal(k.store.rows.has(ownCall.id), false, "the refused flush's own call row lingers in RAM");
  k.store.halt();
});

test("a falsy `error` ('' / 0 / null / false) is no failure: the call row carries no error and a flush lands", async () => {
  const { k, spy } = kernel();
  k.register(new Agent({ id: "probe", parentId: ROOT, handlerModule: "probe.ts" }));
  for (const e of ["", 0, null, false] as Json[]) {
    const out = await say(k, "probe", { error: e, n: 1 });
    assert.equal(out.status, "completed", JSON.stringify(e));
    assert.deepEqual(k.store.rows.get(out.pid as number)!["outcome"], { status: "completed" }, JSON.stringify(e));
  }
  // a truthy one is stamped on the live call row: its text (≤ 200) and its reason
  const no = await say(k, "probe", { error: "x".repeat(300), reason: "unhandled" });
  assert.equal(no.status, "errored");
  assert.deepEqual(k.store.rows.get(no.pid as number)!["outcome"], { status: "errored", error: "x".repeat(200), reason: "unhandled" });
  await started(k);
  spy.reply["append"] = { error: "", ids: [] };
  const out = await k.store.flush();
  assert.equal(typeof out["flush"], "number", JSON.stringify(out));
  assert.ok((out["flushed"] as number) > 0);
  k.store.halt();
});

test("a refused prune is logged and prunes nothing; the flush that ran it still landed", async () => {
  const { k, spy } = kernel();
  await started(k);
  // one settled, unnamed call the kernel does not hold in RAM: over a window of 0
  spy.rows = [
    { id: 900001, kind: "call", agent: "x", named: false },
    { id: 900002, kind: "settle", parent: 900001, outcome: { status: "completed", bytes: 2 } },
  ];
  spy.reply["prune"] = { error: "prune broke" };
  k.ledger.cap = 0;
  await k.send([{ target: ROOT, payload: { type: "reflect", tree: "none" } }]);
  let out: Payload = {};
  const lines = await warns(async () => {
    out = await k.store.flush();
  });
  assert.equal(typeof out["flush"], "number");
  assert.equal(out["pruned"], 0);
  const prunes = lines.filter((l) => l.includes("prune"));
  assert.equal(prunes.length, 1, JSON.stringify(lines));
  assert.ok(prunes[0]!.startsWith("  [kernel] prune of the kernel's store failed: "), prunes[0]);
  k.store.halt();
});

test("a store read that refuses, or answers no dict, fails the reading verb loudly", async () => {
  const { k, spy } = kernel();
  await started(k);
  for (const bad of [{ error: "nope" }, null, [1], "x"] as Json[]) {
    spy.reply["read"] = bad;
    const out = (await k.send([{ target: "kernel", payload: { type: "ledger" } }]))[0]!;
    assert.equal(out.status, "failed", JSON.stringify(bad));
    const r = out.result as Payload;
    assert.equal(r["raised"], true);
    assert.match(String(r["error"]), /^\w*Error: kernel: memory store read failed: /, JSON.stringify(r));
  }
  k.store.halt();
});

// ── rows_put ───────────────────────────────────────────────────────────

test("put: a pre-minted unused id is taken; any other id is refused with python's text", () => {
  const { k } = kernel();
  const minted = k.mintId();
  assert.equal(k.store.put("probe", { kind: "note", id: minted }), minted);
  const row = k.store.rows.get(minted)!;
  assert.deepEqual(Object.keys(row), ["kind", "id", "agent", "ts"]);
  assert.equal(row["agent"], "probe");
  const unminted = k.store.nextId;
  for (const id of [0, -1, 1.5, unminted, unminted + 7, minted]) {
    assert.throws(() => k.store.put("probe", { kind: "note", id }), {
      message: `rows_put: id must be absent or a pre-minted unused id, got ${id}`,
    });
  }
  assert.equal(k.store.nextId, unminted, "a refused put mints nothing");
  // absent: the next id is minted for it
  assert.equal(k.store.put(null, { kind: "note" }), unminted);
});

// ── the start sequence ─────────────────────────────────────────────────

test("kernel_start with no root is python's bare error", async () => {
  assert.deepEqual(await startKernel(new Kernel(), true), { error: "kernel: no root agent registered" });
});

test("kernel_start: a memory_store_id naming no agent is unknown_agent, with python's hint", async () => {
  const { k } = kernel();
  k.updateMeta(ROOT, { memory_store_id: "nowhere" });
  const out = await startKernel(k, true);
  assert.deepEqual(out, {
    error: "no agent 'nowhere'",
    reason: "unknown_agent",
    agent_count: 2,
    agents: ["kernel_state", "memory"],
    hint: "the root's `memory_store_id` names no agent in this tree",
  });
  assert.equal(k.store.started, false);
  assert.equal(k.store.flusherTask, null);
});

test("kernel_start: a root or store boot that does not complete refuses the start, naming it and its status", async () => {
  {
    const { k, spy } = kernel();
    spy.reply["boot"] = { error: "no" };
    const out = (await startKernel(k, true)) as Payload;
    assert.deepEqual(Object.keys(out), ["error", "reason"]);
    assert.equal(out["reason"], "unhandled");
    assert.ok(String(out["error"]).startsWith("kernel_start: boot of 'memory' did not complete (errored): "), String(out["error"]));
    assert.equal(k.store.started, false);
    assert.equal(k.get(ROOT)!.meta["next_id"], undefined, "a refused start reserves no id block");
  }
  {
    // a root that runs a bundle, and whose bundle refuses to boot
    const { k, boots } = kernel("probe.ts");
    boots.set(ROOT, () => {
      throw new Error("root will not boot");
    });
    const out = (await startKernel(k, true)) as Payload;
    assert.equal(out["reason"], "unhandled");
    assert.ok(String(out["error"]).startsWith("kernel_start: boot of 'kernel_state' did not complete (failed): "), String(out["error"]));
    assert.equal(k.store.started, false);
  }
});

test("kernel_start: an agent whose boot errs or throws is left out of `booted` and logged; a falsy error boots", async () => {
  const { k, boots } = kernel();
  for (const id of ["p_err", "p_ok", "p_throw", "p_falsy"]) {
    k.register(new Agent({ id, parentId: ROOT, handlerModule: "probe.ts" }));
  }
  boots.set("p_err", () => ({ error: "nope" }));
  boots.set("p_throw", () => {
    throw new Error("boot blew up");
  });
  boots.set("p_falsy", () => ({ error: "", ready: true }));
  let out: Json = null;
  const lines = await warns(async () => {
    out = await startKernel(k, true);
  });
  assert.deepEqual(out, { started: true, store: "memory", booted: [ROOT, "memory", "p_ok", "p_falsy"] });
  assert.deepEqual(lines.map((l) => l.split(" {")[0]), [
    "  [kernel] boot 'p_err': errored",
    "  [kernel] boot 'p_throw': failed",
  ]);
  k.store.halt();
});

test("a final flush that raises is logged, and the stop still completes", async () => {
  const { k, spy } = kernel();
  k.register(new Agent({ id: "probe", parentId: ROOT, handlerModule: "probe.ts" }));
  await started(k);
  await say(k, "probe", { ok: 1 });
  // the flush lands; its retention prune then cannot read the store
  spy.reply["read"] = { error: "read broke" };
  k.ledger.cap = 0;
  const lines = await warns(() => stopKernel(k));
  assert.equal(k.store.stopped, true);
  assert.equal(k.store.flusherTask, null);
  const raised = lines.filter((l) => l.includes("final flush raised"));
  assert.equal(raised.length, 1, JSON.stringify(lines));
  assert.ok(raised[0]!.includes("final flush raised: kernel: memory store read failed: "), raised[0]);
  // stopped: a second stop is a no-op, and logs nothing
  assert.deepEqual(await warns(() => stopKernel(k)), []);
});

// ── the guid ───────────────────────────────────────────────────────────

test("the root's boot cannot mint a guid without crypto.getRandomValues — it fails loud, no fallback", async () => {
  const saved = Object.getOwnPropertyDescriptor(globalThis, "crypto")!;
  try {
    for (const c of [undefined, {}]) {
      Object.defineProperty(globalThis, "crypto", { value: c, configurable: true, writable: true });
      const k = new Kernel();
      k.setRoot(new Agent({ id: ROOT }));
      const out = (await k.send([{ target: "kernel", payload: { type: "boot" } }]))[0]!;
      assert.equal(out.status, "failed");
      assert.deepEqual(out.result, {
        error: "Error: kernel: this runtime has no crypto.getRandomValues — cannot mint the kernel guid",
        raised: true,
      });
      assert.equal(k.guid, null);
      assert.equal(k.get(ROOT)!.meta["guid"], undefined, "nothing was written in its place");
    }
  } finally {
    Object.defineProperty(globalThis, "crypto", saved);
  }
  // and with it, the mint is 128 random bits as 32 hex digits, minted once
  const k = new Kernel();
  k.setRoot(new Agent({ id: ROOT }));
  await k.send([{ target: "kernel", payload: { type: "boot" } }]);
  const g = k.guid;
  assert.match(String(g), /^[0-9a-f]{32}$/);
  await k.send([{ target: "kernel", payload: { type: "boot" } }]);
  assert.equal(k.guid, g, "a minted guid is never regenerated");
});
