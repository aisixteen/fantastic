// THE BATCH SEND (#29) — `send([Item]) -> [Outcome]`, the kernel's one dispatch
// path, plus the ledger's ensure-semantics (#34/#35/#36) layered on it.
//
// There is NO single-call sugar and its ABSENCE is the feature: with two entry
// points the single one drifts until the two disagree about what a send is.
//
// Shape of a batch:
//   0. RECALL — a started kernel first brings back, from its memory store, any
//      flushed execution an item NAMES, so dedup survives the flush + a restart.
//   1. SYNCHRONOUS LAUNCH — every item validated, resolved, gated and given its
//      message id, its `call` row and its ledger entry BEFORE the first await,
//      so `ps` shows the whole batch at once.
//   2. CONCURRENT SETTLE — all items race; the returned array preserves INPUT
//      order regardless of completion order. The ending is a `settle` row.
//
// `timeout` is per item and the kernel NEVER defaults it (absent = wait
// forever): a default would silently cap somebody else's legitimately slow
// handler.

import type { Json, Payload } from "./json.ts";
import type { Agent } from "./agent.ts";
import type { Kernel } from "./kernel.ts";
import type { Item, Outcome } from "./outcome.ts";
import type { Exec } from "./ledger.ts";
import { errorShaped, refused } from "./outcome.ts";
import { BAD_ARG, DRAINING, TIMEOUT, refusal, missing, unknown } from "./refusal.ts";
import { isKernelVerb } from "./kernel_verbs.ts";
import { applyReflectFlags, reflectIdentity } from "./reflect.ts";
import { pyRepr, pyTypeNameAt } from "./pyjson.ts";
import { timers } from "./timers.ts";

/** An item that reached the ledger and now needs settling. */
interface Launched {
  index: number;
  /** Immediate outcome for anything that never reached the table. */
  settled?: Outcome;
  exec?: Exec;
  target?: string;
  payload?: Payload;
  timeout?: number | undefined;
  /** True when this item JOINED an existing execution instead of starting one. */
  joined?: boolean;
  /** The `kernel` reflect FAST PATH (python `_launch`): a pure identity read —
   *  no dispatch, no task row. Unnamed it has no pid, no rows and no ledger
   *  entry at all; named it is ledgered (dedup + join) but still has no task. */
  identityRead?: boolean;
}

