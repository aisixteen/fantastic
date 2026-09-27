import type { Kernel } from "../../kernel/kernel.ts";
import type { Json, Payload } from "../../kernel/json.ts";
import type { Item, Outcome } from "../../kernel/outcome.ts";
import { errorShaped } from "../../kernel/outcome.ts";
import { estimateOne, inlineCap } from "./context.ts";
import { extractToolCalls, malformedCalls, renderReply } from "./tool_parse.ts";
import type { ToolCall, Value } from "./tool_parse.ts";
import { REFLECT_NUDGE, cleanTitle, originLine, replyTag } from "./replies.ts";
import { aiMem, asId, refreshGrants, storeReply } from "./memory.ts";
import { AWAIT_REFUSED, NO_CALL_NUDGE } from "./howtos.ts";
import { emitStatus } from "./status.ts";

// REPLY ROUTING — port of python `ai_core/core.py` `_route_reply` /
// `_deliver_soon` / `_deliver`. After ONE provider pass, its output goes where it
// was addressed:
//
//   the model's own <call>s — each names a target, a verb and `reply_to`. All are
//     dispatched in ONE kernel batch, and everything owed to one destination —
//     results, parse errors, shape errors — arrives as ONE delivered turn, each
//     piece its own `[reply to …]` block, tagged `N of M in batch <b>` when M > 1.
//     `reply_to:'self'` (the default) is what makes a chain; the chain continues
//     ONLY because the model asked again.
//   `done` ends the chain; `await` waits for what is in flight (refused when
//     nothing is); a turn with NO call is nudged — every time.
//   the caller's `send_to` — hands this pass's TEXT to another agent, once.
//
// Runs DETACHED, after the turn released its lock: a self-addressed delivery
// queues behind the current turn instead of deadlocking on it. (This kernel has
// no `spawn_task`, so the detached routing and its deliveries are not `ps` rows.)

export interface RouteArgs {
  selfId: string;
  /** a view attributed to the agent itself — its routed calls are ITS calls */
  kernel: Kernel;
  text: string;
  clientId: string;
  sendTo: string | null;
  systemPrompt: string | null;
  /** the AI layer's per-call deadline (seconds); the kernel defaults nothing */
  sendTimeout: number;
  /** the text this turn answered */
  incoming: string;
  messageId: number | null;
}

interface Unit {
  dest: string;
  target: string;
  verb: string;
  idx: number | null;
  body: string | null;
  title: string | null;
}

function isPlain(v: unknown): v is { [k: string]: Value } {
  return v !== null && typeof v === "object" && !Array.isArray(v) && !(v instanceof Uint8Array);
}

function omit(o: { [k: string]: Value }, keys: string[]): { [k: string]: Value } {
  return Object.fromEntries(Object.entries(o).filter(([k]) => !keys.includes(k)));
}

function hex2(): string {
  const b = new Uint8Array(2);
  (globalThis as { crypto: { getRandomValues: (a: Uint8Array) => Uint8Array } }).crypto.getRandomValues(b);
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
}

