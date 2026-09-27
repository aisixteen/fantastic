import type { Kernel } from "../../kernel/kernel.ts";
import type { Json, Payload } from "../../kernel/json.ts";
import { nowSeconds } from "../../kernel/timers.ts";
import { aiMem } from "./memory.ts";
import type { CurrentEntry } from "./memory.ts";
import { toCaller } from "./seam.ts";

// THE STATUS CHANNEL — port of python `ai_core/core.py` `_emit_status` /
// `_status_snapshot` / `_redact_text`. One channel, one source of truth: a UI
// subscribes to `status` events for live phase transitions (queued → thinking →
// streaming → done) and calls the `status` verb to rebuild after a reload.

/**
 * Broadcast a structured `status` event to the caller AND keep the in-flight
 * entry's phase in sync, so the `status` verb and the events never disagree.
 * `send_id`/`started_at` come from the in-flight entry unless given.
 */
export function emitStatus(kernel: Kernel, selfId: string, clientId: string, phase: string, detail: Payload = {}): void {
  const mem = aiMem(kernel);
  const cur = mem.current.get(selfId);
  const d: Payload = { ...detail };
  if (cur !== undefined) {
    cur.phase = phase;
    if (!("send_id" in d)) d["send_id"] = cur.send_id;
    if (!("started_at" in d)) d["started_at"] = cur.started_at;
  }
  if (!("queue_depth" in d)) d["queue_depth"] = (mem.queue.get(selfId) ?? []).length;
  toCaller(kernel, selfId, clientId, { type: "status", source: selfId, phase, detail: d, ts: nowSeconds() });
}

/** An entry with its text-bearing fields removed — another caller learns THAT
 *  work is in flight, never what it says. `task_id` is bookkeeping, not status. */
function redact(cur: CurrentEntry): Payload {
  const out: Payload = {};
  for (const [k, v] of Object.entries(cur)) {
    if (k !== "text" && k !== "text_so_far" && k !== "last_tool" && k !== "task_id") out[k] = v as Json;
  }
  return out;
}

/** The `status` verb's reply. With a client id: full text only for that caller's
 *  entries, other clients collapse to a count. Without: text redacted everywhere. */
export function statusSnapshot(kernel: Kernel, selfId: string, clientId: string | null): Payload {
  const mem = aiMem(kernel);
  const cur = mem.current.get(selfId);
  const queue = [...(mem.queue.get(selfId) ?? [])];
  let curOut: Payload | null = null;
  let minePending: Json[] = [];
  let othersPending: number;
  if (clientId) {
    if (cur !== undefined) {
      const isMine = cur.client_id === clientId;
      curOut = {
        phase: cur.phase,
        send_id: cur.send_id,
        started_at: cur.started_at,
        elapsed: nowSeconds() - cur.started_at,
        is_mine: isMine,
      };
      if (isMine) {
        curOut["text"] = cur.text;
        curOut["text_so_far"] = cur.text_so_far;
        if (cur.last_tool !== undefined) curOut["last_tool"] = cur.last_tool;
      }
    }
    minePending = queue
      .filter((e) => e.client_id === clientId)
      .map((e) => ({ send_id: e.send_id, text: e.text, queued_at: e.queued_at }));
    othersPending = queue.filter((e) => e.client_id !== clientId).length;
  } else {
    if (cur !== undefined) {
      curOut = redact(cur);
      curOut["elapsed"] = nowSeconds() - cur.started_at;
      curOut["is_mine"] = false;
    }
    othersPending = queue.length;
  }
  return {
    source: selfId,
    client_id: clientId,
    generating: cur !== undefined,
    current: curOut,
    mine_pending: minePending,
    others_pending: othersPending,
  };
}
