// The LEDGER as the kernel reads it back — a projection over the memory store
// plus what is still in RAM — and the paths that build or rebuild its entries:
// the read-back's shape for flushed and in-RAM executions alike, the recall of a
// named execution at the ring cap, the task every store write parents to, and
// what the store keeps of bytes and of absent fields.
//
// The ledger shapes and the recall-at-cap behaviour were measured on python
// (`Kernel.ledger_entries` / `recall_executions`, kernel/_kernel.py) with the
// same calls on 2026-10-01: identical, except that python names its exception
// class `RuntimeError` where this kernel's is `Error`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import type { Item, Outcome } from "../src/kernel/outcome.ts";
import { startKernel, stopKernel } from "../src/kernel/lifecycle.ts";
import { registerRamStore } from "../src/bundles/memory/ram_store.ts";

const ROOT = "kernel_state";
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

interface Probe {
  counts: Map<string, number>;
  running: boolean;
}

function kernel(): { k: Kernel; p: Probe } {
  const p: Probe = { counts: new Map(), running: false };
  const k = new Kernel();
  k.registerBundle("probe.ts", async (id, payload) => {
    const verb = payload["type"];
    if (verb === "boot") return null;
    if (verb === "slow") {
      p.running = true;
      return new Promise<never>(() => undefined);
    }
    if (verb === "count") {
      const n = (p.counts.get(id) ?? 0) + 1;
      p.counts.set(id, n);
      return { n };
    }
    if (verb === "fail") throw new Error("boom");
    if (verb === "echo") return payload["reply"] ?? null;
    return { ok: true };
  });
  registerRamStore(k);
  k.setRoot(new Agent({ id: ROOT }));
  k.register(new Agent({ id: "probe", parentId: ROOT, handlerModule: "probe.ts" }));
  k.register(new Agent({ id: "memory", parentId: ROOT, handlerModule: "ram_store.ts", meta: { operator: true, admins: ["kernel"] } }));
  k.updateMeta(ROOT, { memory_store_id: "memory" });
  return { k, p };
}

async function call(k: Kernel, target: string, payload: Payload, item: Partial<Item> = {}): Promise<Outcome> {
  return (await k.send([{ target, payload, ...item }]))[0]!;
}

/** Started, with the background flusher switched off: every flush is the test's.
 *  (`cancel` refuses the flusher — a kernel service task — so the test drops its
 *  row and its handle itself.) */
async function started(k: Kernel): Promise<void> {
  const out = (await startKernel(k, false)) as Payload;
  assert.equal(out["started"], true, JSON.stringify(out));
  const fid = k.store.flusherTask as number;
  k.store.flusherTask = null;
  k.tasks.done(fid);
}

async function kernelRows(k: Kernel): Promise<Payload[]> {
  const tables = (await call(k, "memory", { type: "dump" })).result as Payload;
  const rows = (tables["tables"] as Payload)["rows"] as { owner: string; row: Payload }[];
  return rows.filter((r) => r.owner === `${k.guid}|kernel`).map((r) => r.row);
}

