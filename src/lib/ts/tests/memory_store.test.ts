// The kernel keeps NO conversation and NO durable tape. Its execution rows
// (`call`, `settle`, `ack`, `flush`) live in RAM until the kernel's own flusher
// writes them to the root's MEMORY STORE (`memory_store_id`) and drops them;
// `ledger`, dead letters, `ack_deadletter` and named-call dedup read the store
// back. `kernel_start` refuses to run without a store. Also here: the drain and
// `shutdown_agents`, the id counter's block reservation, and the graceful stop
// landing its last rows. Mirrors python tests/kernel/test_memory_store.py.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import type { Item, Outcome } from "../src/kernel/outcome.ts";
import { startKernel, stopKernel } from "../src/kernel/lifecycle.ts";
import { ID_BLOCK } from "../src/kernel/kernel_store.ts";
import { registerRamStore } from "../src/bundles/memory/ram_store.ts";
import { sendAs } from "./kit.ts";

const ROOT = "kernel_state";
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const never = new Promise<never>(() => undefined);

interface Probe {
  shut: string[];
  counts: Map<string, number>;
  running: Set<string>;
}

/** A probe bundle: advertises `shutdown`, can hold forever, fail, and count. */
function probeBundle(k: Kernel): Probe {
  const p: Probe = { shut: [], counts: new Map(), running: new Set() };
  k.registerBundle("mem_probe.ts", async (id, payload, kernel) => {
    const verb = payload["type"];
    if (verb === "boot") return null;
    if (verb === "shutdown") {
      p.shut.push(id);
      // the shutdown dispatch may still use its tools: a nested send passes the drain
      const r = (await kernel.send([{ target: "kernel", payload: { type: "ps" } }]))[0]!;
      return { wrapped: true, tools_ok: r.status === "completed" };
    }
    if (verb === "slow") {
      p.running.add(id);
      return never; // JS cannot abort a handler: this one never answers
    }
    if (verb === "work") return { ok: true };
    if (verb === "count") {
      const n = (p.counts.get(id) ?? 0) + 1;
      p.counts.set(id, n);
      return { n };
    }
    if (verb === "fail") throw new Error("probe failed on purpose");
    return { error: `unknown ${String(verb)}`, reason: "unhandled" };
  }, { shutdown: "wraps up" });
  return p;
}

function mk(k: Kernel, id = "probe", meta: Payload = {}): string {
  k.register(
    new Agent({
      id,
      parentId: ROOT,
      handlerModule: "mem_probe.ts",
      meta: { callable_by: { $parent: "*", caller: "*" }, ...meta },
    }),
  );
  return id;
}

function kernel(): { k: Kernel; p: Probe } {
  const k = new Kernel();
  const p = probeBundle(k);
  registerRamStore(k);
  // the root accepts `caller`'s ledger reads and acks — both ends name them
  k.setRoot(new Agent({ id: ROOT, meta: { callable_by: { caller: ["ledger", "ack_deadletter"] } } }));
  return { k, p };
}

/** A ram_store `memory` under the root, named by the root; the kernel started. */
async function withStore(k: Kernel, opts: { start?: boolean } = {}): Promise<void> {
  k.register(
    new Agent({
      id: "memory",
      parentId: ROOT,
      handlerModule: "ram_store.ts",
    }),
  );
  k.updateMeta(ROOT, { memory_store_id: "memory" });
  if (opts.start ?? true) {
    const out = (await startKernel(k, false)) as Payload;
    assert.equal(out["started"], true, JSON.stringify(out));
  }
}

async function call(k: Kernel, target: string, payload: Payload, item: Partial<Item> = {}): Promise<Outcome> {
  return (await k.send([{ target, payload, ...item }]))[0]!;
}

async function res(k: Kernel, target: string, payload: Payload): Promise<Payload> {
  return (await call(k, target, payload)).result as Payload;
}

/** The kernel's own rows in the store — read as kernel bookkeeping, so it works
 *  while the kernel drains, and records nothing. */
