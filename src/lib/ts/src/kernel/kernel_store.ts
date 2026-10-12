// THE KERNEL'S EXECUTION ROWS and its link to the ROOT MEMORY STORE — port of
// python `Kernel.mint_id` / `rows_put` / `flush_executions` / `prune_executions`
// / `ledger_entries` / `recall_executions` (kernel/_kernel.py).
//
// The kernel keeps NO conversation. What it keeps is its own bookkeeping — one
// row per `call`, per `settle`, per `ack`, per `flush` — in RAM until THE KERNEL
// ITSELF flushes them into the memory store the root record names
// (`memory_store_id`, required by `kernel_start`), under the store key
// `<guid>|kernel`. Once flushed a row leaves RAM, and a settled execution whose
// settle row landed leaves the ledger's RAM table: `ledger`, dead letters,
// `ack_deadletter` and named-call dedup READ THE STORE back.
//
// Every read and write of the store is KERNEL BOOKKEEPING (`Kernel.bookkeeping`):
// the kernel runs the store's handler directly — not a send, so it mints no call
// or settle row of its own, whoever's dispatch asked for it.
//
// Every piece of state here is shared by reference: a handler's kernel view
// (`Object.create(kernel)`) reaches THIS object through its prototype, so a
// counter bumped through any view is the one counter. (Assigning a scalar on a
// view would shadow it on that view alone — which is why none lives on Kernel.)

import type { Kernel } from "./kernel.ts";
import type { Json, Payload } from "./json.ts";
import type { Outcome } from "./outcome.ts";
import { DEAD_STATUSES } from "./outcome.ts";
import type { Exec } from "./ledger.ts";
import { storeSafe, storedOutcome } from "./ledger.ts";
import { nowSeconds, timers } from "./timers.ts";

/** One execution row, as RAM and the store hold it. */
export type Row = Payload & { id: number; kind: string };

/** A ledger entry before the verb scopes it: `_caller` is the originator. */
export type Entry = Payload & { id: number; _caller: string | null };

/** How many ids one reservation in the root record covers. A crash skips at
 *  most this many (ids are opaque; a gap is harmless), never reuses one. */
export const ID_BLOCK = 1000;
/** Quiet before a batch of rows is written — bursts land as one batch. */
export const FLUSH_DEBOUNCE_MS = 150;

function isErr(res: Json | undefined): res is Payload {
  return (
    res !== null && typeof res === "object" && !Array.isArray(res) &&
    Boolean((res as Payload)["error"])
  );
}

function warn(msg: string): void {
  (globalThis as { console?: { warn(m: string): void } }).console?.warn(msg);
}

export class KernelStore {
  /** The BASE kernel (never an agent's view): the store is read and written
   *  through its bookkeeping. */
  private readonly k: Kernel;

  // ── THE ONE COUNTER ──────────────────────────────────────────────────
  // Every id in the kernel — a call (one per batch item), a settle, a flush, a
  // conversation row an AI agent writes to its store — is the next value of this
  // int. Kernel-wide and monotonic: `a < b` means "a was created before b", and
  // an id is never reused (a stale `cancel` kills nothing).
  //
  // PERSISTED BY BLOCK RESERVATION, beside `guid` in the ROOT RECORD: `load`
  // reserves a block past the persisted bound (in memory only — load never edits
  // a record), `kernel_start` writes that bound into the root record (`next_id`),
  // and a started kernel minting past it reserves and writes the next block — the
  // proxy_loader persists it like any record edit. The guid and the counter
  // persist TOGETHER: a root that is not durable mints a new guid on its next
  // boot, so every store key is new and ids restarting from 1 cannot collide.
  nextId = 1;
  idReserved = 1;

  /** Execution rows minted and NOT YET FLUSHED, id-addressed. */
  readonly rows = new Map<number, Row>();

  /** The memory store rows flush to — the root's `memory_store_id`, set by
   *  `kernel_start`. null ⇒ not started: rows stay in RAM. */
  storeId: string | null = null;
  started = false;
  /** DRAINING: the first phase of a graceful stop. New roots are refused
   *  `{reason:"draining"}`; sends with a live parent task, and the kernel's own
   *  traffic, still pass. */
  draining = false;
  /** Set once `shutdown` ran: the flusher is off. */
  stopped = false;
  /** The graceful stop `kernel_stop` scheduled, for whoever wants to await it. */
  stopping: Promise<void> | null = null;

