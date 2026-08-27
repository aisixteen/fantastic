// The batch-send vocabulary (#29): `send([Item]) -> [Outcome]`.
//
// THERE IS NO SINGLE-CALL SUGAR, in any runtime. Its ABSENCE is the feature:
// with two entry points the single one drifts (it grew its own timeout
// handling, its own refusal shape, its own trace hook) until the two disagreed
// about what a send even is. A single call is a one-element array:
//
//     const r = (await kernel.send([{ target, payload }]))[0].result;

import type { Json, Payload } from "./json.ts";

/** `pid is null` ⟺ the item never reached the task table. */
export type Status = "completed" | "failed" | "cancelled" | "timed_out";

export interface Item {
  target: string;
  payload: Payload;
  /**
   * NAMES the execution (ensure-semantics). Unknown ⇒ dispatch; in flight ⇒
   * JOIN, no second run; settled ⇒ the recorded outcome, handler never runs.
   *
   * ONE ID SPACE: this is the same id `ps`/`cancel`/reply tags carry. A caller
   * id may not contain ":" — that namespace is kernel-minted.
   */
  id?: string;
  /** Seconds. The kernel NEVER defaults this; absent = wait forever. */
  timeout?: number;
  /** Re-dispatch budget for a terminal failure under the SAME id. */
  retries?: number;
}

export interface Outcome {
  pid: string | null;
  status: Status;
  result: Json;
  attempts: number;
}

/** A refusal never reached the task table, so it carries no pid. */
export function refused(result: Json): Outcome {
  return { pid: null, status: "failed", result, attempts: 1 };
}

export function completed(pid: string | null, result: Json, attempts = 1): Outcome {
  return { pid, status: "completed", result, attempts };
}

export function isTerminalFailure(status: Status): boolean {
  return status === "failed" || status === "timed_out";
}

/** The wire/`ledger`-verb view of one execution. */
export interface LedgerRow extends Payload {
  id: string;
  target: string;
  status: string;
  attempts: number;
  retries: number;
  named: boolean;
}