async function kernelRows(k: Kernel): Promise<Payload[]> {
  const tables = ((await k.bookkeeping("memory", { type: "dump" })).result as Payload)["tables"] as Payload;
  const rows = tables["rows"] as { owner: string; row: Payload }[];
  return rows.filter((r) => r.owner === `${k.guid}|kernel`).map((r) => r.row);
}

// ── the drain and the agents half of a stop ─────────────────────────────

test("draining refuses new roots but not the kernel's own bookkeeping", async () => {
  const { k } = kernel();
  const a = mk(k, "probe", { can_call: { [ROOT]: ["ps"] } });
  k.updateMeta(ROOT, { callable_by: { probe: ["ps"] } });
  k.store.draining = true;
  const out = await call(k, a, { type: "work" });
  assert.equal(out.status, "failed");
  assert.equal((out.result as Payload)["reason"], "draining");
  assert.equal(out.pid, null, "a refused root costs no id");
  // kernel bookkeeping is a plain call into the handler — no send, no drain
  const own = await k.bookkeeping(a, { type: "work" });
  assert.deepEqual(own, { status: "completed", result: { ok: true } });
  // a `shutdown` the kernel runs may still use its tools
  const shut = await k.bookkeeping(a, { type: "shutdown" }, { shutdown: true });
  assert.deepEqual(shut.result, { wrapped: true, tools_ok: true });
  // the root's control surface stays open
  const ps = await call(k, "kernel", { type: "ps" });
  assert.equal(ps.status, "completed");
});

test("shutdown_agents tells advertisers, waits, then cancels the rest", async () => {
  const { k, p } = kernel();
  const a = mk(k, "sp_a");
  const b = mk(k, "sp_b");
  k.store.quietTimeoutMs = 300;
  const slow = k.send([{ target: b, payload: { type: "slow" } }]);
  while (!p.running.has(b)) await sleep(5);
  const out = await res(k, "kernel", { type: "shutdown_agents" });
  assert.equal(out["drained"], true);
  assert.deepEqual([...(out["shut"] as string[])].sort(), [a, b]);
  assert.deepEqual(new Set(p.shut), new Set([a, b]), "every advertiser was told");
  assert.ok((out["cancelled"] as number[]).length > 0, "the slow task was cancelled after the quiet bound");
  const settled = await slow;
  assert.equal(settled[0]!.status, "cancelled");
  const r = await call(k, a, { type: "work" });
  assert.equal((r.result as Payload)["reason"], "draining");
});

// ── kernel_start: no store, no start ───────────────────────────────────

test("kernel_start fails loud without a store, naming what to create", async () => {
  const { k } = kernel();
  const out = await res(k, "kernel", { type: "kernel_start" });
  assert.equal(out["reason"], "missing_arg");
  assert.equal(out["arg"], "memory_store_id");
  assert.match(String(out["hint"]), /create a ram_store and name it in memory_store_id/);
  assert.equal(k.store.started, false);
});

test("kernel_start boots the root (minting the guid), the store, then everyone; idempotent", async () => {
  const { k } = kernel();
  mk(k);
  await withStore(k, { start: false });
  assert.equal(k.guid, null);
  const out = await res(k, "kernel", { type: "kernel_start" });
  assert.equal(out["started"], true);
  assert.equal(out["store"], "memory");
  assert.deepEqual((out["booted"] as string[]).slice(0, 2), [ROOT, "memory"]);
  assert.ok((out["booted"] as string[]).includes("probe"));
  assert.match(String(k.guid), /^[0-9a-f]{32}$/, "the root's boot minted the guid");
  const again = await res(k, "kernel", { type: "kernel_start" });
  assert.equal(again["already"], true);
});

test("an explicit guid wins over the mint", async () => {
  const { k } = kernel();
  k.updateMeta(ROOT, { guid: "mine" });
  await withStore(k);
  assert.equal(k.guid, "mine");
});

// ── the flusher ────────────────────────────────────────────────────────

