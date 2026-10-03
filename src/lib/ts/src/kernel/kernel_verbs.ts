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
import { BAD_ARG, NOT_FOUND, NOT_HERE, missing, refusal } from "./refusal.ts";
import { pyRepr } from "./pyjson.ts";
import { page } from "./paging.ts";
import { shutdownAgents, startKernel, stopKernel } from "./lifecycle.ts";
import { isService } from "./tasks.ts";
import { timers } from "./timers.ts";

export const KERNEL_VERBS = [
  "ps",
  "ledger",
  "ack_deadletter",
  "kernel_start",
  "cancel",
  "shutdown_agents",
  "kernel_stop",
] as const;

export function isKernelVerb(verb: string): boolean {
  return (KERNEL_VERBS as readonly string[]).includes(verb);
}

/** The signatures reflect advertises — python `_SYSTEM_VERB_DOCS`, VERBATIM and
 *  in its order (rust and swift copy the same bytes), minus the tree CRUD this
 *  kernel does not answer. Each states its CONSEQUENCE BEFORE the first em-dash:
 *  a default reflect truncates there, so anything after it is invisible to the
 *  reader that needs it most. The `_` line is this kernel's own: it says which
 *  verbs live here and what `kernel_stop` means in a page. */
export const KERNEL_VERB_DOCS: { [verb: string]: string } = {
  _:
    "Kernel-owned: send to 'kernel'. The id you name is the OPERAND, not the actor. (`verbs` is what this agent itself does.) This kernel answers the verbs below; tree CRUD lives on the HOST root, reached over the bridge. A browser kernel has no process to exit: `kernel_stop` stops it in place, refusing new work until the page reloads.",
  ps:
    "No args. The live kernel-wide TASK TABLE — {tasks:[{id, agent, parent, " +
    "verb, started_at}]}, one row per in-flight dispatch. Paged.",
  ledger:
    "No args. EVERY execution and what it became — named under its id, " +
    "unnamed under its minted pid — {executions:[{id, target, status, " +
    "attempts, retries, named, outcome?, acked?}]}, paged, scoped to what YOU " +
    "originated; read from the kernel's memory store plus what is not yet " +
    "flushed. `outcome` keeps the RESULT only for a named call; a dead letter " +
    "keeps its error; anything else is {status, bytes} — the reply itself went " +
    "to its caller. Failed/timed_out/cancelled rows are the dead-letter queue " +
    "(announced by the `on_deadletter` event): inspect, retry with a NEW id " +
    "if you choose, then `ack_deadletter` it — nothing is retried silently.",
  ack_deadletter:
    "args: id:int|str (req — an execution id, or its name). Acknowledges a DEAD " +
    "LETTER (failed/timed_out/cancelled) you originated: recorded in the " +
    "kernel's store, it may then be pruned. Returns {acked:true, id}.",
  kernel_start:
    "No args. THE START SEQUENCE — the root must name `memory_store_id` (fail " +
    "loud otherwise): boots the root, then that store, starts flushing the " +
    "kernel's execution rows into it, then boots every other agent in " +
    "registration order. Idempotent. Returns {started, store, booted:[ids]}.",
  cancel:
    "args: task_id:str. Kills that task AND ITS SUBTREE — the id names its " +
    "own agent. Returns {cancelled:bool, killed:[…]}; the blocked caller's " +
    "batch item settles as status:'cancelled'. Unknown id ⇒ {cancelled:false}.",
  shutdown_agents:
    "No args. Stops the AGENTS, not the process: drains (new roots refused), " +
    "sends `shutdown` to every agent advertising it, waits for in-flight work, " +
    "cancels the rest. Returns {drained, shut:[ids], cancelled:[ids]}.",
  kernel_stop:
    "No args. Stops the kernel PROCESS gracefully — drain, every call settles " +
    "(done, or cancelled into the dead letters), the last rows flush, agents " +
    "shut down, exit. Under env:'container', the container.",
};

export async function answerKernelVerb(
  k: Kernel,
  targetId: string,
  verb: string,
  payload: Payload,
): Promise<Json> {
  if (k.rootId !== null && targetId !== k.rootId) {
    return refusal(NOT_HERE, `${verb} is a kernel verb — ${targetId} does not answer it`, {
      hint: "address the kernel root ('kernel'); name the agent you mean in `parent_id` / `id`",
      extra: { verb },
    });
  }

  if (verb === "ps") {
    const listed = k.tasks.list(payload);
    return { tasks: listed.tasks, next: listed.next };
  }

  if (verb === "cancel") {
    const raw = payload["task_id"];
    // python: absent, "", a bool, or anything not an int / str (a float, a list,
    // a dict) is a MISSING task id — there is no task it could name.
    if (
      raw === undefined || raw === null || raw === "" ||
      !((typeof raw === "number" && Number.isInteger(raw)) || typeof raw === "string")
    ) {
      return missing("cancel", "task_id");
    }
    // A caller's NAME for an execution is its handle too.
    const taskId = asId(raw) ?? (typeof raw === "string" ? k.ledger.resolveName(raw) : undefined);
    // Names no task — honest, not a refusal (the same answer a stale id gets).
    if (taskId === undefined || taskId === null) return { cancelled: false, killed: [] };
    // A row already killed is on its way out — not in the table, as python sees it.
    const row = k.tasks.get(taskId);
    if (row === undefined || row.killed) return { cancelled: false, killed: [] };
    if (isService(row, k.rootId)) {
      return refusal(
        BAD_ARG,
        `cancel: task ${taskId} (${row.verb}) is a kernel service task — not cancellable`,
        {
          hint: "it runs for the life of the kernel and stays listed in ps; kernel_stop ends it",
          extra: { arg: "task_id" },
        },
      );
    }
    return { cancelled: true, killed: k.tasks.cancel(taskId, k.rootId) };
  }

  if (verb === "ledger") {
    // CALLER-SCOPED. The ledger carries other agents' outcomes; an unscoped read
    // would be a side-channel around `callable_by`. An agent sees the executions
    // IT originated; the operator sees all. The kernel scopes — no caller ever
    // touches the store's `kernel` key.
    const viewer = k.currentCaller;
    const entries = await k.store.ledgerEntries(k);
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
    const entries = await k.store.ledgerEntries(k);
    const eid = asId(raw);
    const hit = entries.find((e) =>
      eid !== null ? e.id === eid : e["name"] === raw,
    );
    if (hit === undefined || (viewer !== null && hit._caller !== viewer)) {
      return refusal(NOT_FOUND, `ack_deadletter: no execution ${pyRepr(raw)} of yours in the ledger`, {
        extra: { arg: "id" },
      });
    }
    if (!DEAD_STATUSES.has(String(hit["status"]))) {
      return refusal(
        BAD_ARG,
        `ack_deadletter: execution ${hit.id} is ${pyRepr(hit["status"])}, not a dead letter — only failed / timed_out / cancelled work is acknowledged`,
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

  // The kernel's own sends on a verb's behalf parent to that verb's task.
  if (verb === "kernel_start") return startKernel(k.operatorView(), true);

  if (verb === "shutdown_agents") return shutdownAgents(k.operatorView(), k.currentTask);

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
