// The batch-send vocabulary (#29): `send([Item]) -> [Outcome]`.
//
// THERE IS NO SINGLE-CALL SUGAR, in any runtime. Its ABSENCE is the feature:
// with two entry points the single one drifts (it grew its own timeout
// handling, its own refusal shape, its own trace hook) until the two disagreed
// about what a send even is. A single call is a one-element array:
//
//     const r = (await kernel.send([{ target, payload }]))[0].result;

import type { Json, Payload } from "./json.ts";
import { pyTruthy } from "./pyjson.ts";

/**
 * The terminal states of ONE batch item (python's outcome vocabulary):
 *   completed — the handler ran and answered; `result` is its reply verbatim.
 *   errored   — the handler RAN and said NO: its reply carries a truthy `error`
 *               (a missing path, a refusal it composed). FAILURE IS A STATUS —
 *               a reader learns it from `status`, never by parsing `result`.
 *               Not retried (a semantic no is not transient) and not a dead
 *               letter.
 *   failed    — no handler answer exists: a refusal before dispatch, or the
 *               handler threw.
 *   cancelled — killed via `cancel`.
 *   timed_out — the item's own deadline expired.
 * `pid is null` ⟺ the item never reached the task table.
 */
export type Status = "completed" | "errored" | "failed" | "cancelled" | "timed_out";

/** THE DEAD ENDINGS. An execution that settled one of these is a DEAD LETTER: it
 *  is announced (`on_deadletter`), listed by `ledger`, never retried silently,
 *  and retired by `ack_deadletter`. */
export const DEAD_STATUSES: ReadonlySet<string> = new Set(["failed", "timed_out", "cancelled"]);

export interface Item {
  target: string;
  payload: Payload;
  /**
   * A STRING NAMES the execution (ensure-semantics). Unknown ⇒ dispatch; in
   * flight ⇒ JOIN, no second run; settled ⇒ the recorded outcome, handler never
   * runs. The name is the caller's; the execution's id is the kernel's counter.
   *
   * An INT is a message id the kernel minted: it JOINS that execution and never
   * creates one — the kernel is the only allocator. ONE ID SPACE: the same int
   * `ps`/`cancel`/store rows carry.
   */
  id?: string | number;
  /** Seconds. The kernel NEVER defaults this; absent = wait forever. */
  timeout?: number;
  /** Re-dispatch budget for a terminal failure under the SAME id. */
  retries?: number;
}

export interface Outcome {
  pid: number | null;
  status: Status;
  result: Json;
  /** How many times the execution RAN — on an owned execution's final outcome
   *  only. An outcome that ran nothing (a refusal, a dying task's send, a
   *  joiner's own expired wait, the `kernel` reflect fast path) has none —
   *  python `outcome()` is exactly `{pid, status, result}`. */
  attempts?: number;
  /** The caller's NAME for this execution, echoed when the item named one. */
  name?: string;
}

/** A refusal never reached the task table, so it carries no pid. */
export function refused(result: Json): Outcome {
  return { pid: null, status: "failed", result };
}

/** A reply that SAYS no: a dict carrying a truthy `error` (python truthiness). */
export function errorShaped(result: Json): boolean {
  if (result === null || typeof result !== "object" || Array.isArray(result)) return false;
  return pyTruthy((result as Payload)["error"]);
}

/** A message id as an int: a positive integer, or its decimal string (a model
 *  quotes what it copied out of a reply). Anything else is not an id. */
export function asId(x: unknown): number | null {
  if (typeof x === "number") return Number.isInteger(x) && x > 0 ? x : null;
  if (typeof x === "string" && /^\d+$/.test(x.trim())) {
    const n = Number(x.trim());
    return n > 0 ? n : null;
  }
  return null;
}
