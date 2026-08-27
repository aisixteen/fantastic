// THE BATCH SEND (#29) — `send([Item]) -> [Outcome]`, the kernel's one dispatch
// path, plus the ledger's ensure-semantics (#34/#35/#36) layered on it.
//
// There is NO single-call sugar and its ABSENCE is the feature: with two entry
// points the single one drifts until the two disagree about what a send is.
//
// Shape of a batch:
//   1. SYNCHRONOUS LAUNCH — every item validated, resolved, gated and given its
//      ledger row and pid BEFORE the first await, so `ps` shows the whole batch
//      at once rather than one row appearing at a time.
//   2. CONCURRENT SETTLE — all items race; the returned array preserves INPUT
//      order regardless of completion order.
//
// `timeout` is per item and the kernel NEVER defaults it (absent = wait
// forever): a default would silently cap somebody else's legitimately slow
// handler.

import type { Json, Payload } from "./json.ts";
import type { Kernel } from "./kernel.ts";
import type { Item, Outcome, Status } from "./outcome.ts";
import type { Row } from "./ledger.ts";
import { refused } from "./outcome.ts";
import { BAD_ARG, refusal, missing, unknown } from "./refusal.ts";
import { authorizeCall, denial } from "./authz.ts";

/** An item that reached the ledger and now needs settling. */
interface Launched {
  index: number;
  /** Immediate outcome for anything that never reached the table. */
  settled?: Outcome;
  row?: Row;
  pid?: string | null;
  target?: string;
  payload?: Payload;
  timeout?: number | undefined;
  /** True when this item JOINED an existing execution instead of starting one. */
  joined?: boolean;
}

