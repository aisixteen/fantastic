import type { Kernel } from "../../kernel/kernel.ts";
import type { Json, Payload } from "../../kernel/json.ts";
import { nowSeconds, timers } from "../../kernel/timers.ts";
import { extractToolCalls } from "./tool_parse.ts";
import { withoutThinking } from "./think.ts";
import {
  FifoLock,
  MemoryStoreError,
  aiMem,
  asId,
  saveHistory,
  storeOf,
} from "./memory.ts";
import type { CurrentEntry, QueueEntry } from "./memory.ts";
import { assemble, calibrate, projectContext, renderForModel, toCaller } from "./seam.ts";
import type { Provider } from "./seam.ts";
import { contextRecall } from "./verbs.ts";
import { defaultSystemContent } from "./howtos.ts";
import { emitStatus } from "./status.ts";
import { routeReply } from "./routing.ts";

// THE TURN — port of python `ai_core/core.py` `_run` / `_make_send` /
// `_make_inquiry` / `on_cancel`. One `send` is ONE provider pass: assemble the
// view from the memory store, project it to the budget, stream the answer, record
// the incoming message and the answer as rows — then, AFTER the agent's FIFO lock
// is released, route what the answer called (routing.ts): the replies come back
// as NEW turns. The chain is the model asking again; nothing here loops.

/** recursion guard: AI→AI→… chains refuse past this depth */
export const MAX_CALL_DEPTH = 8;
export const DEFAULT_CLIENT_ID = "cli";

export interface TurnConfig {
  /** error-message prefix */
  name: string;
  /** builds (and the agent caches) the provider a turn talks to */
  makeProvider: (id: string, rec: Payload) => Provider;
  /** seconds: a generation's hard ceiling AND the per-call deadline routing
   *  puts on every dispatched item — the kernel defaults nothing */
  sendTimeout: number;
}

/** Trim and sanitize a client id: spaces, slashes and weirdness collapse to `_`. */
export function safeClient(clientId: unknown): string {
  const raw = typeof clientId === "string" ? clientId : clientId ? String(clientId) : "";
  const s = (raw || DEFAULT_CLIENT_ID).trim() || DEFAULT_CLIENT_ID;
  return [...s].map((c) => (/[\p{L}\p{N}._-]/u.test(c) ? c : "_")).join("");
}

/** The agent's provider, built once per kernel (python caches it per id too). */
export function providerFor(id: string, kernel: Kernel, cfg: TurnConfig): Provider {
  const cache = aiMem(kernel).providers;
  let p = cache.get(id);
  if (p === undefined) cache.set(id, (p = cfg.makeProvider(id, kernel.get(id)?.meta ?? {})));
  return p;
}

// EVERY ROW HAS AN ADDRESS. A delivered reply turn names the assistant turn that
// made its calls (`↑`) on its first tag; anything else is a ROOT.
const TAG_LINKS = /^\[reply to \S+ · [^\]]*?↑(\S+)[^\]]*\]/;

/** `[id, parent]` for an incoming turn — python `_row_ids_for_incoming`. The id
 *  is the kernel's next int; the parent is the `↑` on a delivered reply turn's
 *  first tag, else null (a fresh message from a person or a peer is a ROOT). */
export function rowIdsForIncoming(text: string, kernel: Kernel): [number, number | null] {
  const rid = kernel.mintId();
  const m = TAG_LINKS.exec(text);
  return [rid, m !== null ? asId(m[1]) : null];
}

class Aborted extends Error {}

interface Gen {
  aborted: boolean;
}

/**
 * ONE provider pass. Returns `{response, final, client_id, message_id}`, or the
 * projection's `{error}` (too small / unknown strategy). The incoming message and
 * the answer are appended only after the pass succeeds, their ids minted AT THE
 * APPEND so id order is the order the agent lived.
 */