test("execution rows flush into the store and leave RAM", async () => {
  const { k } = kernel();
  mk(k);
  await withStore(k);
  const out = await call(k, "probe", { type: "work" });
  const pid = out.pid as number;
  await k.store.flush();
  const rows = await kernelRows(k);
  const c = rows.find((r) => r["id"] === pid)!;
  assert.equal(c["kind"], "call");
  assert.equal(c["caller"], null);
  const settle = rows.find((r) => r["kind"] === "settle" && r["parent"] === pid)!;
  // UNNAMED: the settle keeps a digest (python's json.dumps length), never the body
  assert.deepEqual(settle["outcome"], { status: "completed", bytes: '{"ok": true}'.length });
  assert.ok(rows.some((r) => r["kind"] === "flush" && (r["covers"] as number[]).includes(pid)));
  assert.equal(k.store.rows.has(pid), false, "flushed rows leave RAM");
  assert.equal(k.ledger.has(pid), false, "a settled, flushed execution too");
});

test("stored rows keep null and reduce bytes to a length; dump → inject round-trips", async () => {
  const { k } = kernel();
  mk(k);
  await withStore(k);
  const out = await call(k, "probe", {
    type: "work",
    blob: new Uint8Array([0, 1, 2]) as unknown as Json,
    none: null,
  });
  await k.store.flush();
  const c = (await kernelRows(k)).find((r) => r["id"] === out.pid)!;
  const payload = c["payload"] as Payload;
  assert.deepEqual(payload["blob"], { len: 3 });
  assert.ok("none" in payload && payload["none"] === null);
  const before = (await res(k, "memory", { type: "dump" }))["tables"] as Payload;
  await res(k, "memory", { type: "inject", tables: before });
  const after = (await res(k, "memory", { type: "dump" }))["tables"] as Payload;
  const bRows = before["rows"] as Json[];
  assert.deepEqual((after["rows"] as Json[]).slice(0, bRows.length), bRows);
  assert.deepEqual(after["fold_refs"], before["fold_refs"]);
});

test("a settle keeps the body only when named, the error when dead, else {status, bytes}", async () => {
  const { k } = kernel();
  mk(k);
  await withStore(k);
  const named = await call(k, "probe", { type: "work" }, { id: "n-1" });
  const plain = await call(k, "probe", { type: "work" });
  const dead = await call(k, "probe", { type: "fail" });
  await k.store.flush();
  const kept = new Map<unknown, Payload>();
  for (const r of await kernelRows(k)) if (r["kind"] === "settle") kept.set(r["parent"], r["outcome"] as Payload);
  assert.deepEqual(kept.get(named.pid), { status: "completed", result: { ok: true } });
  assert.deepEqual(kept.get(plain.pid), { status: "completed", bytes: 12 });
  const d = kept.get(dead.pid)!;
  assert.equal(d["status"], "failed");
  assert.match(String(d["error"]), /probe failed on purpose/);
  assert.equal("result" in d, false);
});

test("repeated ledger calls grow the store linearly", async () => {
  const { k } = kernel();
  mk(k);
  await withStore(k);
  const size = async (): Promise<number> => {
    await k.store.flush();
    return JSON.stringify((await res(k, "memory", { type: "dump" }))["tables"]).length;
  };
  const sizes = [await size()];
  for (let i = 0; i < 6; i++) {
    await call(k, "kernel", { type: "ledger" });
    sizes.push(await size());
  }
  const steps = sizes.slice(1).map((s, i) => s - (sizes[i] as number));
  assert.ok(Math.max(...steps.slice(1)) < 2 * (steps[1] as number), `not linear: ${sizes.join(",")}`);
});

test("the flush records itself and never loops", async () => {
  const { k } = kernel();
  mk(k);
  await withStore(k);
  await call(k, "probe", { type: "work" });
  await sleep(600); // the flusher's own debounce, several times over
  const n1 = (await kernelRows(k)).length;
  await sleep(600);
  const rows = await kernelRows(k);
  const byId = new Map(rows.map((r) => [r["id"], r]));
  for (const f of rows.filter((r) => r["kind"] === "flush")) {
    const covered = (f["covers"] as number[]).filter((i) => byId.has(i)).map((i) => byId.get(i)!);
    assert.ok(
      !covered.every((c) => c["kind"] === "call" && (c["payload"] as Payload)["type"] === "append"),
      `a flush that only re-records flushes — the loop is back: ${JSON.stringify(f)}`,
    );
  }
  assert.ok(rows.length >= n1);
  // quiet now: no flush is pending once nothing but its own rows remained
  const settledCount = rows.length;
  await sleep(400);
  const later = await kernelRows(k);
  // the two dumps above are calls themselves; what must not happen is growth
  // without traffic beyond them
  assert.ok(later.length - settledCount <= 6, `the flusher keeps writing: ${later.length - settledCount}`);
});

