// KERNEL VERBS — answered by the ROOT ONLY (#22/#26).
//
// They act on the kernel-wide task table, the execution ledger and the kernel's
// lifecycle, so the agent you NAME is the OPERAND, not the actor. Answered
// everywhere, "send cancel to X" could mean X performs it, X is cancelled, or X
// owns the task — only one was ever true, and a model that could not tell burned
// whole runs guessing.
//
// A non-root agent refuses with `reason:"not_here"` naming the fix. The refusal
// is issued in the HANDLER, not the call gate, so a probe asserting a gate
// denial's `side` stays valid.
//
// This kernel answers the verbs that correspond to state it actually owns. Tree
// CRUD (`create_agent`/`delete_agent`/`list_agents`) is deliberately NOT here:
// the frontend's tree is built by its bundles and persisted through the host
// loader, so inventing local CRUD verbs would advertise a surface that does not
// exist.

import type { Json, Payload } from "./json.ts";
import type { Kernel } from "./kernel.ts";
import { DEAD_STATUSES, asId } from "./outcome.ts";
import { BAD_ARG, NOT_FOUND, NOT_HERE, bad, missing, refusal } from "./refusal.ts";
import { page } from "./paging.ts";
import { shutdownAgents, startKernel, stopKernel } from "./lifecycle.ts";
import { timers } from "./timers.ts";

export const KERNEL_VERBS = [
  "ps",
  "cancel",
  "ledger",
  "ack_deadletter",
  "kernel_start",
  "shutdown_agents",
  "kernel_stop",
] as const;

export function isKernelVerb(verb: string): boolean {
  return (KERNEL_VERBS as readonly string[]).includes(verb);
}

/** The signatures reflect advertises. Each states its CONSEQUENCE BEFORE the
 *  first em-dash: a default reflect truncates there, so anything after it is
 *  invisible to the reader that needs it most. */
export const KERNEL_VERB_DOCS: { [verb: string]: string } = {
  _:
    "Kernel-owned: send to 'kernel'. The id you name is the OPERAND, not the actor. (`verbs` is what this agent itself does.) This kernel answers the verbs below; tree CRUD lives on the HOST root, reached over the bridge.",
  ps: "No args. The live kernel-wide TASK TABLE — {tasks:[{id, agent, parent, verb, started_at}]}, one row per in-flight dispatch; an id is the message id of the call that started it, `parent` chains the rows into the send tree. Paged.",
  cancel:
    "args: task_id:int|str. Kills that task AND ITS SUBTREE (a name kills the execution it names) — returns {cancelled, killed:[ids]}; the blocked caller's item settles status:'cancelled'. An unknown or finished id kills nothing and says so.",
  ledger:
    "No args. EVERY execution YOU originated and what it became — {executions:[{id, target, status, attempts, retries, named, name?, outcome?, acked?}]}, paged, read from the kernel's memory store plus what is not yet flushed. `outcome` keeps the RESULT only for a named call; a dead letter keeps its error; anything else is {status, bytes}. Failed/timed_out/cancelled rows are the dead-letter queue (announced by the `on_deadletter` event): inspect, retry with a NEW id if you choose, then `ack_deadletter` it.",
  ack_deadletter:
    "args: id:int|str (req — an execution id, or its name). Acknowledges a DEAD LETTER you originated — recorded in the kernel's store, it may then be pruned. Returns {acked:true, id}.",
  kernel_start:
    "No args. THE START SEQUENCE — the root must name `memory_store_id` (fail loud otherwise): boots the root (it mints the guid), then that store, starts flushing the kernel's execution rows into it, then boots every other agent in registration order. Idempotent. Returns {started, store, booted:[ids]}.",
  shutdown_agents:
    "No args. Stops the AGENTS, not the kernel: drains (new roots refused), sends `shutdown` to every agent advertising it, waits for in-flight work, cancels the rest. Returns {drained, shut:[ids], cancelled:[ids]}.",
  kernel_stop:
    "No args. Stops this kernel gracefully — drain, every call settles (done, or cancelled into the dead letters), the last rows flush, the flusher stops. A browser kernel has no process to exit: it refuses new work until the page reloads.",
};