async function run(
  id: string,
  userText: string,
  kernel: Kernel,
  clientId: string,
  cfg: TurnConfig,
  systemPrompt: string | null,
  gen: Gen,
): Promise<Payload> {
  const mem = aiMem(kernel);
  const provider = providerFor(id, kernel, cfg);
  if (mem.recallDue.has(id)) {
    mem.recallDue.delete(id);
    await contextRecall(id, {}, kernel, provider, await defaultSystemContent(id, kernel));
  }
  // A caller-supplied `system_prompt` REPLACES the deterministic substrate prompt.
  const sys = systemPrompt !== null && systemPrompt.trim() !== "" ? systemPrompt : await defaultSystemContent(id, kernel);
  const { messages, storeLen } = await assemble(id, kernel, userText, sys);
  // The window is read off the record EVERY turn, so a model switched to a smaller
  // one is fitted — folded — on its very next turn.
  const projected = await projectContext({
    rec: kernel.get(id)?.meta ?? {}, provider, selfId: id, kernel, clientId, messages, storeLen,
  });
  if (!Array.isArray(projected)) return projected;
  const modelMessages = renderForModel(projected);
  // A FRESH reading per pass.
  provider.lastPromptTokens = null;
  const parts: string[] = [];
  let first = true;
  for await (const chunk of provider.chat(modelMessages)) {
    if (gen.aborted) throw new Aborted();
    if (first) {
      emitStatus(kernel, id, clientId, "streaming");
      first = false;
    }
    parts.push(chunk);
    const cur = mem.current.get(id);
    if (cur !== undefined) cur.text_so_far += chunk;
    toCaller(kernel, id, clientId, { type: "token", text: chunk, source: id });
  }
  if (gen.aborted) throw new Aborted();
  const lastText = parts.join("");
  calibrate(kernel, id, provider, modelMessages);
  // `status(phase='done')` IS the completion event.
  emitStatus(kernel, id, clientId, "done", { reason: "ok" });
  const [inId, inParent] = rowIdsForIncoming(userText, kernel);
  const messageId = kernel.mintId();
  await saveHistory(id, kernel, [
    { role: "user", content: userText, id: inId, parent: inParent },
    // STRIPPED — the deliberation is not part of the conversation.
    { role: "assistant", content: withoutThinking(lastText), id: messageId, parent: inId },
  ]);
  return { response: lastText, final: lastText, client_id: clientId, message_id: messageId };
}

function hex8(): string {
  const b = new Uint8Array(8);
  (globalThis as { crypto: { getRandomValues: (a: Uint8Array) => Uint8Array } }).crypto.getRandomValues(b);
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
}

/**
 * The `send` verb: `{text, client_id?, system_prompt?, send_to?, _call_stack?}`.
 * Per-agent FIFO lock — concurrent callers serialize, and one that finds the lock
 * held gets a `queued` status. A message from another AGENT opens with a header
 * the KERNEL vouches for (`[from X · to … · msg N · re M]`). A turn that runs
 * past `sendTimeout` ends with status done/timeout. A `cancel` of this dispatch
 * closes the turn out for its client (status done/cancelled) and nothing it
 * streamed is recorded or routed.
 */
