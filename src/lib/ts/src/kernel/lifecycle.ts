// THE KERNEL'S LIFECYCLE — port of python `Kernel.start` / `shutdown_agents` /
// `shutdown` (kernel/_kernel.py).
//
//   kernel_start    = the root record must name `memory_store_id` (fail loud
//                     otherwise) → boot the root (it mints the guid every store
//                     key carries) → boot THAT store → prove it answers the
//                     kernel's `read` → start flushing → boot everyone else, in
//                     registration order.
//
// Every boot, the probe and every `shutdown` is KERNEL BOOKKEEPING
// (`Kernel.bookkeeping`): the kernel runs the agent's handler directly, not
// through a send — the same whoever's dispatch asked for it.
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
import { sleep, timers } from "./timers.ts";

function isErr(res: Json | undefined): res is Payload {
  return (
    res !== null && typeof res === "object" && !Array.isArray(res) &&
    Boolean((res as Payload)["error"])
  );
}

/** THE START SEQUENCE — the `kernel_start` verb. Idempotent: a second call
 *  reports the store it is already writing to. The flusher the start spawns is
 *  a task under the verb's own (`k.currentTask`). */
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
          `parent_id:'${root.id}'} to this frontend's ` +
          `tree (persist_record through the host loader), set memory_store_id='memory' on ` +
          `the root record '${root.id}', then reload and send kernel_start`,
        extra: { arg: "memory_store_id" },
      },
    );
  }
  const store = k.get(sid);
  if (store === undefined) {
    return unknown(sid, [...k.agents.keys()], {
      hint: "the root's `memory_store_id` names no agent in this tree",
    });
  }
  const booted: string[] = [];
  for (const a of [root, store]) {
    const out = await k.bookkeeping(a.id, { type: "boot" });
    if (out.status !== "completed") {
      return refusal(
        UNHANDLED,
        `kernel_start: boot of '${a.id}' did not complete (${out.status}): ${JSON.stringify(out.result)}`,
      );
    }
    booted.push(a.id);
  }
  // The store must answer the kernel's `read` (the kernel writes as `kernel`) —
  // probed now, refused now, never discovered on the first flush.
  const probe = (await k.bookkeeping(store.id, { type: "read", limit: 1 })).result;
  if (isErr(probe)) {
    return refusal(
      UNAUTHORIZED,
      `kernel_start: the memory store '${store.id}' refuses the kernel: ${String(probe["error"])}`,
      {
        hint: typeof probe["hint"] === "string"
          ? probe["hint"]
          : `the store must answer the operator's \`read\` — check ${store.id}`,
      },
    );
  }
  // The reserved id bound goes into the root record now (the proxy_loader
  // persists it): a crash from here on skips ids, never reuses one.
  k.updateMeta(root.id, { next_id: s.idReserved });
  s.markStarted(store.id, k.currentTask);
  if (bootAll) {
    for (const a of [...k.agents.values()]) {
      if (a.id === root.id || a.id === store.id) continue;
      const out = await k.bookkeeping(a.id, { type: "boot" });
      if (out.status === "completed") {
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
 * 2. `shutdown` VERB: every agent whose bundle advertises it (its verb docs) is
 *    told, all at once, bounded — kernel bookkeeping, the handler run directly;
 *    what it sends passes the drain.
 * 3. QUIET: wait for the in-flight dispatches to settle, bounded; the rest is
 *    cancelled (roots only — a subtree dies with its root).
 * 4. EVERY CALL SETTLES before this returns: a killed dispatch's batch records
 *    its `cancelled` outcome (and its settle row); bounded.
 * `me` is the task running this verb — it is not pending work, and the
 * `shutdown` handlers run inside it (none for a stop).
 */
export async function shutdownAgents(k: Kernel, me: number | null): Promise<Json> {
  const s = k.store;
  s.draining = true;
  let shut: string[] = [];
  const targets = [...k.agents.values()].filter(
    (a) => {
      const verbs = k.verbDocsOf(a);
      return verbs !== undefined && Object.prototype.hasOwnProperty.call(verbs, "shutdown");
    },
  );
  if (targets.length > 0 && k.rootId !== null) {
    const tell = (id: string): Promise<string> => {
      const { setTimeout: set, clearTimeout: clear } = timers();
      return new Promise((resolve) => {
        const t = set(() => resolve("timed_out"), s.shutdownVerbTimeout * 1000);
        void k.bookkeeping(id, { type: "shutdown" }, { shutdown: true, task: me }).then((out) => {
          clear(t);
          resolve(out.status);
        });
      });
    };
    const outs = await Promise.all(targets.map((a) => tell(a.id)));
    shut = targets.filter((_a, i) => outs[i] === "completed").map((a) => a.id);
  }
  // The verb's own task is not pending work, and a KEEPALIVE loop (the flusher)
  // ends with the stop itself. A FORKED loop never settles on its own, so it is
  // cancelled below rather than waited for.
  const pending = (): number[] =>
    k.tasks.ids().filter((t) => t !== me && k.tasks.get(t)?.keepalive !== true);
  const settling = (): number[] => pending().filter((t) => k.tasks.get(t)?.forked !== true);
  let deadline = Date.now() + s.quietTimeoutMs;
  while (Date.now() < deadline && settling().length > 0) await sleep(50);
  const cancelled: number[] = [];
  for (const tid of pending()) {
    const row = k.tasks.get(tid);
    if (row === undefined || row.parent !== null) continue;
    cancelled.push(...k.tasks.cancel(tid, k.rootId));
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