test("ledger is a projection of store + RAM, scoped to the caller", async () => {
  const { k, p } = kernel();
  mk(k);
  // `ledger` and `ack_deadletter` at the root: both ends name them.
  mk(k, "caller", { can_call: { probe: "*", [ROOT]: ["ledger", "ack_deadletter"] } });
  await withStore(k);
  const asCaller = { send: (items: Item[]) => sendAs(k, "caller", items) };
  const mine = (await asCaller.send([{ target: "probe", payload: { type: "work" }, id: "mine-1" }]))[0]!;
  const other = await call(k, "probe", { type: "work" });
  await k.store.flush();
  void k.send([{ target: "probe", payload: { type: "slow" } }]);
  while (!p.running.has("probe")) await sleep(5);
  const led = await res(k, "kernel", { type: "ledger" });
  const by = new Map((led["executions"] as Payload[]).map((e) => [e["id"], e]));
  assert.equal(by.get(mine.pid)!["status"], "completed", "read back from the store");
  assert.deepEqual((by.get(mine.pid)!["outcome"] as Payload)["result"], { ok: true }, "named: kept");
  assert.equal("result" in (by.get(other.pid)!["outcome"] as Payload), false, "unnamed: a digest");
  assert.equal((by.get(other.pid)!["outcome"] as Payload)["bytes"], '{"ok": true}'.length);
  assert.ok((led["executions"] as Payload[]).some((e) => e["status"] === "in_flight"), "RAM overlay");
  const scoped = (await asCaller.send([{ target: "kernel", payload: { type: "ledger" } }]))[0]!.result as Payload;
  const ids = new Set((scoped["executions"] as Payload[]).map((e) => e["id"]));
  assert.ok(ids.has(mine.pid) && !ids.has(other.pid));
});

test("named dedup survives the flush", async () => {
  const { k, p } = kernel();
  mk(k);
  await withStore(k);
  const first = await call(k, "probe", { type: "count" }, { id: "job-1" });
  await k.store.flush();
  assert.equal(k.ledger.resolveName("job-1"), undefined, "the name left RAM with its rows");
  const again = await call(k, "probe", { type: "count" }, { id: "job-1" });
  assert.deepEqual(again.result, { n: 1 });
  assert.deepEqual(first.result, { n: 1 });
  assert.equal(again.pid, first.pid);
  assert.equal(p.counts.get("probe"), 1, "the step never ran twice");
});

test("named dedup survives a restart on the same store (dump → new kernel → inject)", async () => {
  const { k, p } = kernel();
  mk(k);
  await withStore(k);
  await call(k, "probe", { type: "count" }, { id: "nightly" });
  await k.store.flush();
  const tables = (await res(k, "memory", { type: "dump" }))["tables"] as Payload;
  const records = k.save().records; // what the proxy_loader persisted
  // a NEW kernel from the same records: same guid, same counter bound, an empty RAM store
  const k2 = new Kernel();
  probeBundle(k2);
  registerRamStore(k2);
  k2.load(records);
  assert.equal(k2.guid, k.guid);
  assert.ok(k2.store.nextId >= k.store.nextId, "ids never repeat across a restart");
  await res(k2, "memory", { type: "inject", tables });
  const out = (await startKernel(k2, false)) as Payload;
  assert.equal(out["started"], true, JSON.stringify(out));
  const again = await call(k2, "probe", { type: "count" }, { id: "nightly" });
  assert.deepEqual(again.result, { n: 1 });
  assert.equal(p.counts.get("probe"), 1);
});

// ── dead letters ───────────────────────────────────────────────────────