test("the ledger reads flushed and in-RAM executions back in ONE shape, key for key, oldest first", async () => {
  const { k, p } = kernel();
  await started(k);
  const labels = new Map<number, string>();
  labels.set((await call(k, "probe", { type: "work" }, { id: "named-1" })).pid!, "NAMED");
  labels.set((await call(k, "probe", { type: "work" })).pid!, "PLAIN");
  const dead = await call(k, "probe", { type: "fail" });
  labels.set(dead.pid!, "DEAD");
  assert.deepEqual((await call(k, "kernel", { type: "ack_deadletter", id: dead.pid! })).result, { acked: true, id: dead.pid });
  await k.store.flush();
  labels.set((await call(k, "probe", { type: "work" }, { id: "ram-named" })).pid!, "RAMNAMED");
  labels.set((await call(k, "probe", { type: "work" })).pid!, "RAMPLAIN");
  labels.set((await call(k, "probe", { type: "fail" })).pid!, "RAMDEAD");
  void k.send([{ target: "probe", payload: { type: "slow" } }]);
  while (!p.running) await sleep(1);
  for (const t of k.tasks.ids()) if (k.tasks.get(t)!.verb === "slow") labels.set(t, "SLOW");
  const led = ((await call(k, "kernel", { type: "ledger" })).result as Payload)["executions"] as Payload[];
  const mine = led.filter((e) => labels.has(e["id"] as number)).map((e0) => {
    const e = JSON.parse(JSON.stringify(e0)) as Payload;
    const lab = labels.get(e["id"] as number)!;
    if (e["outcome"] !== undefined) (e["outcome"] as Payload)["pid"] = lab;
    e["id"] = lab;
    return JSON.stringify(e);
  });
  assert.deepEqual(mine, [
    '{"id":"NAMED","target":"probe","attempts":1,"retries":0,"named":true,"name":"named-1","status":"completed","outcome":{"pid":"NAMED","status":"completed","result":{"ok":true},"attempts":1}}',
    '{"id":"PLAIN","target":"probe","attempts":1,"retries":0,"named":false,"status":"completed","outcome":{"pid":"PLAIN","status":"completed","bytes":12,"attempts":1}}',
    '{"id":"DEAD","target":"probe","attempts":1,"retries":0,"named":false,"status":"failed","outcome":{"pid":"DEAD","status":"failed","error":"Error: boom","attempts":1},"acked":true}',
    '{"id":"RAMNAMED","target":"probe","attempts":1,"retries":0,"named":true,"name":"ram-named","status":"completed","outcome":{"pid":"RAMNAMED","status":"completed","result":{"ok":true},"attempts":1}}',
    '{"id":"RAMPLAIN","target":"probe","attempts":1,"retries":0,"named":false,"status":"completed","outcome":{"pid":"RAMPLAIN","status":"completed","bytes":12,"attempts":1}}',
    '{"id":"RAMDEAD","target":"probe","attempts":1,"retries":0,"named":false,"status":"failed","outcome":{"pid":"RAMDEAD","status":"failed","error":"Error: boom","attempts":1}}',
    '{"id":"SLOW","target":"probe","attempts":1,"retries":0,"named":false,"status":"in_flight"}',
  ]);
  // the first three came back from the store, the rest from RAM
  for (const lab of ["NAMED", "PLAIN", "DEAD"]) {
    const id = [...labels].find(([, l]) => l === lab)![0];
    assert.equal(k.ledger.has(id), false, `${lab} is not in RAM`);
  }
  k.store.halt();
});

test("an acked dead letter still in RAM reads back acked; acking twice says `already`", async () => {
  const { k } = kernel();
  await started(k);
  const dead = await call(k, "probe", { type: "fail" });
  assert.deepEqual((await call(k, "kernel", { type: "ack_deadletter", id: dead.pid! })).result, { acked: true, id: dead.pid });
  const led = ((await call(k, "kernel", { type: "ledger" })).result as Payload)["executions"] as Payload[];
  assert.equal(led.find((e) => e["id"] === dead.pid)!["acked"], true);
  assert.deepEqual((await call(k, "kernel", { type: "ack_deadletter", id: dead.pid! })).result, { acked: true, id: dead.pid, already: true });
  await k.store.flush();
  assert.deepEqual((await call(k, "kernel", { type: "ack_deadletter", id: dead.pid! })).result, { acked: true, id: dead.pid, already: true });
  k.store.halt();
});

test("recall AT the ring cap: the recalled named execution is evicted at once and the step runs again (python)", async () => {
  const { k, p } = kernel();
  await started(k);
  const first = await call(k, "probe", { type: "count" }, { id: "job" });
  assert.deepEqual(first.result, { n: 1 });
  await k.store.flush();
  assert.equal(k.ledger.resolveName("job"), undefined, "the name left RAM with its rows");
  // one execution in flight, so the ring cannot get under its cap without the recall
  void k.send([{ target: "probe", payload: { type: "slow" } }]);
  while (!p.running) await sleep(1);
  k.ledger.cap = 1;
  const again = await call(k, "probe", { type: "count" }, { id: "job" });
  assert.deepEqual(again.result, { n: 2 });
  assert.notEqual(again.pid, first.pid);
  assert.equal(p.counts.get("probe"), 2, "the recalled outcome was evicted before the launch could join it");
  assert.equal(k.ledger.resolveName("job"), again.pid, "the name now names the new run");
  // the store still holds the first run's record: the recall read it, the ring dropped it
  k.ledger.cap = 1_000_000;
  const rows = await kernelRows(k);
  assert.ok(rows.some((r) => r["id"] === first.pid && r["kind"] === "call"));
  k.store.halt();
});

