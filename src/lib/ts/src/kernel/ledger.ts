// The EXECUTION LEDGER (#34/#35/#36), PROCESS SIDE — ensure-semantics for sends.
//
// A batch item's optional `id` NAMES the execution, exactly the way a supplied
// `create_agent` id names an agent:
//
//   unknown  ⇒ dispatch and record
//   in flight ⇒ JOIN the running one — no second execution ever starts
//   settled  ⇒ the RECORDED OUTCOME returns and the handler never runs
//
// The name is the CALLER's; the execution's id is the kernel's one counter (an
// int), and the outcome carries both (`pid`, `name`). An int item id joins that
// execution by id and never creates one.
//
// This table holds only what is still in RAM: executions in flight, or settled
// and not yet flushed. A flushed execution lives on as its `call` + `settle` rows
// in the kernel's memory store (`kernel_store.ts`): `send` looks a name up there
// before launching, so dedup survives the flush and a restart, and the `ledger`
// verb is a PROJECTION over the store plus this table.
//
// Retention in RAM is a RING (`cap`, default 1,000,000): evict oldest SETTLED
// entries, unnamed before named (a named one is a dedup CONTRACT), in-flight
// never — what matters for a kernel that was never started and so never flushes.

import type { Json, Payload } from "./json.ts";
import type { Outcome } from "./outcome.ts";
import { DEAD_STATUSES } from "./outcome.ts";

export interface Exec {
  id: number;
  target: string;
  /** The VERB this execution ran. A joiner is handed its outcome verbatim, so
   *  it must be authorized for THIS verb — not for whatever it happened to
   *  send. Kept on the entry because the entry outlives the dispatch. */
  verb: string;
  payload: Payload;
  timeout: number | undefined;
  retries: number;
  attempts: number;
  named: boolean;
  name: string | null;
  /** The id that ORIGINATED this execution; null = operator. Scopes `ledger`. */
  caller: string | null;
  settled: boolean;
  outcome: Outcome | null;
  acked: boolean;
  /** Resolves with the ONE true outcome. Joiners await exactly this. */
  promise: Promise<Outcome>;
  resolve: (o: Outcome) => void;
}

export interface ExecInit {
  id: number;
  target: string;
  verb: string;
  payload: Payload;
  timeout: number | undefined;
  retries: number;
  name: string | null;
  caller: string | null;
}

export const DEFAULT_LEDGER_CAP = 1_000_000;

export class ExecutionLedger {
  /** Insertion-ordered (JS Map guarantees it) — the ring walks oldest first. */
  readonly executions = new Map<number, Exec>();
  /** Caller NAMES → message id, while the execution is in RAM. */
  readonly names = new Map<string, number>();
  /** null = unbounded (the operator's explicit choice). In the store, the same
   *  number is the prune window. */
  cap: number | null = DEFAULT_LEDGER_CAP;
  /** Told when the RING evicts an entry (not when a flush retires one) — the
   *  kernel drops the rows only an unstarted kernel still holds for it. */
  onEvict: ((id: number) => void) | null = null;

  get(id: number): Exec | undefined {
    return this.executions.get(id);
  }

  has(id: number): boolean {
    return this.executions.has(id);
  }

  /** The message id a caller's name stands for, while its execution is held. */
  resolveName(name: string): number | undefined {
    return this.names.get(name);
  }

  /** Open an execution under a pre-minted id. The caller validated `retries`. */
  open(init: ExecInit): Exec {
    let resolve!: (o: Outcome) => void;
    const promise = new Promise<Outcome>((res) => {
      resolve = res;
    });
    const e: Exec = {
      ...init,
      named: init.name !== null,
      attempts: 1,
      settled: false,
      outcome: null,
      acked: false,
      promise,
      resolve,
    };
    this.put(e);
    return e;
  }

  /** Insert one entry (a fresh one, or one recalled from the store), then
   *  enforce the ring. */
  put(e: Exec): void {
    this.executions.set(e.id, e);
    if (e.name !== null) this.names.set(e.name, e.id);
    this.enforceRing();
  }