test("a dead letter is announced, listed and acked", async () => {
  const { k } = kernel();
  mk(k);
  // `ledger` and `ack_deadletter` at the root: both ends name them.
  mk(k, "caller", { can_call: { probe: "*", [ROOT]: ["ledger", "ack_deadletter"] } });
  await withStore(k);
  const inbox: Payload[] = [];
  k.onInbox("caller", (e) => inbox.push(e));
  const asCaller = { send: (items: Item[]) => sendAs(k, "caller", items) };
  const out = (await asCaller.send([{ target: "probe", payload: { type: "fail" } }]))[0]!;
  assert.equal(out.status, "failed");
  const ev = inbox.find((e) => e["type"] === "on_deadletter")!;
  assert.equal(ev["id"], out.pid);
  assert.equal(ev["caller"], "caller");
  await k.store.flush();
  const led = (await asCaller.send([{ target: "kernel", payload: { type: "ledger" } }]))[0]!.result as Payload;
  const entry = (led["executions"] as Payload[]).find((e) => e["id"] === out.pid)!;
  assert.equal(entry["status"], "failed");
  assert.equal(entry["acked"], undefined);
  const acked = (await asCaller.send([{ target: "kernel", payload: { type: "ack_deadletter", id: out.pid } }]))[0]!.result;
  assert.deepEqual(acked, { acked: true, id: out.pid });
  const ok = (await asCaller.send([{ target: "probe", payload: { type: "work" } }]))[0]!;
  const refused = (await asCaller.send([{ target: "kernel", payload: { type: "ack_deadletter", id: ok.pid } }]))[0]!.result as Payload;
  assert.equal(refused["reason"], "bad_arg", "only a dead letter is acked");
  // another agent cannot ack mine — even holding the named grant
  k.updateMeta("probe", { can_call: { [ROOT]: ["ack_deadletter"] } });
  k.updateMeta(ROOT, { callable_by: { caller: ["ledger", "ack_deadletter"], probe: ["ack_deadletter"] } });
  const nosy = (await sendAs(k, "probe", [{ target: "kernel", payload: { type: "ack_deadletter", id: out.pid } }]))[0]!.result as Payload;
  assert.equal(nosy["reason"], "not_found");
  await k.store.flush();
  const led2 = await res(k, "kernel", { type: "ledger" });
  assert.equal((led2["executions"] as Payload[]).find((e) => e["id"] === out.pid)!["acked"], true);
  assert.ok((await kernelRows(k)).some((r) => r["kind"] === "ack" && r["parent"] === out.pid));
});

test("a graceful stop lands in-flight work as a cancelled dead letter", async () => {
  const { k, p } = kernel();
  mk(k);
  await withStore(k);
  k.store.quietTimeoutMs = 200;
  const slow = k.send([{ target: "probe", payload: { type: "slow" } }]);
  while (!p.running.has("probe")) await sleep(5);
  const pid = (((await res(k, "kernel", { type: "ps" }))["tasks"]) as Payload[]).find((t) => t["verb"] === "slow")!["id"];
  await stopKernel(k);
  assert.equal((await slow)[0]!.status, "cancelled");
  assert.equal(k.store.stopped, true);
  // a stopped kernel refuses new roots; the store is read as kernel bookkeeping
  assert.equal((await res(k, "memory", { type: "dump" }))["reason"], "draining");
  const rows = await kernelRows(k);
  const settle = rows.find((r) => r["kind"] === "settle" && r["parent"] === pid)!;
  assert.equal((settle["outcome"] as Payload)["status"], "cancelled", "a dead letter, flushed");
});

test("kernel_stop acks, then stops the kernel gracefully", async () => {
  const { k } = kernel();
  await withStore(k);
  const out = await res(k, "kernel", { type: "kernel_stop" });
  assert.deepEqual(out, { type: "kernel_stop", ok: true });
  await k.store.stopping;
  assert.equal(k.store.stopped, true);
  assert.equal(k.store.draining, true);
});

// ── retention: the store-side window ───────────────────────────────────