function isPlainObject(v: unknown): v is Payload {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

/**
 * Validate one item, mint its message id, write its `call` row and open its
 * ledger entry. Runs entirely SYNCHRONOUSLY — nothing here may await, or the
 * "whole batch visible in ps" property breaks.
 */
export function launch(k: Kernel, index: number, item: Item): Launched {
  if (!isPlainObject(item as unknown)) {
    return { index, settled: refused(refusal(BAD_ARG, "send: item must be a dict", { extra: { arg: "item" } })) };
  }
  const targetId = item.target;
  const payload = item.payload;
  if (typeof targetId !== "string" || targetId === "") {
    return { index, settled: refused(missing("send", "target")) };
  }
  if (!isPlainObject(payload)) {
    return { index, settled: refused(missing("send", "payload")) };
  }
  // An explicit null is ABSENT, as python's `item.get(...)` reads it — for
  // `timeout` and `id` alike (not for `retries`, below).
  const timeout = item.timeout ?? undefined;
  if (
    timeout !== undefined &&
    (typeof timeout !== "number" || !Number.isFinite(timeout) || timeout <= 0)
  ) {
    return {
      index,
      settled: refused(
        refusal(BAD_ARG, `send: timeout must be a positive number of seconds, got ${pyRepr(timeout)}`, { extra: { arg: "timeout" } }),
      ),
    };
  }
  // `type` IS the verb; there is no send without one. Validated for EVERY sender
  // BEFORE the gate, so a typeless payload is a coded refusal rather than an
  // `unhandled` from whatever the target happens to be (2026-09-02; the shared
  // C21 corpus pins this on all four runtimes).
  if (!("type" in payload)) {
    return { index, settled: refused(missing("send", "payload.type")) };
  }
  const verb = payload["type"];
  if (typeof verb !== "string" || verb === "") {
    // A non-string is named by its TYPE, never echoed: a binary frame can put a
    // whole body here, and its repr made a 16 MiB body cost ~80 MiB of refusal
    // text (audit 2026-10-03, W-11).
    const got = typeof verb === "string" ? pyRepr(verb) : pyTypeNameAt(payload, "type");
    return {
      index,
      settled: refused(
        refusal(BAD_ARG, `send: payload.type must be a non-empty string, got ${got}`, { extra: { arg: "payload.type" } }),
      ),
    };
  }

  // The item's OWN fields are validated before anything is resolved, gated or
  // minted. A STRING id is the caller's NAME for the execution; an INT id is a
  // message id the kernel minted and can only JOIN — the kernel is the only
  // allocator.
  const execId = item.id ?? undefined;
  let joinId: number | undefined;
  let name: string | null = null;
  if (execId !== undefined) {
    if (!(typeof execId === "string" || (typeof execId === "number" && Number.isInteger(execId)))) {
      return {
        index,
        settled: refused(
          refusal(BAD_ARG, `send: id must be a name (non-empty string) or a message id (int), got ${pyRepr(execId)}`, { extra: { arg: "id" } }),
        ),
      };
    }
    if (typeof execId === "number") {
      if (!k.ledger.has(execId)) {
        return {
          index,
          settled: refused(
            refusal(BAD_ARG, `send: id ${execId} names no known execution — ids are minted by the kernel and can be joined, never created`, { extra: { arg: "id" } }),
          ),
        };
      }
      joinId = execId;
    } else {
      if (execId === "") {
        return { index, settled: refused(refusal(BAD_ARG, "send: id must be a non-empty name", { extra: { arg: "id" } })) };
      }
      name = execId;
      joinId = k.ledger.resolveName(name);
    }
  }
  // `retries` absent is 0; an explicit null is NOT (python `item.get("retries", 0)`
  // is None, and None is refused).
  const retries: unknown = item.retries === undefined ? 0 : item.retries;
  if (typeof retries !== "number" || !Number.isInteger(retries) || retries < 0) {
    return {
      index,
      settled: refused(
        refusal(BAD_ARG, `send: retries must be a non-negative int, got ${pyRepr(retries)}`, { extra: { arg: "retries" } }),
      ),
    };
  }
  // A DYING TASK REGISTERS NO NEW WORK. `cancel` marks the whole subtree before
  // anything settles, and JS cannot abort a handler — so a killed handler runs
  // on, and what it sends from then on would start fresh work that outlives the
  // cancel. Its row (this attempt's, even after it left the table) says killed.
  const own = k.currentTaskRow;
  if (own !== null && own.killed) {
    return { index, settled: { pid: null, status: "cancelled", result: { cancelled: true } } };
  }

  // WHILE DRAINING, NO NEW ROOTS. A send from inside live work (a parent task in
  // the table) passes; what a `shutdown` the stop runs sends passes (its view is
  // marked `kernelTraffic`); the root's control surface (kernel verbs + reflect)
  // stays open so an operator can watch and finish the stop; everything else is
  // a new root and is refused before it costs an id.
  const resolvedId = k.resolveId(targetId);
  if (k.store.draining && !k.kernelTraffic) {
    const toRoot = targetId === "kernel" || (k.rootId !== null && targetId === k.rootId);
    const parentLive = k.currentTask !== null && k.tasks.has(k.currentTask);
    if (!(toRoot && (isKernelVerb(verb) || verb === "reflect")) && !parentLive) {
      return {
        index,
        settled: refused(
          refusal(DRAINING, "send: the kernel is draining for shutdown — no new work is accepted; in-flight work may finish"),
        ),
      };
    }
  }

  const target = k.get(resolvedId);
  // A target this kernel does not hold is FORWARDED over the bridge — that is
  // federation, not an error. A missing target with no bridge is unknown, and so
  // is `<this kernel's guid>|x` for an `x` this kernel lacks: it names a LOCAL
  // agent, never one on the host.
  if (target === undefined && (k.bridge === null || k.ownQualified(targetId) !== null)) {
    return { index, settled: refused(unknown(targetId, [...k.agents.keys()])) };
  }

  // ── CALL GATE — two-sided, explicit grants only ───────────────────
  // An OPERATOR send (no origin) is the root of trust and never reaches here.
  // Every AGENT send — ANY verb, `reflect` included — needs BOTH ends. A
  // HOST-BOUND send (no local target: it will be forwarded) passes the SENDER's
  // half here, before the bridge sees it; the target's half is the host's, on
  // the call as it arrives there.
  const origin = k.currentOrigin;
  if (origin !== null) {
    const d = target !== undefined ? k.gate(origin, verb, resolvedId) : k.egressGate(origin, verb, resolvedId);
    if (d !== null) return { index, settled: refused(d) };
  }

  if (joinId !== undefined) {
    const existing = k.ledger.get(joinId);
    if (existing !== undefined) {
      if (existing.target !== resolvedId) {
        // The id is BOUND to its first execution's target — handing back an
        // unrelated agent's outcome would be silent corruption.
        return {
          index,
          settled: refused(
            refusal(BAD_ARG, `send: id ${pyRepr(execId)} already names an execution on ${pyRepr(existing.target)}, not ${pyRepr(resolvedId)} — an id is bound to its first execution; a retry aimed elsewhere needs a new id`, { extra: { arg: "id" } }),
          ),
        };
      }
      // AUTHORIZED AGAINST THE EXECUTION IT JOINS. The recorded outcome belongs
      // to whoever ran it, so handing it back is equivalent to having made THAT
      // call — the gate must be asked that question, not about the verb this
      // caller happened to send. Measured in python 2026-08-28: a caller granted
      // only a harmless verb reused another's id and received its result
      // verbatim. An OPERATOR joiner is ungated, as everywhere.
      if (origin !== null) {
        const d = k.gate(origin, existing.verb, resolvedId);
        if (d !== null) return { index, settled: refused(d) };
      }
      // JOIN — settled or in flight, the ledger entry carries the ONE true
      // outcome and no second execution ever starts.
      return { index, exec: existing, joined: true, timeout };
    }
  }

  // THE `kernel` REFLECT FAST PATH — a pure identity read, applied AFTER the
  // gate so it is authorized like any other reflect. Nothing to cancel, so no
  // task row; unnamed, not even an id.
  const identityRead = targetId === "kernel" && verb === "reflect" && target !== undefined;
  if (identityRead && name === null) {
    return { index, target: resolvedId, payload, timeout, identityRead };
  }

  // NEW: the message id is minted like any other; a name is a field on it.
  // UNNAMED work is ledgered too, under its minted id: one id space, one engine.
  const pid = k.mintId();
  const exec = openExecution(k, pid, resolvedId, verb, payload, timeout, retries, name);
  return { index, exec, target: resolvedId, payload, timeout, identityRead };
}

/**
 * Open one execution under a pre-minted id: the CALL ROW (what outlives RAM, once
 * flushed) and the process side (the ledger entry). EVERY SEND IS A MESSAGE: the
 * row names the call — who asked whom what, from which task — and carries the
 * dispatch payload verbatim, the retry budget and the deadline. The caller field is `caller` (the id that
 * originated it; null = operator).
 */
function openExecution(
  k: Kernel,
  pid: number,
  target: string,
  verb: string,
  payload: Payload,
  timeout: number | undefined,
  retries: number,
  name: string | null,
): Exec {
  const row: Payload & { kind: string; id: number } = {
    id: pid,
    kind: "call",
    verb,
    parent: k.currentTask,
    caller: k.currentOrigin,
    payload,
    named: name !== null,
    retries,
    attempts: 1,
    timeout: timeout ?? null,
  };
  if (name !== null) row["name"] = name;
  k.store.put(target, row);
  return k.ledger.open({ id: pid, target, verb, payload, timeout, retries, name, caller: k.currentOrigin });
}

/** Await one launched item to its final outcome, applying the retry budget. */
export async function settle(k: Kernel, l: Launched): Promise<Outcome> {
  if (l.settled !== undefined) return l.settled;
  if (l.identityRead === true && l.exec === undefined) {
    // Unnamed `kernel` reflect: the read itself, nothing recorded.
    return { pid: null, status: "completed", result: identityReflect(k, l) };
  }
  const exec = l.exec as Exec;
  const named = (o: Outcome): Outcome => (exec.name !== null ? { ...o, name: exec.name } : o);

  if (l.joined === true) {
    // A joiner's `timeout` bounds only ITS OWN wait — it does not own the
    // execution, so expiring here leaves the real one running.
    if (l.timeout !== undefined) {
      const raced = await withTimeout(exec.promise, l.timeout);
      if (raced === TIMED_OUT) {
        return named({
          pid: exec.id,
          status: "timed_out",
          result: refusal(
            TIMEOUT,
            `send: joined execution ${exec.id} did not settle within this caller's ${l.timeout}s wait; the execution itself keeps running`,
            { extra: { task_id: exec.id } },
          ),
        });
      }
      return named(raced);
    }
    return named(await exec.promise);
  }

  if (l.identityRead === true) {
    // Named `kernel` reflect: ledgered (dedup, join, a settle row), no task.
    const result = identityReflect(k, l);
    const out: Outcome = { pid: exec.id, status: errorShaped(result) ? "errored" : "completed", result, attempts: 1 };
    k.store.settle(exec, out);
    return named(out);
  }

  let attempt = 0;
  for (;;) {
    attempt += 1;
    exec.attempts = attempt;
    const call = k.store.rows.get(exec.id);
    if (call !== undefined) call["attempts"] = attempt; // the row counts real runs
    const out = await runOnce(k, l, exec.id, attempt);
    // `completed` never retries; `cancelled` is ABSORBING — an operator kill
    // stops the loop rather than being re-dispatched over.
    const retryable = out.status === "failed" || out.status === "timed_out";
    if (!retryable || attempt > exec.retries) {
      k.store.settle(exec, out);
      return named(out);
    }
    // Only the FINAL outcome settles the entry: a joiner must never observe an
    // attempt that was retried away.
  }
}

const TIMED_OUT = Symbol("timed_out");
const CANCELLED = Symbol("cancelled");

/** The root's identity reflect, decorated for the CALLER (`k` — its roster is
 *  gated on the caller's origin). */
function identityReflect(k: Kernel, l: Launched): Json {
  const root = k.get(l.target as string) as Agent;
  return applyReflectFlags(k, root, l.payload as Payload, reflectIdentity(root, k.rootId));
}

/** `p`, or `TIMED_OUT` if it is still pending after `seconds`. Only the DEADLINE
 *  is a timeout: a `p` that REJECTS rejects here too — a handler that raised
 *  settles `failed`, whether or not its item carried a timeout. */
function withTimeout<T>(p: Promise<T>, seconds: number): Promise<T | typeof TIMED_OUT> {
  const { setTimeout: set, clearTimeout: clear } = timers();
  return new Promise((resolve, reject) => {
    const t = set(() => resolve(TIMED_OUT), seconds * 1000);
    p.then(
      (v) => { clear(t); resolve(v); },
      (e: unknown) => { clear(t); reject(e); },
    );
  });
}

async function runOnce(k: Kernel, l: Launched, pid: number, attempt: number): Promise<Outcome> {
  const target = l.target as string;
  const payload = l.payload as Payload;
  const verb = typeof payload["type"] === "string" ? payload["type"] : "";

  const task = k.tasks.register(pid, target, verb, k.currentTask);
  // A CANCEL SETTLES AT ONCE. JS cannot abort a running handler, so the settle
  // races it against the task's kill: a cancelled dispatch's caller sees
  // `cancelled` now, and the handler's late answer lands nowhere.
  const killed = new Promise<typeof CANCELLED>((res) => {
    task.onCancel.push(() => res(CANCELLED));
  });
  const run = k.dispatch(target, payload, pid);
  run.catch(() => undefined); // a late failure after a cancel has no reader
  try {
    const raced = Promise.race([run, killed]);
    const result = l.timeout === undefined
      ? await raced
      : await withTimeout(raced, l.timeout);

    if (result === TIMED_OUT) {
      // Expiry goes through the FRONT DOOR so every `on_cancel` hook runs
      // before the abort — an expiring call must unwind like any other cancel.
      k.tasks.cancel(pid, k.rootId);
      return {
        pid,
        status: "timed_out",
        result: refusal(
          TIMEOUT,
          `send: ${target} ${pyRepr(verb)} exceeded its ${l.timeout}s timeout; task cancelled`,
          { extra: { task_id: pid } },
        ),
        attempts: attempt,
      };
    }
    if (result === CANCELLED || task.killed) {
      return { pid, status: "cancelled", result: { cancelled: true }, attempts: attempt };
    }
    // FAILURE IS A STATUS: a handler that ran and said no settles `errored`.
    const status = errorShaped(result as Json) ? "errored" : "completed";
    return { pid, status, result: result as Json, attempts: attempt };
  } catch (err) {
    // A handler THROWING settles as a failed outcome — `send` itself raises
    // nothing. A thrown error that propagated would kill every sibling item in
    // the batch for one bad handler. The text names the error's class first, as
    // python's `f"{type(e).__name__}: {e}"` does.
    const msg = err instanceof Error ? `${err.name}: ${err.message}` : String(err);
    return { pid, status: "failed", result: { error: msg, raised: true }, attempts: attempt };
  } finally {
    k.tasks.done(pid);
  }
}

/** Assemble the batch: recall named executions from the store (started only),
 *  then the sync launch for all, then the concurrent settle. */
export async function runBatch(k: Kernel, items: Item[]): Promise<Outcome[]> {
  if (!Array.isArray(items)) {
    throw new TypeError("send takes a LIST of {target, payload, timeout?} items");
  }
  if (
    k.store.storeId !== null &&
    items.some((it) => isPlainObject(it as unknown) && it.id !== undefined && it.id !== null)
  ) {
    await k.store.recall(items, k.currentTask);
  }
  const launched = items.map((it, i) => launch(k, i, it));
  return Promise.all(launched.map((l) => settle(k, l)));
}