  /** Settle an entry with its FINAL outcome. Intermediate retry attempts never
   *  reach here — a joiner must not observe an attempt that was retried away. */
  settle(e: Exec, out: Outcome): void {
    if (e.settled) return;
    e.settled = true;
    e.outcome = out;
    e.resolve(out);
  }

  /** Drop one entry and its name (the store is its record now, or the ring let
   *  it go). */
  forget(id: number): void {
    const e = this.executions.get(id);
    if (e === undefined) return;
    this.executions.delete(id);
    if (e.name !== null && this.names.get(e.name) === id) this.names.delete(e.name);
  }

  /** Evict oldest SETTLED entries until the cap holds: unnamed first, then named.
   *  In-flight entries are NEVER evicted — their joiners are still waiting. */
  private enforceRing(): void {
    const cap = this.cap;
    if (cap === null || this.executions.size <= cap) return;
    for (const namedPass of [false, true]) {
      if (this.executions.size <= cap) return;
      for (const [id, e] of [...this.executions]) {
        if (e.named !== namedPass || !e.settled) continue;
        this.forget(id);
        this.onEvict?.(id);
        if (this.executions.size <= cap) return;
      }
    }
  }

  size(): number {
    return this.executions.size;
  }
}

// ─── what the store keeps ─────────────────────────────────────────────

/** Raw bytes as the store and a JSON wire hold them: `{len: N}` — the one
 *  reduction (base64 is banned); everything else passes through, copied. */
export function storeSafe(o: unknown): Json {
  if (o instanceof ArrayBuffer) return { len: o.byteLength };
  if (ArrayBuffer.isView(o)) return { len: o.byteLength };
  if (Array.isArray(o)) return o.map(storeSafe);
  if (o !== null && typeof o === "object") {
    const out: { [k: string]: Json } = {};
    for (const [k, v] of Object.entries(o)) {
      if (v !== undefined) out[k] = storeSafe(v);
    }
    return out;
  }
  if (o === undefined) return null;
  return o as Json;
}

/**
 * The length of python's `json.dumps(o)` — default separators (`", "`, `": "`)
 * and ASCII escaping — so a settle digest's `bytes` agrees with the host
 * runtimes (rust measures the same way). A digest, not a codec.
 */
export function pyJsonLen(o: Json): number {
  return pyJson(o).length;
}

function pyJson(o: Json): string {
  if (o === null) return "null";
  if (typeof o === "string") {
    return JSON.stringify(o).replace(
      /[\u0080-￿]/g,
      (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, "0")}`,
    );
  }
  if (typeof o !== "object") return JSON.stringify(o);
  if (Array.isArray(o)) return `[${o.map(pyJson).join(", ")}]`;
  return `{${Object.entries(o).map(([k, v]) => `${pyJson(k)}: ${pyJson(v)}`).join(", ")}}`;
}

/**
 * WHAT A SETTLE ROW KEEPS of an outcome — the one definition (decided 2026-09-26).
 *
 * A NAMED call keeps its whole result: its name is a dedup contract, and a re-send
 * returns the recorded outcome. A DEAD LETTER keeps its error text: someone has to
 * read why it died. Every other settle keeps a digest — `{status, bytes}`.
 *
 * Why not the body everywhere: the kernel's own store reads are executions too
 * (the `ledger` projection, an AI agent's head read), so a settle holding its body
 * re-recorded the store inside the store — measured in python ~6x growth per
 * `ledger` call (2 KB → 14.8 MB in five). And a reply body is not persisted
 * anyway: its receiver holds it in RAM with the one-line `origin` that gets it
 * again.
 */
export function storedOutcome(out: Outcome, named: boolean): Payload {
  const res = out.result;
  if (named) return { status: out.status, result: storeSafe(res) };
  if (DEAD_STATUSES.has(out.status)) {
    const kept: Payload = { status: out.status };
    if (res !== null && typeof res === "object" && !Array.isArray(res)) {
      for (const k of ["error", "reason", "side", "cancelled", "timed_out"]) {
        if (k in res) {
          const v = (res as Payload)[k] as Json;
          kept[k] = typeof v === "string" ? v.slice(0, 500) : v;
        }
      }
    }
    return kept;
  }
  return { status: out.status, bytes: pyJsonLen(storeSafe(res)) };
}
