// THE KERNEL'S LIFECYCLE — port of python `Kernel.start` / `shutdown_agents` /
// `shutdown` (kernel/_kernel.py).
//
//   kernel_start    = the root record must name `memory_store_id` (fail loud
//                     otherwise) → boot the root (it mints the guid every store
//                     key carries) → boot THAT store → prove it serves the
//                     operator → start flushing → boot everyone else, in
//                     registration order.
//   shutdown_agents = drain (new roots refused) → `shutdown` to every agent that
//                     advertises it → quiet → cancel the rest → every call
//                     settles (done, or cancelled ⇒ a dead letter).
//   kernel_stop     = shutdown_agents → the last rows flush → the flusher stops.
//
// The browser kernel is not a process: nothing exits. A stopped kernel refuses
// new roots until the page reloads.

import type { Kernel } from "./kernel.ts";
import type { Json, Payload } from "./json.ts";
import { MISSING_ARG, UNAUTHORIZED, UNHANDLED, refusal, unknown } from "./refusal.ts";
import { sleep } from "./timers.ts";

function isErr(res: Json | undefined): res is Payload {
  return (
    res !== null && typeof res === "object" && !Array.isArray(res) &&
    Boolean((res as Payload)["error"])
  );
}

/** THE START SEQUENCE — the `kernel_start` verb. Idempotent: a second call
 *  reports the store it is already writing to. `k` is the base kernel. */
export async function startKernel(k: Kernel, bootAll: boolean): Promise<Json> {
  const s = k.store;
  if (s.started) return { started: true, already: true, store: s.storeId };
  const root = k.rootId !== null ? k.get(k.rootId) : undefined;
  if (root === undefined) return { error: "kernel: no root agent registered" };
  const sid = root.meta["memory_store_id"];
  if (typeof sid !== "string" || sid === "") {
    return refusal(
      MISSING_ARG,
      "kernel_start: the root record names no `memory_store_id` — the kernel keeps its " +
        "execution rows (ledger, dead letters, named-call dedup) in a memory store and " +
        "will not start without one",
      {
        hint:
          "a memory store is needed — create a ram_store and name it in memory_store_id " +
          `if you have none: add a record {handler_module:'ram_store.ts', id:'memory', ` +
          `parent_id:'${root.id}', operator:true, admins:['kernel']} to this frontend's ` +
          `tree (persist_record through the host loader), set memory_store_id='memory' on ` +
          `the root record '${root.id}', then reload and send kernel_start`,
        extra: { arg: "memory_store_id" },
      },
    );
  }
  const store = k.get(sid);
  if (store === undefined) {
    const r = unknown(sid, [...k.agents.keys()]);
    r["hint"] = "the root's `memory_store_id` names no agent in this tree";
    return r;
  }
  const booted: string[] = [];
  for (const a of [root, store]) {
    const out = (await k.send([{ target: a.id, payload: { type: "boot" } }]))[0]!;
    if (out.status !== "completed") {
      return refusal(
        UNHANDLED,
        `kernel_start: boot of '${a.id}' did not complete (${out.status}): ${JSON.stringify(out.result)}`,
      );
    }
    booted.push(a.id);
  }
  // The store must serve the operator (the kernel writes as `kernel`) — probed
  // now, refused now, never discovered on the first flush.
  const probe = (await k.send([
    { target: store.id, payload: { type: "read", limit: 1 } },
  ]))[0]!.result;
  if (isErr(probe)) {
    return refusal(
      UNAUTHORIZED,
      `kernel_start: the memory store '${store.id}' refuses the kernel: ${String(probe["error"])}`,
      {
        hint: typeof probe["hint"] === "string"
          ? probe["hint"]
          : `the store must serve the operator: set operator=true on '${store.id}'`,
      },
    );
  }
  // The reserved id bound goes into the root record now (the proxy_loader
  // persists it): a crash from here on skips ids, never reuses one.
  k.updateMeta(root.id, { next_id: s.idReserved });
  s.markStarted(store.id);
  if (bootAll) {
    for (const a of [...k.agents.values()]) {
      if (a.id === root.id || a.id === store.id) continue;
      const out = (await k.send([{ target: a.id, payload: { type: "boot" } }]))[0]!;
      if (out.status === "completed" && !isErr(out.result)) {
        booted.push(a.id);
      } else {
        (globalThis as { console?: { warn(m: string): void } }).console?.warn(
          `  [kernel] boot '${a.id}': ${out.status} ${JSON.stringify(out.result)}`,
        );
      }
    }
  }
  return { started: true, store: store.id, booted };
}

/**
 * The AGENT half of a graceful stop — also the `shutdown_agents` kernel verb.
 *
 * 1. DRAIN: new roots are refused; sends with a live parent task still pass.
 * 2. `shutdown` VERB: every agent that advertises it (its reflect `verbs`) is
 *    told, all at once, bounded, as the kernel's own traffic.
 * 3. QUIET: wait for the in-flight dispatches to settle, bounded; the rest is
 *    cancelled (roots only — a subtree dies with its root).
 * 4. EVERY CALL SETTLES before this returns: a killed dispatch's batch records
 *    its `cancelled` outcome (and its settle row); bounded.
 * `me` is the task running this verb — it is not pending work.
 */
export async function shutdownAgents(k: Kernel, me: number | null): Promise<Json> {
  const s = k.store;
  s.draining = true;
  let shut: string[] = [];
  const targets = [...k.agents.values()].filter(
    (a) => a.verbs !== null && Object.prototype.hasOwnProperty.call(a.verbs, "shutdown"),
  );
  if (targets.length > 0 && k.rootId !== null) {
    const outs = await k.kernelView().send(
      targets.map((a) => ({
        target: a.id,
        payload: { type: "shutdown" },
        timeout: s.shutdownVerbTimeout,
      })),
    );
    shut = targets.filter((_a, i) => outs[i]?.status === "completed").map((a) => a.id);
  }
  const pending = (): number[] => k.tasks.ids().filter((t) => t !== me);
  let deadline = Date.now() + s.quietTimeoutMs;
  while (Date.now() < deadline && pending().length > 0) await sleep(50);
  const cancelled: number[] = [];
  for (const tid of pending()) {
    const row = k.tasks.get(tid);
    if (row === undefined || row.parent !== null) continue;
    cancelled.push(...k.tasks.cancel(tid));
  }
  deadline = Date.now() + s.settleTimeoutMs;
  // An execution whose task is gone or KILLED but that has not recorded its
  // ending yet (a killed row leaves the table when its settle runs, a few
  // microtasks after the cancel).
  const unsettled = (): boolean =>
    [...k.ledger.executions.values()].some(
      (e) => !e.settled && (k.tasks.get(e.id)?.killed ?? true),
    );
  while (Date.now() < deadline && unsettled()) await sleep(10);
  return { drained: true, shut, cancelled };
}

/** THE GRACEFUL STOP: the agents half, then the last execution rows land while
 *  the store is still alive, then the flusher stops. Idempotent. */
export async function stopKernel(k: Kernel): Promise<void> {
  const s = k.store;
  if (s.stopped) return;
  await shutdownAgents(k, null);
  if (s.started) {
    try {
      await s.flush();
    } catch (e) {
      (globalThis as { console?: { warn(m: string): void } }).console?.warn(
        `  [kernel] final flush raised: ${e instanceof Error ? e.message : String(e)}`,
      );
    }
  }
  s.halt();
}