test("the kernel's store is pruned to its window; named and unacked dead letters survive", async () => {
  const { k } = kernel();
  mk(k);
  await withStore(k);
  const named = await call(k, "probe", { type: "work" }, { id: "keep-me" });
  const dead = await call(k, "probe", { type: "fail" });
  for (let i = 0; i < 40; i++) await call(k, "probe", { type: "work" });
  k.ledger.cap = 20;
  await k.store.flush();
  const rows = await kernelRows(k);
  const calls = rows.filter((r) => r["kind"] === "call" && !r["internal"]);
  const ids = new Set(calls.map((r) => r["id"]));
  assert.ok(calls.length <= 22, `the store holds ${calls.length} calls against a window of 20`);
  assert.ok(ids.has(named.pid), "a named execution is kept — it carries a dedup contract");
  assert.ok(ids.has(dead.pid), "an unacked dead letter waits for a person");
  const settles = new Set(rows.filter((r) => r["kind"] === "settle").map((r) => r["parent"]));
  for (const id of ids) {
    assert.ok(settles.has(id) || k.ledger.has(id as number), "no call loses its settle");
  }
});

// ── the id counter: block reservation in the root record ───────────────

test("the id counter reserves blocks in the root record", async () => {
  const { k } = kernel();
  assert.equal(k.get(ROOT)!.meta["next_id"], undefined, "an unstarted kernel never edits a record");
  await withStore(k);
  const n = k.mintId();
  const bound = k.get(ROOT)!.meta["next_id"] as number;
  assert.ok(typeof bound === "number" && bound > n, "start wrote the reserved bound");
  for (let i = 0; i < ID_BLOCK; i++) k.mintId();
  const next = k.get(ROOT)!.meta["next_id"] as number;
  assert.ok(next > bound, "a started kernel persists the next block");
  const k2 = new Kernel();
  registerRamStore(k2);
  probeBundle(k2);
  k2.load(k.save().records);
  assert.ok(k2.mintId() >= next, "a reload resumes past the reserved block");
});

// The flush mints exactly ONE id (its `flush` row): with no headroom left, that
// mint reserves the next block inside the flush.
for (const headroom of [0]) {
  test(`an id block reserved inside the flush publishes nothing (CLI-B, headroom ${headroom})`, async () => {
    const { k } = kernel();
    await withStore(k);
    mk(k);
    await k.store.flush();
    await call(k, "probe", { type: "work" }); // rows to flush
    const seen: Payload[] = [];
    const unsub = k.addStateSubscriber((e) => seen.push(e as unknown as Payload));
    try {
      k.store.idReserved = k.store.nextId + headroom; // the flush's own mints cross the bound
      const before = k.store.idReserved;
      const out = await k.store.flush();
      assert.ok((out["flushed"] as number) > 0, JSON.stringify(out));
      assert.ok(k.store.idReserved > before);
      assert.deepEqual(seen, []);
      assert.equal(k.get(ROOT)!.meta["next_id"], k.store.idReserved, "the record holds the bound");
    } finally {
      unsub();
    }
  });
}

test("an unstarted kernel keeps its rows bounded by the ledger's ring", async () => {
  const { k } = kernel();
  mk(k);
  k.ledger.cap = 5;
  for (let i = 0; i < 30; i++) await call(k, "probe", { type: "work" });
  assert.ok(k.ledger.size() <= 5);
  // each held execution keeps its call + settle row; nothing else lingers
  assert.ok(k.store.rows.size <= 2 * 5, `rows grew past the ring: ${k.store.rows.size}`);
});


// ── the flusher is a task; the kernel's bookkeeping runs inside its task ──
// python `Kernel.start`: `spawn_task(root, "flush_executions", keepalive=True)`
// — a `task` row, a live `ps` row from then on — and the start's boots and probe
// are BOOKKEEPING: the handlers run directly, recorded as no call.

