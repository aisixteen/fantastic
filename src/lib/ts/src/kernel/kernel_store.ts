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
// Every piece of state here is shared by reference: a handler's scoped kernel
// view (`Object.create(kernel)`) reaches THIS object through its prototype, so a
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
  /** The BASE kernel (never a scoped view): the store's own sends go out from it. */
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
  // The flush rows this kernel wrote (by id), and the calls that carried them:
  // those calls' own rows are ALREADY RECORDED — the flush row is their record.
  private readonly ownFlushes = new Set<number>();
  private readonly ownFlushCalls = new Map<number, number>(); // call id → flush id
  /** How many `call` rows the store holds for the kernel (re-read at every
   *  prune) — what the retention window is measured against. */
  storedCalls = 0;
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
        this.k.updateMeta(this.k.rootId, { next_id: this.idReserved });
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
    const stored: Row = { ts: nowSeconds(), ...row, id: rid, agent: agentId };
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

  /** Called by `kernel_start` once the store is proven: rows now leave by flush. */
  markStarted(storeId: string): void {
    this.storeId = storeId;
    this.started = true;
    this.unflushable.clear();
    this.wake();
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
      attempts: out.attempts,
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
      attempts: out.attempts,
    };
    if (e.name !== null) entry["name"] = e.name;
    const src = e.caller !== null && this.k.get(e.caller) !== undefined ? e.caller : this.k.rootId;
    if (src !== null) this.k.emit(src, entry);
  }

  // ── the flusher ────────────────────────────────────────────────────────

  /** A row was appended: once started, write a batch after a short quiet. */
  private wake(): void {
    if (!this.started || this.stopped || this.flushTimer !== null) return;
    this.flushTimer = timers().setTimeout(() => {
      this.flushTimer = null;
      void this.flush();
    }, FLUSH_DEBOUNCE_MS);
  }

  /** Stop the debounce (the graceful stop's last step). */
  halt(): void {
    this.stopped = true;
    if (this.flushTimer !== null) {
      timers().clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
  }

  /**
   * A row the kernel's own flush produced — ALREADY RECORDED by its flush row.
   *
   * THE LOOP a flush must not create: a flush is a `send` to the store, so it
   * mints a `call` row and a `settle` row like every send; writing those would
   * mint two more on the next flush, forever. The flush therefore DESCRIBES
   * ITSELF: it mints one id and appends a `flush` row inside the very batch it
   * writes. The call that carried that batch names the flush row in its payload
   * — the store already holds the record of this execution — so its call and
   * settle rows are not written again. This is dedup BY EXECUTION (the store
   * proves it), not a blind spot: every OTHER call to the store, the kernel's
   * own reads included, is recorded like any call.
   *
   * Rows are walked in insertion order, so a flush's call row is always seen
   * (and remembered) before its settle row.
   */
  private isOwnFlushRow(row: Row): boolean {
    if (row.kind === "call" && row["agent"] === this.storeId) {
      const payload = row["payload"];
      if (payload !== null && typeof payload === "object" && !Array.isArray(payload) &&
        payload["type"] === "append" && Array.isArray(payload["rows"])) {
        for (const r of payload["rows"]) {
          if (r !== null && typeof r === "object" && !Array.isArray(r) &&
            r["kind"] === "flush" && typeof r["id"] === "number" && this.ownFlushes.has(r["id"])) {
            this.ownFlushCalls.set(row.id, r["id"]);
            return true;
          }
        }
      }
    }
    return row.kind === "settle" && this.ownFlushCalls.has(row["parent"] as number);
  }

  /**
   * Write every execution row still in RAM to the memory store, as ONE batch
   * closed by a self-describing `flush` row; on success the rows leave RAM and a
   * settled execution whose settle row landed leaves the ledger's RAM table (the
   * store is its record now). All or nothing: a refused batch stays in RAM for
   * the next flush, and the refused flush is itself a dead letter.
   *
   * EDGE (recorded, not resolved): the write can LAND while its reply is lost.
   * The store then holds the flush row as `done` while RAM saw the flush call
   * fail — that call is a dead letter in RAM (the ledger shows it,
   * `on_deadletter` announced it), and its own rows are not re-flushed because
   * the store already recorded them. RAM and store disagree about THAT flush
   * only; the admin reconciles.
   *
   * One flush at a time (a promise chain is the lock); the retention prune runs
   * after it, outside the chain, as in python.
   */
  async flush(): Promise<Payload> {
    const run = this.flushChain.then(() => this.flushOnce());
    this.flushChain = run.catch(() => undefined);
    const out = await run;
    // Only after a batch LANDED (python's early returns skip the prune too).
    const cap = this.k.ledger.cap;
    if (cap !== null && out["flush"] !== undefined && this.storedCalls > cap) {
      out["pruned"] = await this.prune();
    }
    return out;
  }

  private async flushOnce(): Promise<Payload> {
    if (this.storeId === null) return { flushed: 0 };
    const own = [...this.rows.values()].filter((r) => this.isOwnFlushRow(r));
    for (const r of own) {
      this.rows.delete(r.id);
      if (r.kind === "settle") {
        // The flush call ended: its bookkeeping goes. A flush that did not
        // complete stays in the ledger — a dead letter in RAM.
        const parent = r["parent"] as number;
        const fid = this.ownFlushCalls.get(parent);
        this.ownFlushCalls.delete(parent);
        if (fid !== undefined) this.ownFlushes.delete(fid);
        const oc = r["outcome"] as Payload | undefined;
        if (oc?.["status"] === "completed") this.k.ledger.forget(parent);
      }
    }
    const ids = [...this.rows.keys()].sort((a, b) => a - b);
    if (ids.length === 0) return { flushed: 0 };
    const fid = this.mintId();
    this.ownFlushes.add(fid);
    const kinds = new Map(ids.map((i) => [i, this.rows.get(i)!.kind]));
    const batch: Json[] = ids.map((i) => storeSafe(this.rows.get(i)));
    batch.push({ id: fid, kind: "flush", covers: ids, status: "done", ts: nowSeconds() });
    const out = (await this.k.kernelView().send([
      { target: this.storeId, payload: { type: "append", rows: batch } },
    ]))[0]!;
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
  async prune(): Promise<number> {
    const rows = await this.read({});
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
    const out = (await this.k.kernelView().send([
      { target: this.storeId as string, payload: { type: "prune", ids: [...doomed].sort((a, b) => a - b) } },
    ]))[0]!.result;
    if (isErr(out)) {
      warn(`  [kernel] prune of the kernel's store failed: ${JSON.stringify(out)}`);
      return 0;
    }
    this.storedCalls -= gone.size;
    return gone.size;
  }

  /** Every row of the kernel's own key matching `payload`, all pages. A store
   *  that refuses or is unreachable fails fast. */
  async read(payload: Payload): Promise<Row[]> {
    const out: Row[] = [];
    let after: Json | undefined;
    for (;;) {
      const q: Payload = { type: "read", ...payload, limit: 1000 };
      if (after !== undefined && after !== null) q["after"] = after;
      const res = (await this.k.kernelView().send([
        { target: this.storeId as string, payload: q },
      ]))[0]!.result;
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
  async ledgerEntries(): Promise<Entry[]> {
    const entries = new Map<number, Entry>();
    const acked = new Set<number>();
    let rows: Row[] = [];
    if (this.storeId !== null) rows = await this.read({ kinds: ["call", "settle", "ack"] });
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
        e["status"] = x.outcome.status;
        e["outcome"] = {
          pid: eid,
          ...storedOutcome(x.outcome, x.named),
          attempts: x.outcome.attempts,
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
  async recall(items: readonly unknown[]): Promise<void> {
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
      );
      const call = calls[calls.length - 1];
      if (call === undefined) continue;
      const settles = await this.read({ kinds: ["settle"], match: { parent: call.id } });
      const st = settles[settles.length - 1];
      if (st === undefined) continue;
      const oc = (st["outcome"] as Payload | undefined) ?? {};
      const attempts =
        (st["attempts"] as number | undefined) || (call["attempts"] as number | undefined) || 1;
      // A NAMED call's settle kept its result; an unnamed one (joined by its int
      // id) kept only what `storedOutcome` keeps — and says so.
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
