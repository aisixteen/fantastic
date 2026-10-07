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
import { BAD_ARG, INVALID_META, NOT_CONNECTED, NOT_FOUND, NOT_HERE, missing, refusal } from "./refusal.ts";
import { pyRepr } from "./pyjson.ts";
import { page } from "./paging.ts";
import { shutdownAgents, startKernel, stopKernel } from "./lifecycle.ts";
import { isService } from "./tasks.ts";
import { timers } from "./timers.ts";
import { renderDocs } from "./docs.ts";
import { isFolderName, loadBundle } from "./folders.ts";
import type { FolderText } from "./folders.ts";

export const KERNEL_VERBS = [
  "ps",
  "ledger",
  "ack_deadletter",
  "kernel_start",
  "cancel",
  "shutdown_agents",
  "kernel_stop",
  "docs",
  "reload_bundle",
] as const;

export function isKernelVerb(verb: string): boolean {
  return (KERNEL_VERBS as readonly string[]).includes(verb);
}

/** What `docs of=kernel` serves — python `_SYSTEM_VERB_DOCS`, VERBATIM and in
 *  its order (rust and swift copy the same bytes), minus the tree CRUD this
 *  kernel does not answer (that lives on the HOST root, over the bridge). */
export const KERNEL_VERB_DOCS: { [verb: string]: string } = {
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
  docs:
    "args: of:str (req — an agent id (`<guid>|<agent id>` on a connected kernel), a bundle, or 'kernel'), " +
    "tier:'oneliners'|'detailed'?, verb:str?. HOW TO CALL an agent — its " +
    "bundle's verbs: `oneliners` (default) is one signature line per verb, " +
    "`detailed` the full text (no `verb`: plus emits, cancel and notes). Pull " +
    "once per bundle; `detailed` for a verb you are about to use. Gated like " +
    "every kernel verb: a `can_call` grant of `docs` on the root. " +
    "Returns {of, bundle, tier, verbs, emits?, cancel?, notes?}.",
  reload_bundle:
    "args: name:str (req). Re-read `bundles/<name>/bundle.yaml` through this " +
    "kernel's loader (a new folder loads) and restart every live instance of it, " +
    "abruptly — shutdown with reason 'reloaded', then boot under the fresh load. " +
    "A folder that no longer loads is refused naming the folder and field; its " +
    "instances keep the previous load. Returns {reloaded, instances}.",
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

  if (verb === "docs") {
    const of = payload["of"];
    if (typeof of !== "string" || of === "") {
      return missing("docs", "of", "an agent id, a bundle (e.g. 'exec_bridge.tools') or 'kernel'");
    }
    const tierRaw = payload["tier"];
    const tier = tierRaw === undefined ? "oneliners" : tierRaw;
    if (tier !== "oneliners" && tier !== "detailed") {
      return refusal(BAD_ARG, "docs: tier must be 'oneliners' or 'detailed'", { extra: { arg: "tier" } });
    }
    const v = payload["verb"];
    if (v !== undefined && v !== null && (typeof v !== "string" || v === "")) {
      return refusal(BAD_ARG, "docs: verb must be a verb name", { extra: { arg: "verb" } });
    }
    const vName = typeof v === "string" ? v : null;
    if (of === "kernel") return renderDocs(of, "kernel", tier, vName, KERNEL_VERB_DOCS);
    const agent = k.agents.get(of);
    if (agent !== undefined) {
      return renderDocs(of, agent.handlerModule, tier, vName, k.verbDocsOf(agent) ?? {});
    }
    // A BUNDLE by either name the kernel lists it under: `bundles='ids'` lists the
    // NAME (`file_bridge`), an agent record carries the handler_module
    // (`file_bridge.tools`). Measured 2026-10-06 (rust, plan + build): models asked
    // for the listed name and were refused.
    // a refused folder (its row carries `error`) is listed, never documented
    const listed = k.availableBundles().find(
      (b) => b.error === undefined && (b.handler_module === of || b.name === of),
    );
    if (listed !== undefined) {
      return renderDocs(of, listed.handler_module, tier, vName, k.bundleVerbDocs(listed.handler_module) ?? {});
    }
    return refusal(BAD_ARG, `docs: ${pyRepr(of)} names no agent, bundle or 'kernel'`, {
      hint: "an agent id from a reflect tree, a bundle's name or handler_module (reflect bundles='all' lists both), or 'kernel'",
      extra: { arg: "of" },
    });
  }

  if (verb === "reload_bundle") return reloadBundle(k, payload);

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
    const entries = await k.store.ledgerEntries(k.currentTask);
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
    const entries = await k.store.ledgerEntries(k.currentTask);
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

  if (verb === "kernel_start") return startKernel(k, true);

  if (verb === "shutdown_agents") return shutdownAgents(k, k.currentTask);

  // `kernel_stop` — acknowledged FIRST; the graceful stop runs after the reply
  // is on its way (python defers the exit the same way).
  const s = k.store;
  if (s.stopping === null) {
    s.stopping = new Promise((res) => {
      timers().setTimeout(() => {
        void stopKernel(k).then(res);
      }, 100);
    });
  }
  return { type: "kernel_stop", ok: true };
}

/** `reload_bundle {name}` — python `kernel_state._reload_bundle`, through this
 *  kernel's loader: re-read the folder (`read_bundles`), re-load it, then each
 *  live instance AS ITSELF (kernel bookkeeping): `shutdown` with the reason under
 *  the handler it runs, rebound to the fresh load, `boot`. */
async function reloadBundle(k: Kernel, payload: Payload): Promise<Json> {
  const name = payload["name"];
  if (typeof name !== "string" || name === "") {
    return missing("reload_bundle", "name", "a folder under the page root's bundles/");
  }
  if (k.readFolders === null) {
    return refusal(NOT_CONNECTED, "reload_bundle: this kernel reads no folders — no loader is wired", {
      hint: "boot the kernel with a loader (boot({loader}))",
    });
  }
  if (!isFolderName(name)) {
    return refusal(NOT_FOUND, `reload_bundle: no folder bundles/${name}`, {
      hint: "a bundle is a folder under the page root's bundles/ with a bundle.yaml",
    });
  }
  const reply = (await k.readFolders(name)) as Payload | null;
  const rows = reply?.["bundles"];
  if (!Array.isArray(rows) || rows.length !== 1) {
    // the loader's own refusal (no such folder, a denied read) — said through
    const why = typeof reply?.["error"] === "string" ? reply["error"] : JSON.stringify(reply);
    return refusal(typeof reply?.["reason"] === "string" ? reply["reason"] : NOT_FOUND, `reload_bundle: ${why}`, {
      ...(typeof reply?.["hint"] === "string" ? { hint: reply["hint"] } : {}),
    });
  }
  const err = loadBundle(k, rows[0] as unknown as FolderText);
  if (err !== null) return refusal(INVALID_META, err);
  const handler = k.folderHandler(name) ?? null;
  const live = [...k.agents.values()]
    .filter((a) => a.handlerModule === name)
    .sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  for (const a of live) {
    await k.bookkeeping(a.id, { type: "shutdown", reason: "reloaded" });
    a.handler = handler;
    await k.bookkeeping(a.id, { type: "boot" });
  }
  return { reloaded: name, instances: live.map((a) => a.id) };
}