  // Bounds of a graceful stop (mutable so a test can shorten them).
  shutdownVerbTimeout = 30; // seconds, one bound for every agent's `shutdown`
  quietTimeoutMs = 10_000; // in-flight work may take this long to settle
  settleTimeoutMs = 2_000; // a cancelled batch may take this long to record its end

  private flushTimer: unknown = null;
  private flushChain: Promise<unknown> = Promise.resolve();
  /** THE FLUSHER'S TASK (python `spawn_task(root, "flush_executions",
   *  keepalive=True)`): a row in the task table from `kernel_start` on, so `ps`
   *  shows it — a kernel service task, which `cancel` refuses (`isService`).
   *  null = not started, or stopped. */
  flusherTask: number | null = null;
  /** How many `call` rows the store holds for the kernel (re-read at every
   *  prune) — what the retention window is measured against. */
  storedCalls = 0;
  /** > 0 while the kernel's own flush runs: an id-block reservation then
   *  publishes nothing (python `_state_quiet`; a page has no task-scoped
   *  context, so a reservation by other work in that window is quiet too). */
  private flushing = 0;
  /** UNSTARTED ONLY: execution id → its settle/ack row ids. A kernel that was
   *  never started has nowhere to flush, so when the ledger's RAM ring evicts an
   *  execution its rows go with it — RAM stays bounded by the same ring (a
   *  browser page runs unstarted for hours; python's unstarted kernel is a
   *  one-shot). Cleared at `kernel_start`, when rows start leaving by flush. */
  private readonly unflushable = new Map<number, number[]>();

  constructor(k: Kernel) {
    this.k = k;
    k.ledger.onEvict = (id) => this.dropEvicted(id);
  }

  /** The next message id. The ONLY allocator in the kernel. Reserves (and, once
   *  started, persists) a new block when the old one is spent. */
  mintId(): number {
    const n = this.nextId;
    this.nextId = n + 1;
    if (n >= this.idReserved) {
      this.idReserved = n + ID_BLOCK;
      if (this.started && this.k.rootId !== null) {
        if (this.flushing > 0) {
          // Inside the kernel's own flush NOTHING publishes (python
          // `_state_quiet`, CLI-B): the record holds the bound, the next
          // published update carries it.
          const root = this.k.agents.get(this.k.rootId);
          if (root !== undefined) root.meta["next_id"] = this.idReserved;
        } else {
          this.k.updateMeta(this.k.rootId, { next_id: this.idReserved });
        }
      }
    }
    return n;
  }

  /** `load`: THE COUNTER COMES BACK WITH THE GUID — the root record's reserved
   *  bound is where this run starts; this run's first block is reserved in
   *  memory. The record is not edited (save → load → save is identical). */
  resume(nextId: Json | undefined): void {
    if (typeof nextId === "number" && Number.isInteger(nextId) && nextId > 0) {
      this.nextId = Math.max(this.nextId, nextId);
    }
    this.idReserved = this.nextId + ID_BLOCK;
  }

  /**
   * Append one EXECUTION row (`agent` = the agent it is about); mint its id when
   * absent, accept a PRE-MINTED unused one, refuse anything else — the kernel is
   * the only allocator. Bookkeeping, never gated, no send. Wakes the flusher.
   */
  put(agentId: string | null, row: Payload & { kind: string; id?: number }): number {
    let rid = row.id;
    if (rid === undefined) {
      rid = this.mintId();
    } else if (
      !Number.isInteger(rid) || rid <= 0 || rid >= this.nextId || this.rows.has(rid)
    ) {
      throw new Error(`rows_put: id must be absent or a pre-minted unused id, got ${String(rid)}`);
    }
    // python `{**row, id, agent}` then `setdefault("ts", …)`: `ts` is the LAST key
    // unless the row brought its own.
    const stored: Row = { ...row, id: rid, agent: agentId };
    if (!("ts" in stored)) stored["ts"] = nowSeconds();
    this.rows.set(rid, stored);
    if (!this.started && (row.kind === "settle" || row.kind === "ack")) {
      const parent = row["parent"];
      if (typeof parent === "number") {
        const list = this.unflushable.get(parent) ?? [];
        list.push(rid);
        this.unflushable.set(parent, list);
      }
    }
    this.wake();
    return rid;
  }