export async function answerKernelVerb(
  k: Kernel,
  targetId: string,
  verb: string,
  payload: Payload,
): Promise<Json> {
  if (k.rootId !== null && targetId !== k.rootId) {
    return refusal(
      NOT_HERE,
      `'${verb}' is a kernel verb and is answered by the root only, not by '${targetId}'`,
      {
        hint: `send {type:'${verb}'} to 'kernel'; name '${targetId}' in the payload if it is the operand`,
        extra: { verb },
      },
    );
  }

  if (verb === "ps") {
    const listed = k.tasks.list(payload);
    return { tasks: listed.tasks, next: listed.next };
  }

  if (verb === "cancel") {
    const raw = payload["task_id"];
    if (raw === undefined || raw === null || raw === "") return missing("cancel", "task_id");
    if (typeof raw !== "string" && typeof raw !== "number") {
      return bad("cancel", "task_id", "a message id (int) or an execution name");
    }
    // A caller's NAME for an execution is its handle too.
    const taskId = asId(raw) ?? (typeof raw === "string" ? k.ledger.resolveName(raw) : undefined);
    // Names no task — honest, not a refusal (the same answer a stale id gets).
    if (taskId === undefined || taskId === null) return { cancelled: false, killed: [] };
    const killed = k.tasks.cancel(taskId);
    return { cancelled: killed.length > 0, killed };
  }

  if (verb === "ledger") {
    // CALLER-SCOPED. The ledger carries other agents' outcomes; an unscoped read
    // would be a side-channel around `callable_by`. An agent sees the executions
    // IT originated; the operator sees all. The kernel scopes — no caller ever
    // touches the store's `kernel` key.
    const viewer = k.currentCaller;
    const entries = await k.store.ledgerEntries();
    const rows: Payload[] = [];
    for (const e of entries) {
      if (viewer !== null && e._caller !== viewer) continue;
      const { _caller: _drop, ...rest } = e;
      rows.push(rest);
    }
    const p = page(rows, payload, (r) => String(r["id"]));
    return { executions: p.page as unknown as Json, next: p.next };
  }

  if (verb === "ack_deadletter") {
    const raw = payload["id"];
    if (raw === undefined || raw === null || raw === "" || typeof raw === "boolean") {
      return missing("ack_deadletter", "id");
    }
    const viewer = k.currentCaller;
    const entries = await k.store.ledgerEntries();
    const eid = asId(raw);
    const hit = entries.find((e) =>
      eid !== null ? e.id === eid : e["name"] === raw,
    );
    if (hit === undefined || (viewer !== null && hit._caller !== viewer)) {
      return refusal(NOT_FOUND, `ack_deadletter: no execution ${JSON.stringify(raw)} of yours in the ledger`, {
        extra: { arg: "id" },
      });
    }
    if (!DEAD_STATUSES.has(String(hit["status"]))) {
      return refusal(
        BAD_ARG,
        `ack_deadletter: execution ${hit.id} is '${String(hit["status"])}', not a dead letter — only failed / timed_out / cancelled work is acknowledged`,
        { extra: { arg: "id" } },
      );
    }
    if (hit["acked"] === true) return { acked: true, id: hit.id, already: true };
    // The ROW kind stays `ack`; the verb is `ack_deadletter` (kernel verbs
    // shadow bundle verbs, and a terminal backend owns `ack`).
    k.store.put(k.rootId, { kind: "ack", parent: hit.id, caller: viewer });
    const exe = k.ledger.get(hit.id);
    if (exe !== undefined) exe.acked = true;
    return { acked: true, id: hit.id };
  }

  if (verb === "kernel_start") return startKernel(k.base, true);

  if (verb === "shutdown_agents") return shutdownAgents(k.base, k.currentTask);

  // `kernel_stop` — acknowledged FIRST; the graceful stop runs after the reply
  // is on its way (python defers the exit the same way).
  const s = k.store;
  if (s.stopping === null) {
    s.stopping = new Promise((res) => {
      timers().setTimeout(() => {
        void stopKernel(k.base).then(res);
      }, 100);
    });
  }
  return { type: "kernel_stop", ok: true };
}