test("the flusher's store writes parent to the flusher; the stop's final flush and a direct flush to nothing; the start's probe to its verb", async () => {
  const seen: { verb: string; parent: number | null }[] = [];
  const k = new Kernel();
  k.registerBundle("spy_store.ts", (_id, payload, kernel) => {
    const verb = String(payload["type"]);
    const row = kernel.currentTask !== null ? kernel.tasks.get(kernel.currentTask) : undefined;
    seen.push({ verb, parent: row?.parent ?? null });
    if (verb === "boot") return null;
    if (verb === "read") return { rows: [], next: null, total: 0 };
    if (verb === "append") return { ids: [], appended: (payload["rows"] as Json[]).length };
    return { ok: true };
  });
  k.registerBundle("probe.ts", () => ({ ok: true }));
  k.setRoot(new Agent({ id: ROOT }));
  k.register(new Agent({ id: "memory", parentId: ROOT, handlerModule: "spy_store.ts" }));
  k.register(new Agent({ id: "probe", parentId: ROOT, handlerModule: "probe.ts" }));
  k.updateMeta(ROOT, { memory_store_id: "memory" });
  const start = await call(k, "kernel", { type: "kernel_start" });
  assert.equal((start.result as Payload)["started"], true);
  assert.deepEqual(seen.filter((s) => s.verb === "read"), [{ verb: "read", parent: start.pid }], "the probe read parents to kernel_start");
  const fid = k.store.flusherTask as number;
  seen.length = 0;
  await call(k, "probe", { type: "work" });
  await sleep(300); // the debounce fires: the flusher writes
  const flusherWrites = seen.filter((s) => s.verb === "append");
  assert.ok(flusherWrites.length > 0, "the flusher wrote nothing");
  assert.ok(flusherWrites.every((s) => s.parent === fid), JSON.stringify(flusherWrites));
  seen.length = 0;
  await call(k, "probe", { type: "work" });
  await k.store.flush();
  assert.deepEqual(seen.filter((s) => s.verb === "append").map((s) => s.parent).slice(0, 1), [null], "a direct flush is no task's");
  seen.length = 0;
  await call(k, "probe", { type: "work" });
  await stopKernel(k);
  // the stop's final flush is its last write (a debounced one may precede it)
  const appends = seen.filter((s) => s.verb === "append");
  assert.ok(appends.length > 0, "the stop flushed the last rows");
  assert.equal(appends[appends.length - 1]!.parent, null, JSON.stringify(seen));
});

test("what the store keeps: bytes as their length at any depth, an absent field absent, the digest measured on that", async () => {
  const { k } = kernel();
  await started(k);
  const payload = {
    type: "echo",
    nest: { blob: new Uint8Array([1, 2, 3]), list: [new ArrayBuffer(2), undefined, "x"], gone: undefined },
    reply: { blob: new Uint8Array([9, 9]), u: undefined, deep: [{ b: new Uint8Array(4) }] },
  } as unknown as Payload;
  const out = await call(k, "probe", payload);
  assert.equal(out.status, "completed");
  // the caller holds the real reply
  assert.ok(((out.result as Payload)["blob"] as unknown) instanceof Uint8Array);
  await k.store.flush();
  const rows = await kernelRows(k);
  const c = rows.find((r) => r["id"] === out.pid)!;
  assert.deepEqual(c["payload"], {
    type: "echo",
    nest: { blob: { len: 3 }, list: [{ len: 2 }, null, "x"] },
    reply: { blob: { len: 2 }, deep: [{ b: { len: 4 } }] },
  });
  assert.equal("gone" in ((c["payload"] as Payload)["nest"] as Payload), false);
  const settle = rows.find((r) => r["kind"] === "settle" && r["parent"] === out.pid)!;
  // python `len(json.dumps(...))` of what is kept: {"blob": {"len": 2}, "deep": [{"b": {"len": 4}}]}
  assert.deepEqual(settle["outcome"], { status: "completed", bytes: '{"blob": {"len": 2}, "deep": [{"b": {"len": 4}}]}'.length });
  k.store.halt();
});

test("any OTHER call to the store — an append included — is recorded like every call", async () => {
  const { k } = kernel();
  await started(k);
  const mine = await call(k, "memory", { type: "append", rows: [{ kind: "note", content: "hi" }] });
  assert.equal(mine.status, "completed");
  await k.store.flush();
  const rows = await kernelRows(k);
  const c = rows.find((r) => r["id"] === mine.pid)!;
  assert.equal(c["kind"], "call");
  assert.equal(c["agent"], "memory");
  assert.equal((c["payload"] as Payload)["type"], "append");
  assert.ok(rows.some((r) => r["kind"] === "settle" && r["parent"] === mine.pid), "its settle landed too");
  k.store.halt();
});