  private dropEvicted(id: number): void {
    if (this.started) return; // rows leave by flush once started
    this.rows.delete(id);
    for (const rid of this.unflushable.get(id) ?? []) this.rows.delete(rid);
    this.unflushable.delete(id);
  }

  /** Called by `kernel_start` once the store is proven: rows now leave by flush.
   *  Spawns the flusher as a task under `parent` (the `kernel_start` task) — a
   *  FORK IS A MESSAGE: its id is the id of the `task` row written for it. */
  markStarted(storeId: string, parent: number | null): void {
    this.storeId = storeId;
    this.started = true;
    this.unflushable.clear();
    const root = this.k.rootId as string;
    const tid = this.put(root, { kind: "task", verb: "flush_executions", parent });
    this.k.tasks.register(tid, root, "flush_executions", parent, { keepalive: true, forked: true });
    this.flusherTask = tid;
    this.wake();
  }

  /** Stop the flush loop and drop its row (the stop's last step). */
  private endFlusher(): void {
    if (this.flushTimer !== null) {
      timers().clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    if (this.flusherTask !== null) {
      this.k.tasks.done(this.flusherTask);
      this.flusherTask = null;
    }
  }

  // ── settle: the ending of a call is its own row ────────────────────────

  /**
   * Settle an execution with its FINAL outcome and append its `settle` row — a
   * row is append-only, so the ending is an append, never an edit of the call
   * row. The settle keeps what `storedOutcome` keeps. A dead ending (failed /
   * timed_out / cancelled) is ANNOUNCED — `on_deadletter` — because nothing
   * retries it on its own: someone has to look.
   */
  settle(e: Exec, out: Outcome): void {
    if (e.settled) return;
    this.k.ledger.settle(e, out);
    const call = this.rows.get(e.id);
    if (call !== undefined) {
      // the live call row, while it is still in RAM
      const stamp: Payload = { status: out.status };
      const res = out.result;
      if (isErr(res)) {
        stamp["error"] = String(res["error"]).slice(0, 200);
        if (res["reason"]) stamp["reason"] = res["reason"] as Json;
      }
      call["outcome"] = stamp;
    }
    // ON THE ROOT: a settle is KERNEL bookkeeping; `settled` names the target.
    this.put(this.k.rootId, {
      kind: "settle",
      parent: e.id,
      settled: e.target,
      outcome: storedOutcome(out, e.named),
      attempts: out.attempts ?? e.attempts,
    });
    if (DEAD_STATUSES.has(out.status)) this.announceDeadletter(e, out);
  }

  /**
   * `on_deadletter` — an execution settled failed / timed_out / cancelled.
   * Emitted on the ORIGINATING caller's own inbox (and so to whoever watches it,
   * a UI included), or on the root for an operator send. Nothing retries it; the
   * event is how a person learns there is something to decide.
   */
  private announceDeadletter(e: Exec, out: Outcome): void {
    const entry: Payload = {
      type: "on_deadletter",
      id: e.id,
      target: e.target,
      caller: e.caller,
      status: out.status,
      attempts: out.attempts ?? e.attempts,
    };
    if (e.name !== null) entry["name"] = e.name;
    const src = e.caller !== null && this.k.get(e.caller) !== undefined ? e.caller : this.k.rootId;
    if (src !== null) this.k.emit(src, entry);
  }

  // ── the flusher ────────────────────────────────────────────────────────

  /** A row was appended: once started, the flusher writes a batch after a
   *  short quiet — as its own task. */
  private wake(): void {
    if (this.flusherTask === null || this.stopped || this.flushTimer !== null) return;
    const tid = this.flusherTask;
    this.flushTimer = timers().setTimeout(() => {
      this.flushTimer = null;
      if (this.flusherTask === tid) void this.flush(tid);
    }, FLUSH_DEBOUNCE_MS);
  }

  /** Stop the flusher (the graceful stop's last step). */
  halt(): void {
    this.stopped = true;
    this.endFlusher();
  }

  /**
   * Write every execution row still in RAM to the memory store, as ONE batch
   * closed by a self-describing `flush` row; on success the rows leave RAM and a
   * settled execution whose settle row landed leaves the ledger's RAM table (the
   * store is its record now). All or nothing: a refused batch stays in RAM for
   * the next flush, and the refusal is said on the console.
   *
   * The write is KERNEL BOOKKEEPING: the kernel runs the store's `append`
   * directly — not a send, so the flush mints no call or settle row of its own,
   * and the `flush` row is its whole record.
   *
   * One flush at a time (a promise chain is the lock); the retention prune runs
   * after it, outside the chain, as in python. `task` is the task the store's
   * handler runs inside: the flusher's own, or none for a direct flush.
   */
  async flush(task: number | null = null): Promise<Payload> {
    const run = this.flushChain.then(() => this.flushOnce(task));
    this.flushChain = run.catch(() => undefined);
    const out = await run;
    // Only after a batch LANDED (python's early returns skip the prune too).
    const cap = this.k.ledger.cap;
    if (cap !== null && out["flush"] !== undefined && this.storedCalls > cap) {
      out["pruned"] = await this.prune(task);
    }
    return out;
  }

  private async flushOnce(task: number | null): Promise<Payload> {
    if (this.storeId === null) return { flushed: 0 };
    const ids = [...this.rows.keys()].sort((a, b) => a - b);
    if (ids.length === 0) return { flushed: 0 };
    // Nothing of this flush is published — its own id mints (and an id-block
    // reservation they cross) included (CLI-B).
    this.flushing++;
    let fid: number;
    let kinds: Map<number, string>;
    let out;
    try {
      fid = this.mintId();
      kinds = new Map(ids.map((i) => [i, this.rows.get(i)!.kind]));
      const batch: Json[] = ids.map((i) => storeSafe(this.rows.get(i)));
      batch.push({ id: fid, kind: "flush", covers: ids, status: "done", ts: nowSeconds() });
      out = await this.k.bookkeeping(this.storeId, { type: "append", rows: batch }, { task });
    } finally {
      this.flushing--;
    }
    const res = out.result;
    if (out.status !== "completed" || isErr(res)) {
      warn(
        `  [kernel] flush of ${ids.length} execution row(s) to '${this.storeId}' failed: ` +
          `${out.status} ${JSON.stringify(res)}`,
      );
      return { flushed: 0, error: res };
    }
    for (const i of ids) {
      const r = this.rows.get(i);
      this.rows.delete(i);
      if (r?.kind === "settle") {
        const parent = r["parent"] as number;
        if (this.k.ledger.get(parent)?.settled === true) this.k.ledger.forget(parent);
      }
    }
    this.storedCalls += ids.filter((i) => kinds.get(i) === "call").length;
    return { flushed: ids.length, flush: fid };
  }

  /**
   * THE STORE-SIDE RETENTION WINDOW (`ledger.cap`). Over the cap, the OLDEST
   * settled executions leave the kernel's store — unnamed first, then named (a
   * named one carries a dedup contract, so it goes last) — each with its settle
   * and ack rows, and the flush rows that only covered what was pruned. A DEAD
   * LETTER that nobody acked stays: it is waiting for a person. In-flight work is
   * never pruned. HYSTERESIS: down to 90% of the window, so a full store is not
   * re-read and re-pruned on every flush. Returns how many executions went.
   */
  async prune(task: number | null = null): Promise<number> {
    let rows: Row[];
    try {
      rows = await this.read({}, task);
    } catch (e) {
      // never thrown into the flush loop: a store read that fails prunes nothing
      warn(`  [kernel] prune of the kernel's store failed: ${e instanceof Error ? e.message : String(e)}`);
      return 0;
    }
    const calls = rows.filter((r) => r.kind === "call" && !r["internal"]);
    const settles = new Map<unknown, Row>();
    const acked = new Set<unknown>();
    for (const r of rows) {
      if (r.kind === "settle") settles.set(r["parent"], r);
      else if (r.kind === "ack") acked.add(r["parent"]);
    }
    this.storedCalls = calls.length;
    const cap = this.k.ledger.cap ?? 0;
    if (calls.length <= cap) return 0;
    const over = calls.length - (cap >= 10 ? Math.floor((cap * 9) / 10) : cap);
    const prunable = (c: Row): boolean => {
      const st = settles.get(c.id);
      if (st === undefined || this.k.ledger.has(c.id)) return false; // in flight / live in RAM
      const status = (st["outcome"] as Payload | undefined)?.["status"];
      return !DEAD_STATUSES.has(String(status)) || acked.has(c.id);
    };
    const doomedCalls: number[] = [];
    const byId = [...calls].sort((a, b) => a.id - b.id);
    for (const namedPass of [false, true]) {
      for (const c of byId) {
        if (doomedCalls.length >= over) break;
        if (Boolean(c["named"]) === namedPass && prunable(c)) doomedCalls.push(c.id);
      }
    }
    if (doomedCalls.length === 0) return 0;
    const gone = new Set(doomedCalls);
    const doomed = new Set<number>(gone);
    for (const r of rows) {
      if ((r.kind === "settle" || r.kind === "ack") && gone.has(r["parent"] as number)) {
        doomed.add(r.id);
      }
    }
    const kept = calls.filter((c) => !gone.has(c.id)).map((c) => c.id);
    const floor = kept.length > 0 ? Math.min(...kept) : null;
    for (const r of rows) {
      if (r.kind === "flush") {
        const covers = Array.isArray(r["covers"]) ? (r["covers"] as number[]) : [];
        if (covers.every((x) => doomed.has(x) || (floor !== null && x < floor))) doomed.add(r.id);
      } else if (r.kind === "task" && floor !== null && r.id < floor) {
        doomed.add(r.id);
      }
    }
    const out = (await this.k.bookkeeping(
      this.storeId as string,
      { type: "prune", ids: [...doomed].sort((a, b) => a - b) },
      { task },
    )).result;
    if (isErr(out)) {
      warn(`  [kernel] prune of the kernel's store failed: ${JSON.stringify(out)}`);
      return 0;
    }
    this.storedCalls -= gone.size;
    return gone.size;
  }

  /** Every row of the kernel's own key matching `payload`, all pages — read as
   *  kernel bookkeeping (no caller: the store answers the kernel's key), inside
   *  `task` (a `ledger` verb's, a send's recall). A store that refuses or is
   *  unreachable fails fast. */
  async read(payload: Payload, task: number | null = null): Promise<Row[]> {
    const out: Row[] = [];
    let after: Json | undefined;
    for (;;) {
      const q: Payload = { type: "read", ...payload, limit: 1000 };
      if (after !== undefined && after !== null) q["after"] = after;
      const res = (await this.k.bookkeeping(this.storeId as string, q, { task })).result;
      if (res === null || typeof res !== "object" || Array.isArray(res) || isErr(res)) {
        throw new Error(`kernel: memory store read failed: ${JSON.stringify(res)}`);
      }
      out.push(...((res["rows"] as Row[] | undefined) ?? []));
      after = res["next"];
      if (after === null || after === undefined) return out;
    }
  }

  /**
   * THE LEDGER PROJECTION: flushed `call`/`settle`/`ack` rows from the store,
   * overlaid by what is still in RAM (live executions, unflushed rows). Each
   * entry carries its originator as `_caller` for the verb to scope by. The SAME
   * outcome shape flushed or not: what the settle row keeps.
   */
  async ledgerEntries(task: number | null = null): Promise<Entry[]> {
    const entries = new Map<number, Entry>();
    const acked = new Set<number>();
    let rows: Row[] = [];
    if (this.storeId !== null) rows = await this.read({ kinds: ["call", "settle", "ack"] }, task);
    rows = rows.concat(
      [...this.rows.values()].filter((r) => r.kind === "call" || r.kind === "settle" || r.kind === "ack"),
    );
    const settles = new Map<number, Row>();
    for (const r of rows) {
      if (r.kind === "ack") acked.add(r["parent"] as number);
      else if (r.kind === "settle") settles.set(r["parent"] as number, r);
    }
    for (const r of rows) {
      if (r.kind !== "call" || r["internal"]) continue;
      const e: Entry = {
        id: r.id,
        target: (r["agent"] as string | null) ?? null,
        attempts: (r["attempts"] as number | undefined) ?? 1,
        retries: (r["retries"] as number | undefined) ?? 0,
        named: Boolean(r["named"]),
        _caller: (r["caller"] as string | null | undefined) ?? null,
      };
      if (r["name"]) e["name"] = r["name"] as Json;
      const st = settles.get(r.id);
      if (st !== undefined) {
        const oc = (st["outcome"] as Payload | undefined) ?? {};
        e["status"] = oc["status"] ?? null;
        e["attempts"] = (st["attempts"] as number | undefined) || (e["attempts"] as number);
        e["outcome"] = { pid: r.id, ...oc, attempts: e["attempts"] };
      } else {
        e["status"] = "in_flight";
      }
      entries.set(r.id, e);
    }
    for (const [eid, x] of this.k.ledger.executions) {
      const e: Entry = {
        id: eid,
        target: x.target,
        attempts: x.attempts,
        retries: x.retries,
        named: x.named,
        _caller: x.caller,
      };
      if (x.name !== null) e["name"] = x.name;
      if (x.settled && x.outcome !== null) {
        const res = x.outcome;
        e["status"] = res.status;
        e["outcome"] = {
          pid: eid,
          ...storedOutcome(res, x.named),
          attempts: res.attempts ?? x.attempts,
        };
      } else {
        e["status"] = "in_flight";
      }
      if (x.acked) acked.add(eid);
      entries.set(eid, e);
    }
    for (const eid of acked) {
      const e = entries.get(eid);
      if (e !== undefined) e["acked"] = true;
    }
    return [...entries.keys()].sort((a, b) => a - b).map((i) => entries.get(i)!);
  }

  /**
   * NAMED EXECUTIONS OUTLIVE RAM. Bring a flushed execution back into the
   * ledger's RAM table before `send` launches an item that names it — a caller
   * NAME not held in RAM, or an int id not in the table. A settled one comes back
   * holding its recorded outcome (dedup survives the flush and a restart); one
   * with no settle row was in flight when the process stopped and rebuilds
   * nothing — re-sending runs it.
   */
  async recall(items: readonly unknown[], task: number | null = null): Promise<void> {
    const wanted: ["name" | "id", string | number][] = [];
    const ledger = this.k.ledger;
    for (const it of items) {
      if (it === null || typeof it !== "object" || Array.isArray(it)) continue;
      const x = (it as { id?: unknown }).id;
      if (typeof x === "string" && x !== "" && !ledger.names.has(x)) wanted.push(["name", x]);
      else if (typeof x === "number" && Number.isInteger(x) && !ledger.has(x)) wanted.push(["id", x]);
    }
    for (const [kind, key] of wanted) {
      const calls = await this.read(
        kind === "name"
          ? { kinds: ["call"], match: { name: key } }
          : { kinds: ["call"], ids: [key] },
        task,
      );
      const call = calls[calls.length - 1];
      if (call === undefined) continue;
      const settles = await this.read({ kinds: ["settle"], match: { parent: call.id } }, task);
      const st = settles[settles.length - 1];
      if (st === undefined) continue;
      const oc = (st["outcome"] as Payload | undefined) ?? {};
      const attempts =
        (st["attempts"] as number | undefined) || (call["attempts"] as number | undefined) || 1;
      // A NAMED call's settle kept its result; an unnamed one (joined by its
      // int id) kept only what `storedOutcome` keeps — and says so.
      const result: Json = "result" in oc
        ? (oc["result"] as Json)
        : Object.fromEntries(Object.entries(oc).filter(([k]) => k !== "status"));
      const out: Outcome = {
        pid: call.id,
        status: oc["status"] as Outcome["status"],
        result,
        attempts,
      };
      const payload = call["payload"];
      ledger.put({
        id: call.id,
        target: String(call["agent"]),
        verb: String(call["verb"] ?? ""),
        payload: payload !== null && typeof payload === "object" && !Array.isArray(payload) ? payload : {},
        timeout: undefined,
        retries: (call["retries"] as number | undefined) ?? 0,
        attempts,
        named: Boolean(call["named"]),
        name: typeof call["name"] === "string" ? call["name"] : null,
        caller: (call["caller"] as string | null | undefined) ?? null,
        settled: true,
        outcome: out,
        acked: false,
        promise: Promise.resolve(out),
        resolve: () => undefined,
      });
    }
  }
}