test("kernel_start spawns the flusher as a task; its boots are bookkeeping, not calls", async () => {
  const { k } = kernel();
  mk(k);
  await withStore(k, { start: false });
  const started = await call(k, "kernel", { type: "kernel_start" });
  assert.equal((started.result as Payload)["started"], true);
  const startPid = started.pid as number;
  const ps = (((await res(k, "kernel", { type: "ps" }))["tasks"]) as Payload[]);
  const flusher = ps.find((t) => t["verb"] === "flush_executions");
  assert.ok(flusher !== undefined, JSON.stringify(ps));
  assert.equal(flusher!["agent"], ROOT);
  // adopted: its parent (the finished kernel_start) handed it up to the operator
  assert.equal(flusher!["parent"], null);
  await k.store.flush();
  const rows = await kernelRows(k);
  const task = rows.find((r) => r["id"] === flusher!["id"])!;
  assert.deepEqual([task["kind"], task["verb"], task["parent"]], ["task", "flush_executions", startPid]);
  const boots = rows.filter((r) => r["kind"] === "call" && (r["payload"] as Payload)["type"] === "boot");
  assert.deepEqual(boots, [], "a boot the kernel runs is bookkeeping, recorded as no call");
});

test("a `ledger` verb's store reads are bookkeeping — no call row; a stop ends the flusher last", async () => {
  const { k } = kernel();
  mk(k);
  await withStore(k);
  const fid = k.store.flusherTask as number;
  await call(k, "probe", { type: "work" });
  await sleep(250); // the debounce fires: the flusher writes
  const led = await call(k, "kernel", { type: "ledger" });
  assert.equal(led.status, "completed");
  const reads = [...k.store.rows.values()].filter(
    (r) => r.kind === "call" && r["agent"] === "memory",
  );
  assert.deepEqual(reads, [], "the kernel's own store traffic mints no call row");
  // a stop neither waits for the flusher nor cancels it: it ends it last
  const out = await res(k, "kernel", { type: "shutdown_agents" });
  assert.deepEqual(out["cancelled"], []);
  assert.equal(k.tasks.has(fid), true);
  await stopKernel(k);
  assert.equal(k.tasks.has(fid), false);
});

// owner 2026-10-03 (audit KV-5): `cancel` refuses the kernel's own service tasks
test("cancel refuses the flusher; ps still lists it and it keeps flushing", async () => {
  const { k } = kernel();
  mk(k);
  await withStore(k);
  const fid = k.store.flusherTask as number;
  const out = await call(k, "kernel", { type: "cancel", task_id: fid });
  assert.equal(out.status, "errored");
  assert.deepEqual(out.result, {
    error: `cancel: task ${fid} (flush_executions) is a kernel service task — not cancellable`,
    reason: "bad_arg",
    hint: "it runs for the life of the kernel and stays listed in ps; kernel_stop ends it",
    arg: "task_id",
  });
  const ps = (await res(k, "kernel", { type: "ps" }))["tasks"] as Payload[];
  assert.ok(ps.some((t) => t["id"] === fid), "still listed");
  const w = await call(k, "probe", { type: "work" });
  await sleep(250);
  assert.equal(k.store.rows.has(w.pid as number), false, "the flusher kept writing");
});

// owner 2026-10-03 (audit CLI-B): the kernel's own ledger flush publishes
// NOTHING on the state stream — a long-poll waiter is not woken; an id block
// reserved inside the flush is quiet too
test("the ledger flush publishes nothing on the state stream", async () => {
  const { k } = kernel();
  mk(k);
  await withStore(k);
  await k.store.flush(); // start from an empty RAM
  const seen: Payload[] = [];
  const off = k.addStateSubscriber((e) => seen.push(e as unknown as Payload));
  await call(k, "kernel", { type: "list_agents" }); // rows to flush
  seen.length = 0;
  const out = await k.store.flush();
  assert.ok((out["flushed"] as number) > 0, JSON.stringify(out));
  assert.deepEqual(seen, [], "a flush wakes no waiter");
  await call(k, "kernel", { type: "list_agents" });
  // the flush's own id crosses the bound inside the flush
  while (k.store.nextId < k.store.idReserved) k.store.mintId();
  const before = k.store.idReserved;
  await k.store.flush();
  assert.ok(k.store.idReserved > before);
  assert.deepEqual(seen, [], "the reservation inside the flush is quiet");
  assert.equal(k.get(ROOT)!.meta["next_id"], k.store.idReserved, "the record holds the bound");
  off();
});