export async function send(id: string, payload: Payload, kernel: Kernel, cfg: TurnConfig): Promise<Json> {
  const mem = aiMem(kernel);
  const clientId = safeClient(payload["client_id"] ?? DEFAULT_CLIENT_ID);
  // Recursion guard — checked BEFORE the lock, so a cycle can never deadlock on it.
  const callStack = Array.isArray(payload["_call_stack"]) ? (payload["_call_stack"] as Json[]) : [];
  if (callStack.includes(id)) {
    return { error: `${cfg.name}: cycle detected`, response: "", cycle: [...callStack, id], client_id: clientId };
  }
  if (callStack.length >= MAX_CALL_DEPTH) {
    return { error: `${cfg.name}: max call depth ${MAX_CALL_DEPTH} reached`, response: "", client_id: clientId };
  }
  const systemPrompt = typeof payload["system_prompt"] === "string" ? payload["system_prompt"] : null;
  // NO STORE, NO TURN — said up front, not discovered mid-generation.
  try {
    storeOf(id, kernel);
  } catch (e) {
    return { error: (e as Error).message, reason: "missing_arg", client_id: clientId };
  }
  if (payload["late"] === true) {
    // A reply to a call a PREVIOUS incarnation made: recorded in this agent's own
    // store, never a turn.
    await saveHistory(id, kernel, [
      {
        role: "user",
        parent: null,
        kind: "late",
        content:
          "[late reply — answered a call made before this agent last booted; recorded, not acted on]\n" +
          String(payload["text"] ?? ""),
      },
    ]);
    return { late: true, client_id: clientId };
  }
  const sendToRaw = payload["send_to"];
  const sendTo = typeof sendToRaw === "string" && sendToRaw.trim() !== "" ? sendToRaw : null;
  let text = payload["text"] ?? "";
  // A message is TEXT whatever the caller typed: the tag grammar types a bare number.
  if (typeof text === "number") text = String(text);
  let textStr = String(text);
  // WHO SAID IT, TO WHOM, UNDER WHICH NUMBER — the dispatch's own caller, never
  // the sender's prose.
  const sender = kernel.currentCaller;
  if (sender !== null && sender !== id) {
    const parts = [`from ${sender}`];
    const to = payload["to"];
    if (Array.isArray(to) && to.length > 0) parts.push("to " + to.map((t) => String(t)).join(", "));
    if (payload["msg"] !== undefined && payload["msg"] !== null) parts.push(`msg ${String(payload["msg"])}`);
    if (payload["re"] !== undefined && payload["re"] !== null) parts.push(`re ${String(payload["re"])}`);
    textStr = `[${parts.join(" · ")}]\n${textStr}`;
  }
  const entry: QueueEntry = { client_id: clientId, text: textStr, send_id: hex8(), queued_at: nowSeconds() };
  const queue = mem.queue.get(id) ?? [];
  mem.queue.set(id, queue);
  queue.push(entry);
  let lock = mem.locks.get(id);
  if (lock === undefined) mem.locks.set(id, (lock = new FifoLock()));
  if (lock.locked) {
    emitStatus(kernel, id, clientId, "queued", { send_id: entry.send_id, ahead: Math.max(0, queue.length - 1) });
  }
  const dequeue = (): void => {
    const q = mem.queue.get(id) ?? [];
    const i = q.indexOf(entry);
    if (i >= 0) q.splice(i, 1);
  };
  // THE CANCEL CLOSE-OUT (python `on_cancel`): a `cancel` of THIS dispatch —
  // queued or generating — drops its queue row, clears the in-flight entry so
  // `status` stops reporting a phantom, and tells the client the turn ended.
  const gen: Gen = { aborted: false };
  const taskId = kernel.currentTask;
  const task = taskId !== null ? kernel.tasks.get(taskId) : undefined;
  task?.onCancel.push(() => {
    gen.aborted = true;
    dequeue();
    const cur = mem.current.get(id);
    if (cur !== undefined && cur.task_id === taskId) {
      mem.current.delete(id);
      emitStatus(kernel, id, cur.client_id, "done", { reason: "cancelled", send_id: cur.send_id, started_at: cur.started_at });
    }
  });
  await lock.acquire();
  let reply: Payload;
  try {
    if (gen.aborted) return { cancelled: true };
    dequeue();
    const cur: CurrentEntry = { ...entry, started_at: nowSeconds(), phase: "thinking", text_so_far: "", task_id: taskId };
    mem.current.set(id, cur);
    emitStatus(kernel, id, clientId, "thinking");
    const t = timers();
    let timer: unknown = null;
    const timedOut = new Promise<"timeout">((res) => {
      timer = t.setTimeout(() => res("timeout"), cfg.sendTimeout * 1000);
    });
    // The pass keeps running until its next chunk after a timeout or a cancel
    // (JS cannot abort it); it then throws `Aborted` into a promise nobody reads.
    const running = run(id, textStr, kernel, clientId, cfg, systemPrompt, gen);
    running.catch(() => undefined);
    try {
      const outcome = await Promise.race([running, timedOut]);
      if (outcome === "timeout") {
        gen.aborted = true;
        emitStatus(kernel, id, clientId, "done", { reason: "timeout" });
        return { error: `send: timeout after ${cfg.sendTimeout}s`, client_id: clientId };
      }
      reply = outcome;
    } catch (e) {
      if (e instanceof Aborted) return { cancelled: true };
      if (e instanceof MemoryStoreError) {
        // FAIL FAST: the store refused or is gone — no turn, said so.
        emitStatus(kernel, id, clientId, "done", { reason: "error", error: e.message });
        return { error: e.message, reason: "memory_store", client_id: clientId };
      }
      emitStatus(kernel, id, clientId, "done", { reason: "error", error: e instanceof Error ? e.message : String(e) });
      throw e;
    } finally {
      t.clearTimeout(timer);
      if (mem.current.get(id)?.task_id === taskId) mem.current.delete(id);
    }
  } finally {
    lock.release();
  }
  // OUTSIDE the lock, DETACHED: a self-addressed follow-up queues behind this
  // turn instead of deadlocking on it, and this dispatch returns at once.
  if (!("error" in reply)) {
    void routeReply({
      selfId: id,
      kernel: kernel.scopedFor(id, null),
      text: String(reply["response"] ?? ""),
      clientId,
      sendTo,
      systemPrompt,
      sendTimeout: cfg.sendTimeout,
      incoming: textStr,
      messageId: typeof reply["message_id"] === "number" ? reply["message_id"] : null,
    });
  }
  return reply;
}