function isPlainObject(v: unknown): v is Payload {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

/**
 * Validate one item and open its ledger row. Runs entirely SYNCHRONOUSLY —
 * nothing here may await, or the "whole batch visible in ps" property breaks.
 */
export function launch(k: Kernel, index: number, item: Item): Launched {
  if (!isPlainObject(item as unknown)) {
    return { index, settled: refused(refusal(BAD_ARG, "send: item must be an object", { extra: { arg: "item" } })) };
  }
  const targetId = item.target;
  const payload = item.payload;
  if (typeof targetId !== "string" || targetId === "") {
    return { index, settled: refused(missing("send", "target")) };
  }
  if (!isPlainObject(payload)) {
    return { index, settled: refused(missing("send", "payload")) };
  }
  const timeout = item.timeout;
  if (
    timeout !== undefined &&
    (typeof timeout !== "number" || !Number.isFinite(timeout) || timeout <= 0)
  ) {
    return {
      index,
      settled: refused(
        refusal(BAD_ARG, `send: timeout must be a positive number of seconds, got ${JSON.stringify(timeout)}`, { extra: { arg: "timeout" } }),
      ),
    };
  }

  const resolvedId = k.resolveId(targetId);
  const target = k.get(resolvedId);
  // A target this kernel does not hold is FORWARDED over the bridge — that is
  // federation, not an error. Only a missing target with no bridge is unknown.
  if (target === undefined && k.bridge === null) {
    return { index, settled: refused(unknown(resolvedId, [...k.agents.keys()])) };
  }

  // ── CALL GATE — fully gated, two-sided, from above ───────────────
  // An OPERATOR send (no origin) is the root of trust and never reaches here.
  // Every AGENT send — ANY verb, `reflect` included — needs BOTH ends.
  const origin = k.currentOrigin;
  if (origin !== null && target !== undefined) {
    const verb = typeof payload["type"] === "string" ? payload["type"] : "";
    const d = k.gate(origin, verb, resolvedId);
    if (d !== null) return { index, settled: refused(d) };
  }

  const retries = item.retries ?? 0;
  if (typeof retries !== "number" || !Number.isInteger(retries) || retries < 0) {
    return {
      index,
      settled: refused(
        refusal(BAD_ARG, `send: retries must be a non-negative integer, got ${JSON.stringify(retries)}`, { extra: { arg: "retries" } }),
      ),
    };
  }

  const execId = item.id;
  if (execId !== undefined) {
    if (typeof execId !== "string" || execId === "") {
      return {
        index,
        settled: refused(
          refusal(BAD_ARG, `send: id must be a non-empty string, got ${JSON.stringify(execId)}`, { extra: { arg: "id" } }),
        ),
      };
    }
    if (execId.includes(":") && !k.ledger.has(execId)) {
      // The minted namespace: a colon id NAMES an existing execution (re-fetch
      // or join by pid) and never CREATES one — the kernel is the only
      // allocator there.
      return {
        index,
        settled: refused(
          refusal(BAD_ARG, `send: id '${execId}' is in the kernel-minted pid namespace (':') and names no known execution — minted ids can be referenced, never created`, { extra: { arg: "id" } }),
        ),
      };
    }
    const existing = k.ledger.get(execId);
    if (existing !== undefined) {
      if (existing.target !== resolvedId) {
        // The id is BOUND to its first execution's target — handing back an
        // unrelated agent's outcome would be silent corruption.
        return {
          index,
          settled: refused(
            refusal(BAD_ARG, `send: id '${execId}' already names an execution on '${existing.target}', not '${resolvedId}' — an id is bound to its first execution; a retry aimed elsewhere needs a new id`, { extra: { arg: "id" } }),
          ),
        };
      }
      // JOIN — settled or in flight, the ledger row carries the ONE true
      // outcome and no second execution ever starts.
      return { index, row: existing, joined: true, timeout, pid: execId };
    }
    const row = k.ledger.open(execId, resolvedId, k.currentOrigin, retries, true);
    return { index, row, pid: execId, target: resolvedId, payload, timeout };
  }

  // UNNAMED work is ledgered too, under its minted pid: one id space, one
  // engine — any reply re-fetchable and joinable by pid, and the ring bounds
  // what that history costs.
  const pid = k.tasks.mint(resolvedId);
  const row = k.ledger.open(pid, resolvedId, k.currentOrigin, retries, false);
  return { index, row, pid, target: resolvedId, payload, timeout };
}

/** Await one launched item to its final outcome, applying the retry budget. */
export async function settle(k: Kernel, l: Launched): Promise<Outcome> {
  if (l.settled !== undefined) return l.settled;
  const row = l.row as Row;

  if (l.joined === true) {
    // A joiner's `timeout` bounds only ITS OWN wait — it does not own the
    // execution, so expiring here leaves the real one running.
    if (l.timeout !== undefined) {
      const raced = await withTimeout(row.promise, l.timeout);
      if (raced === TIMED_OUT) {
        return { pid: row.id, status: "timed_out", result: { error: `join timed out after ${l.timeout}s`, reason: "timeout" }, attempts: row.attempts };
      }
      return raced;
    }
    return row.promise;
  }

  let attempt = 0;
  for (;;) {
    attempt += 1;
    row.attempts = attempt;
    const out = await runOnce(k, l, row, attempt);
    // `completed` never retries; `cancelled` is ABSORBING — an operator kill
    // stops the loop rather than being re-dispatched over.
    const retryable = out.status === "failed" || out.status === "timed_out";
    if (!retryable || attempt > row.retries) {
      k.ledger.settle(row, out);
      return out;
    }
    // Only the FINAL outcome settles the row: a joiner must never observe an
    // attempt that was retried away.
  }
}

const TIMED_OUT = Symbol("timed_out");

// Timers are a PLATFORM capability, and the pure kernel compiles without the
// DOM lib (src/kernel/tsconfig.json), so it reads them off globalThis
// structurally — the same guarded shape the tree-load warning uses for
// `console`. Both browsers and node provide them.
interface Timers {
  setTimeout: (fn: () => void, ms: number) => unknown;
  clearTimeout: (h: unknown) => void;
}

function timers(): Timers {
  const g = globalThis as unknown as Partial<Timers>;
  if (typeof g.setTimeout !== "function" || typeof g.clearTimeout !== "function") {
    // NO FALLBACK. Silently ignoring the bound would turn "wait at most 5s"
    // into "wait forever" — the exact stall the per-item timeout exists to
    // prevent — so a runtime without timers fails LOUDLY, and only for the
    // callers that actually asked for a bound.
    throw new Error(
      "send: this runtime provides no setTimeout/clearTimeout, so a per-item " +
        "`timeout` cannot be honoured; omit it to wait indefinitely",
    );
  }
  return g as Timers;
}

function withTimeout<T>(p: Promise<T>, seconds: number): Promise<T | typeof TIMED_OUT> {
  const { setTimeout: set, clearTimeout: clear } = timers();
  return new Promise((resolve) => {
    const t = set(() => resolve(TIMED_OUT), seconds * 1000);
    p.then(
      (v) => { clear(t); resolve(v); },
      () => { clear(t); resolve(TIMED_OUT); },
    );
  });
}

async function runOnce(k: Kernel, l: Launched, row: Row, attempt: number): Promise<Outcome> {
  const pid = l.pid as string;
  const target = l.target as string;
  const payload = l.payload as Payload;
  const verb = typeof payload["type"] === "string" ? payload["type"] : "";

  const task = k.tasks.register(pid, target, verb, k.currentTask);
  const run = k.dispatch(target, payload, pid);
  try {
    const result = l.timeout === undefined
      ? await run
      : await withTimeout(run, l.timeout);

    if (result === TIMED_OUT) {
      // Expiry goes through the FRONT DOOR so every `on_cancel` hook runs
      // before the abort — an expiring call must unwind like any other cancel.
      k.tasks.cancel(pid);
      return { pid, status: "timed_out", result: { error: `timed out after ${l.timeout}s`, reason: "timeout" }, attempts: attempt };
    }
    if (task.killed) {
      return { pid, status: "cancelled", result: { cancelled: true }, attempts: attempt };
    }
    return { pid, status: "completed", result: result as Json, attempts: attempt };
  } catch (err) {
    // A handler THROWING settles as a failed outcome — `send` itself raises
    // nothing. A thrown error that propagated would kill every sibling item in
    // the batch for one bad handler.
    const msg = err instanceof Error ? err.message : String(err);
    return { pid, status: "failed", result: { error: msg, raised: true }, attempts: attempt };
  } finally {
    k.tasks.done(pid);
  }
}

/** Assemble the batch: sync launch for all, then concurrent settle. */
export async function runBatch(k: Kernel, items: Item[]): Promise<Outcome[]> {
  const launched = items.map((it, i) => launch(k, i, it));
  const settled = await Promise.all(launched.map((l) => settle(k, l)));
  return settled;
}

export type { Status };
