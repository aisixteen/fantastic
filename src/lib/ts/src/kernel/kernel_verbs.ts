// KERNEL VERBS — answered by the ROOT ONLY (#22/#26).
//
// They act on the kernel-wide task table and execution ledger, so the agent you
// NAME is the OPERAND, not the actor. Answered everywhere, "send cancel to X"
// could mean X performs it, X is cancelled, or X owns the task — only one was
// ever true, and a model that could not tell burned whole runs guessing.
//
// A non-root agent refuses with `reason:"not_here"` naming the fix. The refusal
// is issued in the HANDLER, not the call gate, so a probe asserting a gate
// denial's `side` stays valid.
//
// This kernel answers the three verbs that correspond to state it actually
// owns. Tree CRUD (`create_agent`/`delete_agent`/`list_agents`) is deliberately
// NOT here: the frontend's tree is built by its bundles and persisted through
// the host loader, so inventing local CRUD verbs would advertise a surface that
// does not exist.

import type { Json, Payload } from "./json.ts";
import type { Kernel } from "./kernel.ts";
import { NOT_HERE, bad, missing, refusal } from "./refusal.ts";
import { page } from "./paging.ts";

export const KERNEL_VERBS = ["ps", "cancel", "ledger"] as const;

export function isKernelVerb(verb: string): boolean {
  return (KERNEL_VERBS as readonly string[]).includes(verb);
}

/** The signatures reflect advertises. Each states its CONSEQUENCE BEFORE the
 *  first em-dash: a default reflect truncates there, so anything after it is
 *  invisible to the reader that needs it most. */
export const KERNEL_VERB_DOCS: { [verb: string]: string } = {
  _:
    "Kernel-owned: send to 'kernel'. The id you name is the OPERAND, not the actor. (`verbs` is what this agent itself does.) This kernel answers the three below; tree CRUD lives on the HOST root, reached over the bridge.",
  ps: "args: limit:int?, after:str? — lists the LIVE dispatches (id, agent, verb), paged.",
  cancel:
    "args: task_id:str — kills that task AND ITS SUBTREE; returns {cancelled, killed:[ids]}. An unknown or finished id kills nothing and says so.",
  ledger:
    "args: limit:int?, after:str? — every execution YOU originated and what it became (id, target, status, attempts, retries, named); terminal non-completed rows ARE the dead-letter queue. Origin-scoped: an agent never sees another's rows.",
};

export function answerKernelVerb(
  k: Kernel,
  targetId: string,
  verb: string,
  payload: Payload,
): Json {
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
    const taskId = payload["task_id"];
    if (taskId === undefined) return missing("cancel", "task_id");
    if (typeof taskId !== "string" || taskId === "") {
      return bad("cancel", "task_id", "a non-empty string");
    }
    const killed = k.tasks.cancel(taskId);
    return { cancelled: killed.length > 0, killed };
  }

  // `ledger` — ORIGIN-SCOPED. Unscoped this is a read side-channel around
  // `callable_by`: an agent that may not call another could still read the
  // result BODIES of everything that other agent ran.
  const rows = k.ledger.visibleTo(k.currentOrigin);
  const p = page(rows, payload, (r) => r.id);
  return { executions: p.page as unknown as Json, next: p.next };
}
