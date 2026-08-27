// The EXECUTION LEDGER (#34/#35/#36) — ensure-semantics for sends.
//
// A batch item's optional `id` NAMES the execution, exactly the way a supplied
// `create_agent` id names an agent:
//
//   unknown  ⇒ dispatch and record
//   in flight ⇒ JOIN the running one — no second execution ever starts
//   settled  ⇒ the RECORDED OUTCOME returns and the handler never runs
//
// That is what makes a double-click, a racing tab, or a reconnect-retry safe:
// reusing an id DECLARES "same intent". EVERY execution is ledgered, unnamed
// ones under their minted pid — one id space, one engine.
//
// Retention is a RING (`cap`, default 1,000,000): evict oldest SETTLED rows,
// unnamed before named (a named row is a dedup CONTRACT), in-flight never.
//
// The `ledger` verb is ORIGIN-SCOPED: an agent sees only executions it
// originated; an operator sees all. Unscoped it is a read side-channel around
// `callable_by` — the same class of hole as #39's ungated `watch`.

import type { Json } from "./json.ts";
import type { LedgerRow, Outcome, Status } from "./outcome.ts";

export interface Row {
  id: string;
  target: string;
  named: boolean;
  /** The id that ORIGINATED this execution; null = operator. */
  origin: string | null;
  retries: number;
  attempts: number;
  settled: boolean;
  outcome: Outcome | null;
  /** Resolves with the ONE true outcome. Joiners await exactly this. */
  promise: Promise<Outcome>;
  resolve: (o: Outcome) => void;
}

export const DEFAULT_LEDGER_CAP = 1_000_000;

export class ExecutionLedger {
  /** Insertion-ordered (JS Map guarantees it) — the ring walks oldest first. */
  private readonly rows = new Map<string, Row>();
  /** null = unbounded (the `--ledger-cap inf` escape hatch). */
  cap: number | null = DEFAULT_LEDGER_CAP;
  /** Called at every settle; the substrate itself never touches storage. */
  flusher: (() => void) | null = null;

  get(id: string): Row | undefined {
    return this.rows.get(id);
  }

  has(id: string): boolean {
    return this.rows.has(id);
  }

  /** Open a row and return it. The caller has already validated `retries`. */
  open(
    id: string,
    target: string,
    origin: string | null,
    retries: number,
    named: boolean,
  ): Row {
    let resolve!: (o: Outcome) => void;
    const promise = new Promise<Outcome>((res) => {
      resolve = res;
    });
    const row: Row = {
      id, target, named, origin, retries,
      attempts: 0, settled: false, outcome: null, promise, resolve,
    };
    this.rows.set(id, row);
    this.enforceRing();
    return row;
  }

  /** Settle a row with its FINAL outcome. Intermediate retry attempts never
   *  reach here — a joiner must not observe an attempt that was retried away. */
  settle(row: Row, out: Outcome): void {
    if (row.settled) return;
    row.settled = true;
    row.outcome = out;
    row.resolve(out);
    if (this.flusher !== null) this.flusher();
  }

  /** Evict oldest SETTLED rows until the cap holds: unnamed first, then named.
   *  In-flight rows are NEVER evicted — their joiners are still waiting. */
  private enforceRing(): void {
    const cap = this.cap;
    if (cap === null || this.rows.size <= cap) return;
    for (const namedPass of [false, true]) {
      if (this.rows.size <= cap) return;
      for (const [id, r] of [...this.rows]) {
        if (r.named !== namedPass) continue;
        if (!r.settled) continue;
        this.rows.delete(id);
        if (this.rows.size <= cap) return;
      }
    }
  }

  /**
   * Rows visible to `origin` — an agent sees only what IT originated; an
   * operator (`null`) sees everything.
   */
  visibleTo(origin: string | null): LedgerRow[] {
    const out: LedgerRow[] = [];
    for (const r of this.rows.values()) {
      if (origin !== null && r.origin !== origin) continue;
      const row: LedgerRow = {
        id: r.id,
        target: r.target,
        status: r.settled ? (r.outcome?.status ?? "completed") : "in_flight",
        attempts: r.attempts,
        retries: r.retries,
        named: r.named,
      };
      if (r.outcome !== null) row["outcome"] = r.outcome.result as Json;
      out.push(row);
    }
    return out;
  }

  /** Settled rows only — the durable half persists nothing in flight, because
   *  work interrupted by a crash never completed. */
  settledSnapshot(): { v: number; rows: { [id: string]: Json } } {
    const rows: { [id: string]: Json } = {};
    for (const [id, r] of this.rows) {
      if (!r.settled || r.outcome === null) continue;
      rows[id] = {
        target: r.target,
        named: r.named,
        origin: r.origin,
        attempts: r.attempts,
        status: r.outcome.status,
        result: r.outcome.result,
      };
    }
    return { v: 1, rows };
  }

  /** Restore settled rows at boot so dedup holds across a reload. Returns the
   *  count restored; an unknown version is the CALLER's failfast, not ours. */
  rehydrate(doc: { v?: number; rows?: { [id: string]: Json } }): number {
    const rows = doc.rows ?? {};
    let n = 0;
    for (const [id, raw] of Object.entries(rows)) {
      if (raw === null || typeof raw !== "object" || Array.isArray(raw)) continue;
      const r = raw as { [k: string]: Json };
      const target = typeof r["target"] === "string" ? r["target"] : "";
      if (target === "") continue;
      const status = (typeof r["status"] === "string" ? r["status"] : "completed") as Status;
      const origin = typeof r["origin"] === "string" ? r["origin"] : null;
      const row = this.open(id, target, origin, 0, r["named"] === true);
      row.attempts = typeof r["attempts"] === "number" ? r["attempts"] : 1;
      this.settle(row, {
        pid: id, status, result: r["result"] ?? null, attempts: row.attempts,
      });
      n++;
    }
    return n;
  }

  /** Test/inspection seam. */
  size(): number {
    return this.rows.size;
  }
}