/**
 * The `inquiry` verb on an AI: the same turn `send` runs, answered as the REPLY
 * — `{answer, request_id}`, the answer the model emits with `inquiry_answer`.
 * A turn that never answered did not answer, and the asker is told so.
 */
export async function inquiry(id: string, payload: Payload, kernel: Kernel, cfg: TurnConfig): Promise<Json> {
  const q = payload["text"];
  if (typeof q !== "string" || q.trim() === "") {
    return { error: "inquiry: `text` must be a non-empty string", reason: "missing_arg", arg: "text" };
  }
  let text = q;
  const opts = payload["options"];
  if (Array.isArray(opts) && opts.length > 0) text += "\n\nOptions: " + opts.map((o) => String(o)).join(", ");
  const act = payload["action"];
  if (act !== null && typeof act === "object" && !Array.isArray(act)) text += "\n\nProposed action: " + JSON.stringify(act);
  const out = await send(id, { ...payload, type: "send", text }, kernel, cfg);
  if (out !== null && typeof out === "object" && !Array.isArray(out) && out["error"]) return out;
  const response = out !== null && typeof out === "object" && !Array.isArray(out) ? String(out["response"] ?? "") : "";
  const said = extractToolCalls(response).filter((c) => c.name === "inquiry_answer");
  if (said.length === 0) {
    return {
      error: 'you were sent an inquiry and did not answer it: emit <call name="inquiry_answer"><answer>what you concluded</answer></call>',
      reason: "unhandled",
      request_id: payload["request_id"] ?? null,
    };
  }
  const answer = said[said.length - 1]!.arguments["answer"];
  // THE PLACEHOLDER IS NOT AN ANSWER.
  if (typeof answer !== "string" || answer.trim().replace(/^\.+|\.+$/g, "") === "") {
    return {
      error: `that is the PLACEHOLDER, not an answer — \`answer\` came back as ${JSON.stringify(answer ?? null)}. Say what you actually concluded.`,
      reason: "bad_arg",
      arg: "answer",
      request_id: payload["request_id"] ?? null,
    };
  }
  return { answer, request_id: payload["request_id"] ?? null };
}