test("ack_deadletter and cancel take a message id as an int or its decimal string; ack without an id is missing_arg", async () => {
  const { k, p } = kernel();
  await started(k);
  for (const id of [undefined, null, "", true, false]) {
    const payload: Payload = { type: "ack_deadletter" };
    if (id !== undefined) payload["id"] = id;
    assert.deepEqual((await call(k, "kernel", payload)).result, { error: "ack_deadletter: id required", reason: "missing_arg", arg: "id" }, String(id));
  }
  const dead = await call(k, "probe", { type: "fail" });
  assert.deepEqual((await call(k, "kernel", { type: "ack_deadletter", id: String(dead.pid) })).result, { acked: true, id: dead.pid });
  assert.deepEqual((await call(k, "kernel", { type: "ack_deadletter", id: "0" })).result, {
    error: "ack_deadletter: no execution '0' of yours in the ledger",
    reason: "not_found",
    arg: "id",
  });
  const held = k.send([{ target: "probe", payload: { type: "slow" } }]);
  while (!p.running) await sleep(1);
  const pid = k.tasks.ids().find((t) => k.tasks.get(t)!.verb === "slow")!;
  assert.deepEqual((await call(k, "kernel", { type: "cancel", task_id: String(pid) })).result, { cancelled: true, killed: [pid] });
  assert.equal((await held)[0]!.status, "cancelled");
  assert.deepEqual((await call(k, "kernel", { type: "cancel", task_id: String(pid) })).result, { cancelled: false, killed: [] }, "a finished id kills nothing");
  k.store.halt();
});

test("recall: an int id joins a flushed execution with what its settle kept; a name with no settle row, or never seen, runs", async () => {
  const { k, p } = kernel();
  await started(k);
  const plain = await call(k, "probe", { type: "count" });
  const dead = await call(k, "probe", { type: "fail" });
  await k.store.flush();
  assert.equal(k.ledger.has(plain.pid!), false);
  // unnamed: the settle kept a digest, and the join says exactly that
  assert.deepEqual(await call(k, "probe", { type: "count" }, { id: plain.pid! }), {
    pid: plain.pid,
    status: "completed",
    result: { bytes: '{"n": 1}'.length },
    attempts: 1,
  });
  assert.deepEqual(await call(k, "probe", { type: "fail" }, { id: dead.pid! }), {
    pid: dead.pid,
    status: "failed",
    result: { error: "Error: boom" },
    attempts: 1,
  });
  assert.equal(p.counts.get("probe"), 1, "a join never runs the step");
  // a call row with no settle row was in flight when the process stopped: nothing to recall
  const orphan = k.mintId();
  await call(k, "memory", {
    type: "append",
    rows: [{ id: orphan, kind: "call", agent: "probe", verb: "count", name: "orphan", named: true, payload: { type: "count" } }],
  });
  const o = await call(k, "probe", { type: "count" }, { id: "orphan" });
  assert.notEqual(o.pid, orphan);
  assert.deepEqual({ ...o, pid: 0 }, { pid: 0, status: "completed", result: { n: 2 }, attempts: 1, name: "orphan" });
  const u = await call(k, "probe", { type: "count" }, { id: "never-seen" });
  assert.deepEqual({ ...u, pid: 0 }, { pid: 0, status: "completed", result: { n: 3 }, attempts: 1, name: "never-seen" });
  k.store.halt();
});

test("a stored call row read back with python's defaults: attempts 1, retries 0; a settle without attempts keeps the call's", async () => {
  const { k } = kernel();
  await started(k);
  const a = k.mintId();
  const b = k.mintId();
  await call(k, "memory", {
    type: "append",
    rows: [
      { id: a, kind: "call", agent: "probe", name: "orphan", named: true },
      { id: b, kind: "call", agent: "probe", attempts: 3, retries: 2, caller: "someone" },
      { id: k.mintId(), kind: "settle", parent: b, outcome: { status: "completed", bytes: 1 } },
    ],
  });
  const led = ((await call(k, "kernel", { type: "ledger" })).result as Payload)["executions"] as Payload[];
  const shown = led
    .filter((e) => e["id"] === a || e["id"] === b)
    .map((e0) => {
      const e = JSON.parse(JSON.stringify(e0)) as Payload;
      e["id"] = e0["id"] === a ? "A" : "B";
      if (e["outcome"] !== undefined) (e["outcome"] as Payload)["pid"] = e["id"];
      return JSON.stringify(e);
    });
  assert.deepEqual(shown, [
    '{"id":"A","target":"probe","attempts":1,"retries":0,"named":true,"name":"orphan","status":"in_flight"}',
    '{"id":"B","target":"probe","attempts":3,"retries":2,"named":false,"status":"completed","outcome":{"pid":"B","status":"completed","bytes":1,"attempts":3}}',
  ]);
  k.store.halt();
});