export async function routeReply(a: RouteArgs): Promise<void> {
  const { selfId, kernel, clientId, systemPrompt } = a;
  const mem = aiMem(kernel);
  let calls = extractToolCalls(a.text);
  const units: Unit[] = [];
  const items: Item[] = [];
  // An envelope that OPENED and did not parse gets an error block — to SELF:
  // `reply_to` lives inside the body we could not read.
  for (const bad of malformedCalls(a.text)) {
    units.push({ dest: selfId, target: "-", verb: "-", idx: null, body: `${bad.error}\n\nyour block:\n${bad.your_block}`, title: null });
  }
  const doneCalled = calls.some((c) => c.name === "done");
  const awaitCalled = calls.some((c) => c.name === "await");
  // ONE MESSAGE, SEVERAL RECIPIENTS: a send with `to` fans out into one item per
  // recipient; every copy carries the whole list (`to`), the sending turn's id
  // (`msg`) and the message it answers (`re`).
  const expanded: ToolCall[] = [];
  for (let call of calls) {
    const args = call.arguments;
    const to = args["to"];
    if (call.name === "send" && Array.isArray(to) && to.length > 0) {
      const recipients = to.map((t) => String(t).trim()).filter((t) => t !== "");
      const base = omit(args, ["to", "re", "target_id"]);
      for (const r of recipients) {
        const payload: { [k: string]: Value } = { ...(isPlain(args["payload"]) ? args["payload"] : {}) };
        payload["to"] = recipients;
        payload["msg"] = a.messageId;
        if ("re" in args) payload["re"] = args["re"] as Value;
        expanded.push({ ...call, arguments: { ...base, target_id: r, payload } });
      }
      continue;
    }
    if (call.name === "send" && isPlain(args["payload"]) && args["payload"]["type"] === "send") {
      const payload: { [k: string]: Value } = { ...args["payload"] };
      if (!("msg" in payload)) payload["msg"] = a.messageId;
      if ("re" in args) payload["re"] = args["re"] as Value;
      call = { ...call, arguments: { ...omit(args, ["re"]), payload } };
    }
    expanded.push(call);
  }
  calls = expanded;
  for (const call of calls) {
    // CONTROL CALLS, not dispatches: `done` ends the chain, `await` waits,
    // `inquiry_answer` hands this turn's answer back to whoever asked.
    if (call.name === "done" || call.name === "await" || call.name === "inquiry_answer") continue;
    const args = call.arguments;
    const rawTarget = args["target_id"] ?? args["target"];
    // `reply_to` DEFAULTS TO SELF: an omitted key means the model gets its own
    // answer back. Discarding a reply is something you must SAY (null/none).
    let replyTo: string | null;
    if ("reply_to" in args) {
      const r = args["reply_to"];
      replyTo = typeof r === "string" ? r.trim() : null;
      if (replyTo && ["none", "null", "nobody"].includes(replyTo.toLowerCase())) replyTo = null;
    } else {
      replyTo = selfId;
    }
    if (replyTo === "self") replyTo = selfId;
    if (typeof rawTarget !== "string" || rawTarget.trim() === "") {
      if (replyTo) {
        units.push({
          dest: replyTo, target: "-", verb: "-", idx: null, title: null,
          body: renderReply({
            error:
              "no target_id, so nothing was dispatched — a send must name the AGENT it goes to. " +
              'Shape: <call name="send"><target_id>agent</target_id><type>verb</type></call>. ' +
              "Send `kernel` <type>reflect</type> for the list of agent ids.",
            you_sent: omit(args, ["payload"]),
          }),
        });
      }
      continue;
    }
    const target = rawTarget.trim();
    let payload = args["payload"];
    if (!isPlain(payload) || Object.keys(payload).length === 0) {
      if (replyTo) {
        units.push({
          dest: replyTo, target, verb: "-", idx: null, title: null,
          body: renderReply({
            error:
              `send to '${target}' carried no payload, so nothing was dispatched. A call names ` +
              "its verb in `<type>`, e.g. <type>reflect</type>.",
            target,
            you_sent: omit(args, ["payload"]),
          }),
        });
      }
      continue;
    }
    // A SELF-CALL INHERITS ITS CONVERSATION: the verbs that resolve their
    // conversation from the caller's client see the one this call was made in.
    if (target === selfId && !("client_id" in payload)) payload = { ...payload, client_id: clientId };
    const item: Item = { target, payload: payload as Payload, timeout: a.sendTimeout };
    // The model's own `timeout` overrides the layer default; the kernel
    // validates it, so a bad value comes back as a readable refusal.
    if ("timeout" in args) item.timeout = args["timeout"] as number;
    // The model's execution NAME rides through untouched as the item's `id`.
    if ("exec_id" in args) item.id = args["exec_id"] as string;
    if ("retries" in args) item.retries = args["retries"] as number;
    items.push(item);
    if (replyTo) {
      units.push({
        dest: replyTo, target, verb: typeof payload["type"] === "string" ? payload["type"] : "?",
        idx: items.length - 1, body: null, title: cleanTitle(args["title"]),
      });
    }
    // reply_to null: fire-and-forget — dispatched, counted nowhere
  }
  // IN FLIGHT while the batch settles: `await` from a LATER turn must see it.
  if (items.length > 0) mem.inflight.set(selfId, (mem.inflight.get(selfId) ?? 0) + 1);
  let outcomes: Outcome[];
  try {
    outcomes = items.length > 0 ? await kernel.send(items) : [];
  } finally {
    if (items.length > 0) mem.inflight.set(selfId, Math.max(0, (mem.inflight.get(selfId) ?? 0) - 1));
  }
  // THE GRANTS CACHE IS WRITE-THROUGH: a share/unshare this turn made on its own
  // store changes who may read row bodies — reload before the next row event.
  const rec = kernel.get(selfId)?.meta ?? {};
  if (items.some((it) => it.target === rec["memory_store_id"] && ["share", "unshare"].includes(String(it.payload["type"])))) {
    await refreshGrants(selfId, kernel);
  }
  if (units.length > 0) {
    // A STABLE TOKEN PER TURN — the batch token IS the assistant row's id, so
    // `in batch <b>` and `↑<message>` name the same thing.
    const batch = a.messageId !== null ? String(a.messageId) : hex2();
    const groups = new Map<string, string[]>();
    const counts = new Map<string, number>();
    for (const u of units) {
      if (!groups.has(u.dest)) groups.set(u.dest, []);
    }
    const cap = inlineCap(rec);
    for (const u of units) {
      let body = u.body ?? "";
      let pid: Json = "-";
      if (u.idx !== null) {
        const out = outcomes[u.idx]!;
        pid = out.pid ?? "-";
        // The SAME notation the model writes calls in — never JSON.
        body = renderReply(out.result as Value);
        if (u.verb === "reflect") body += REFLECT_NUDGE;
        // A body is never persisted: over the inline cap it waits in RAM and the
        // turn carries its origin line; under it, it is inline.
        if (estimateOne({ role: "user", content: body }) > cap) {
          const size = body.length;
          if (storeReply(selfId, kernel, asId(pid), body)) {
            const item = items[u.idx]!;
            body =
              `(settled, ${size} bytes — NOT inlined, held in memory only until this agent restarts. origin: ` +
              `${originLine(out.result, u.target, u.verb, item.payload)}. To find a line in it: send yourself ` +
              `<type>grep</type> <query>substring</query> <ids><item>${String(pid)}</item></ids>)`;
          }
        }
      }
      const n = (counts.get(u.dest) ?? 0) + 1;
      counts.set(u.dest, n);
      const mTotal = units.filter((x) => x.dest === u.dest).length;
      const tag = replyTag(pid, u.target, u.verb, u.title, a.messageId, n, mTotal, batch);
      groups.get(u.dest)![n - 1] = tag + "\n" + body;
    }
    for (const [dest, blocks] of groups) deliverSoon(kernel, dest, blocks.join("\n\n"), clientId, systemPrompt);
  }
  if (awaitCalled && !doneCalled) {
    // AWAIT: "I am waiting for what is in flight; wake me when it lands." HONEST
    // OR REFUSED: with nothing of this agent's in flight and nothing queued for
    // it, awaiting is stalling, and it is told so.
    const pending =
      (mem.inflight.get(selfId) ?? 0) + (mem.queue.get(selfId) ?? []).length + (mem.locks.get(selfId)?.locked ? 1 : 0);
    if (pending > 0 || items.length > 0) {
      emitStatus(kernel, selfId, clientId, "done", { reason: "awaiting", pending });
      return;
    }
    deliverSoon(kernel, selfId, AWAIT_REFUSED, clientId, systemPrompt);
    return;
  }
  if (doneCalled) {
    // Explicitly finished — nothing more runs, nothing reminds. A caller's
    // `send_to` hand-off still applies: the finished text is what it asked for.
    if (a.sendTo !== null && units.length === 0) deliverSoon(kernel, a.sendTo, a.text, clientId, systemPrompt);
    return;
  }
  if (calls.length === 0 && units.length === 0) {
    // TRY AGAIN. Every turn, chained or not: a turn that emits nothing ends
    // nothing — the model ends it with `done`, waits with `await`, or acts.
    deliverSoon(kernel, selfId, NO_CALL_NUDGE, clientId, systemPrompt);
    return;
  }
  if (calls.length > 0) return; // the model routed its own work
  if (a.sendTo !== null) deliverSoon(kernel, a.sendTo, a.text, clientId, systemPrompt);
}

/** Hand `text` to `target` as a fresh turn WITHOUT waiting for that turn — a
 *  send to an AI agent takes its lock and returns only when the whole turn is
 *  done. The target's incarnation is captured NOW: a reply that lands after it
 *  rebooted is recorded as `late`, never acted on. */
export function deliverSoon(kernel: Kernel, target: string, text: string, clientId: string, systemPrompt: string | null): void {
  const born = aiMem(kernel).incarnation.get(target) ?? 0;
  void deliver(kernel, target, text, clientId, systemPrompt, born);
}

/** Hand `text` to `target` as a fresh turn and AWAIT it. A FAILED HANDOFF IS
 *  ANNOUNCED, not dropped: the same `status(done, reason=error)` a failed direct
 *  send produces lands on the target's own inbox. */
export async function deliver(
  kernel: Kernel,
  target: string,
  text: string,
  clientId: string,
  systemPrompt: string | null,
  born: number | null,
): Promise<void> {
  const payload: Payload = { type: "send", text, client_id: clientId };
  if (born !== null && (aiMem(kernel).incarnation.get(target) ?? 0) !== born) payload["late"] = true;
  if (systemPrompt !== null) payload["system_prompt"] = systemPrompt;
  const out = (await kernel.send([{ target, payload }]))[0]!;
  const failed = out.status === "failed" || out.status === "timed_out";
  if (failed || errorShaped(out.result)) {
    const err = errorShaped(out.result) ? (out.result as Payload)["error"] ?? out.status : out.status;
    emitStatus(kernel, target, clientId, "done", { reason: "error", error: err as Json });
  }
}
