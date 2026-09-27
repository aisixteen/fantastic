import type { Kernel } from "../../kernel/kernel.ts";
import type { Json, Payload } from "../../kernel/json.ts";
import { MISSING_ARG, NOT_FOUND, BAD_ARG, NO_CONSUMER, UNAUTHORIZED, refusal } from "../../kernel/refusal.ts";
import {
  MemoryStoreError,
  addWatcher,
  aiMem,
  asId,
  forkFraming,
  myGrants,
  removeWatcher,
  resetIncarnation,
  saveHistory,
  storeCall,
  storeOf,
} from "./memory.ts";
import type { Row } from "./memory.ts";
import type { Provider } from "./seam.ts";
import { contextClear, contextRecall, contextStatus, grep, history, recall } from "./verbs.ts";
import { CANCEL_DOC, defaultSystemContent } from "./howtos.ts";
import { statusSnapshot } from "./status.ts";
import { deliverSoon } from "./routing.ts";
import { inquiry, providerFor, safeClient, send } from "./turn.ts";
import type { TurnConfig } from "./turn.ts";

// ai_backend — an LLM agent that runs IN THE BROWSER: the ts port of python's
// `ai_core` (the canonical AI agent; rust and swift match it).
//
// TOOL-CALLING IS RAW, exactly as in python/rust/swift. No provider's native
// tool API is ever used: the system block teaches the `<call>` tag grammar
// (howtos.ts), the model writes calls as text, `tool_parse` reads them back, and
// the replies come back in the same tag notation (never JSON).
//
// ONE `send` IS ONE PROVIDER PASS (turn.ts). What the answer called is routed
// after the pass (routing.ts): the calls dispatch as one batch and their replies
// arrive as a NEW turn — the chain continues because the model asks again; `done`
// ends it, `await` waits, silence is nudged. The conversation lives in the
// agent's memory store (memory.ts) and its view is projected to the budget every
// turn (seam.ts).
//
// WHERE THE KEY LIVES. A browser backend holds its credential in the browser,
// so this is safe against a local `llama-server` (no key at all), a keyless
// proxy, or a user's own key in their own browser — and is NOT a way to ship a
// shared key to strangers. `endpoint` is required for exactly that reason:
// nothing here guesses a host to send a key to.

/** python `SEND_TIMEOUT`: a generation's hard ceiling and the per-call deadline
 *  routing puts on every dispatched item (the kernel defaults nothing). */
export const SEND_TIMEOUT = 180;

export interface AIRecord {
  endpoint?: string;
  model?: string;
  api_key?: string;
  /** The memory store this agent keeps its conversation in. REQUIRED — no store,
   *  no turn (fail loud). */
  memory_store_id?: string;
  /** The model's window (tokens) → the budget, the trigger and the inline cap. */
  context_window?: number;
  num_ctx?: number;
  output_reserve?: number;
  /** compact (default) | truncate | strip | fold */
  context_strategy?: string;
  /** turns `compact` keeps verbatim (default 6) */
  recent_n?: number;
  /** teach call titles in the system block */
  context_titles?: boolean;
  /** teach the large-reply / fold / thread how-to in the system block */
  context_async?: boolean;
  /** llama.cpp gates its thought channel behind the chat template's
   *  `enable_thinking`. Undefined means DO NOT SEND it — a model without a
   *  thinking gate rejects an unknown kwarg. */
  think?: boolean;
}

/** One chat message as the wire wants it. */
export type Message = { role: string; content: string };

/** Verb → its one-line doc (python's docstrings). The kernel answers `reflect`
 *  for every agent, so these reach a reader through the bundle readme. */
export const AI_VERBS: { [verb: string]: string } = {
  reflect: "Identity + model + endpoint + generating flag + context posture. No args.",
  send: "args: text:str (req), client_id:str? (default 'cli'). Streams tokens to ONLY the caller — cli (stdout) for client_id='cli', or the browser tab whose WS is subscribed and filters by client_id otherwise. Persists per-client chat.json. Per-backend FIFO lock: concurrent callers serialize. If the lock is held when this call arrives, emits a `status` event (phase='queued', detail.ahead) for the caller; first `token` (or `status` of phase != queued) for the same client_id implicitly unqueues. Optional system_prompt:str REPLACES the auto-built prompt (caller-supplied role/context — read it from a state agent yourself, no yaml coupling here). The conversation is always this agent's own (its memory store). _call_stack is reserved for the recursion guard (cycle/over-depth refusal before the lock). Optional send_to:str hands this pass's TEXT to that agent id as a fresh turn, once. The model routes its own work with `<reply_to>` inside its calls. Returns {response, final, client_id}.",
  inquiry: "args: text:str (req), kind:str? (text|single|multi|confirm), options:list?, action:dict?, request_id:str?, client_id:str?. Sends THIS agent an inquiry and returns {answer, request_id} — the answer is the one it emits with the `answer` tool. The same verb a human answers; a sender writes one call for either.",
  history: "args: fold:int? (a fold or clear id — list ITS members instead), limit:int? (default: as many as fit inline), after:int? (a row id — page forward from it). The conversation at DIGEST resolution — {id, parent, kind, role, head, bytes} per row, oldest first, paged. With `fold`, that fold's title + summary and its members (one level down; a member that is itself a fold opens the same way). A row's text is behind `recall` / `grep`. Returns {rows, next, total, fold?}.",
  status: "args: client_id:str? — when provided, returns a privacy-filtered snapshot: full text only for this client's queue entries / current generation; other clients collapse to `others_pending` (an integer count). Without client_id the snapshot redacts text everywhere. Used by chat UIs to rebuild queued/in-flight state on page reload (and for headless polling). Returns {source, client_id, generating, current:{phase, send_id, started_at, elapsed, is_mine, text?, text_so_far?, last_tool?}|null, mine_pending:[{send_id,text,queued_at}], others_pending:int}.",
  context_status: "No args. The context-budget posture + the last overflow projection + the model's",
  recall: "args: query?:str (case-insensitive substring over the serialized turn — matches tool args/replies too), limit?:int (default 20, max 100), before?:int (store index — page backward). Pages turns back from the DURABLE store (the FULL conversation, never trimmed — so anything compaction dropped is one call away). Read-only. A page always fits INLINE (the cap); trimmed from the oldest end when it would not, with the index to continue from. Returns {messages:[{index,role,content}], total, truncated, note?}.",
  grep: "args: query:str (req — case-insensitive substring), ids:list[int]? (row ids / pids / fold ids to search; a fold id searches its members), limit:int? (matches, default: as many as fit inline), width:int? (chars of each matched line returned, from its start; default 200 — raise it when the value you need sits deep in a long line). Finds the LINE that mentions something ANYWHERE in your memory — every row you ever wrote or received (behind every clear, inside every fold), the rows other agents SHARED with you, and the reply bodies still held in memory this run. Returns {matches:[{id, line, text}], total, truncated}.",
  context_recall: "args: dry:bool? (mechanical fit only — no model call). Rebuild this agent's VIEW from its store: fit the head to the budget at digest resolution, then ask the model which rows it wants in full, persist the id list, log the decision as a row, and drop RAM scratch. Runs on boot, after a fold batch, or on request. Returns {kept, expanded, dropped, budget, why, degraded}.",
  context_clear: "args: why:str? — start a NEW THREAD on the same store: a CLEAR-FOLD — one row whose members are every row since the previous clear (no summary) — and the model's view begins at its `[cleared <id> · N rows]` line. Nothing is deleted or forgotten: grep, recall and `history {fold: <id>}` reach every member. Empties the persisted view and drops RAM scratch (projection, fold counters, pending recall). Returns {cleared:id, rows:N}.",
  fork_from: "args: owner:str (req), grant:int (req — the grant `owner` gave you), message:str (req — the task this fork starts on), client_id:str?. Makes THIS agent a fork of `owner`'s thread: the granted rows become read-only context in your view — read back under the grant every turn, so a revoked grant fails your next turn — followed by the framing line and `message`, and the turn STARTS AT ONCE. Returns {forked:true, fork:<row id>, rows:N}.",
  request_share: "args: row:int (req), why:str?. Asks THIS agent — the OWNER of `row` — to share it with the caller. The owner's model decides: it calls its store's `share`, or declines. Arrives as a turn; returns {requested:true} at once.",
  watch: "args: body:bool? (default false), watcher:str? (operator only — an inbox id to deliver to). Announce every row this agent appends to YOUR inbox as {type:'row', source, row:{id, kind, role, bytes}} — content-free; with body:true the row's `content` too, but only for rows a live grant from this agent gives you (share). Needs a registered inbox. Returns {watching:true, watcher, body}.",
  unwatch: "args: watcher:str? (operator only). Stop announcing rows to you. Returns {watching:false}.",
  boot: "LAZY: never touches the memory store. Checks the record names one (fail loud otherwise), starts a new incarnation, and marks the view for rebuilding — the first turn reads the head from the store and runs `context_recall`.",
};

export const AI_README =
  "Browser-resident LLM agent (OpenAI-compatible; raw prompt-and-parse tool-calling in the `<call>` tag grammar). " +
  "Its conversation lives in the memory store its record names (`memory_store_id`). " +
  CANCEL_DOC + "\n\nVerbs:\n" + Object.entries(AI_VERBS).map(([v, d]) => `- ${v}: ${d}`).join("\n");

/**
 * An OpenAI-compatible streaming provider — `llama-server`, and anything else
 * that speaks `/chat/completions`.
 *
 * READS BOTH CHANNELS. A reasoning model streams its thought as
 * `reasoning_content` and only its conclusion as `content`; reading `content`
 * alone silently drops whole turns. The thought is wrapped once in
 * `<think>…</think>` so it can be parsed for calls and stripped before display.
 *
 * REPORTS what the compaction seam needs: the endpoint's own prompt count
 * (`usage.prompt_tokens`, asked for with `stream_options.include_usage`) and
 * whether the stream stopped AT a side call's `maxTokens` (`finish_reason:
 * "length"` → `lastStopReason = "max_tokens"`).
 */
export class OpenAIProvider implements Provider {
  // Written out rather than as a constructor parameter property: node's
  // strip-only TypeScript rejects those, and these tests run under `node --test`
  // with no build step.
  readonly rec: AIRecord;
  lastPromptTokens: number | null = null;
  lastStopReason: string | null = null;

  constructor(rec: AIRecord) {
    this.rec = rec;
  }

  async *chat(messages: Message[], maxTokens?: number): AsyncIterable<string> {
    const rec = this.rec;
    if (!rec.endpoint) throw new Error("ai_backend: `endpoint` is required");
    const body: Record<string, unknown> = {
      messages,
      stream: true,
      // PROVIDER-REPORTED USAGE on the last chunk (`usage.prompt_tokens`).
      stream_options: { include_usage: true },
    };
    if (rec.model) body.model = rec.model;
    if (maxTokens !== undefined) body.max_tokens = Math.floor(maxTokens); // a side call's own bound
    if (rec.think !== undefined) {
      body.chat_template_kwargs = { enable_thinking: rec.think };
    }
    const headers: Record<string, string> = { "content-type": "application/json" };
    if (rec.api_key) headers.authorization = `Bearer ${rec.api_key}`;

    const resp = await fetch(`${rec.endpoint.replace(/\/$/, "")}/chat/completions`, {
      method: "POST",
      headers,
      body: JSON.stringify(body),
    });
    if (!resp.ok) throw new Error(`ai_backend: provider HTTP ${resp.status}`);
    if (!resp.body) throw new Error("ai_backend: provider returned no body");

    const reader = resp.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    let thinking = false;
    let finished = false;
    while (!finished) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      let nl: number;
      while ((nl = buf.indexOf("\n")) !== -1) {
        const line = buf.slice(0, nl).replace(/\r$/, "");
        buf = buf.slice(nl + 1);
        if (!line.startsWith("data:")) continue;
        const data = line.slice(5).trim();
        if (!data) continue;
        if (data === "[DONE]") {
          finished = true;
          break;
        }
        let obj: any;
        try {
          obj = JSON.parse(data);
        } catch {
          continue;
        }
        const usage = obj?.usage;
        if (usage && typeof usage.prompt_tokens === "number" && usage.prompt_tokens > 0) {
          // what the window really held — the seam calibrates with it
          this.lastPromptTokens = usage.prompt_tokens;
        }
        const choice = obj?.choices?.[0];
        if (!choice) continue;
        if (typeof choice.finish_reason === "string" && choice.finish_reason) {
          this.lastStopReason = choice.finish_reason === "length" ? "max_tokens" : choice.finish_reason;
        }
        const delta = choice.delta;
        if (!delta) continue;
        const think = delta.reasoning_content;
        if (typeof think === "string" && think) {
          if (!thinking) {
            thinking = true;
            yield "<think>";
          }
          yield think;
        }
        const content = delta.content;
        if (typeof content === "string" && content) {
          if (thinking) {
            thinking = false;
            yield "</think>";
          }
          yield content;
        }
      }
    }
    // The turn ended while still reasoning — close the span so the text a
    // parser reads stays balanced.
    if (thinking) yield "</think>";
  }
}

export interface AIBackendOptions {
  /** Builds the provider a turn talks to (tests pass a scripted one). */
  makeProvider?: (id: string, rec: AIRecord) => Provider;
  /** seconds — `SEND_TIMEOUT` unless a test shortens it */
  sendTimeout?: number;
}

/**
 * The bundle handler — python `ai_core.build()`'s verb table. `ai_view` fronts it
 * unchanged: it speaks send/history/status and renders the status, token and
 * context events on this agent's inbox.
 */
export function makeAIBackend(getRecord: (id: string) => AIRecord, opts: AIBackendOptions = {}) {
  const cfg: TurnConfig = {
    name: "ai_backend",
    makeProvider: (id) => (opts.makeProvider ?? ((_i: string, rec: AIRecord) => new OpenAIProvider(rec)))(id, getRecord(id)),
    sendTimeout: opts.sendTimeout ?? SEND_TIMEOUT,
  };
  return async function handler(id: string, payload: Payload, kernel: Kernel): Promise<Json> {
    const p = payload as any;
    const verb = String(p.type ?? "");
    try {
      switch (verb) {
        case "boot": {
          // LAZY: never touches the memory store. Checks the record names one
          // (fail loud otherwise), starts a new incarnation, and marks the view
          // for rebuilding — the first turn reads the head and runs context_recall.
          const mem = aiMem(kernel);
          mem.incarnation.set(id, (mem.incarnation.get(id) ?? 0) + 1);
          try {
            storeOf(id, kernel);
          } catch (e) {
            return refusal(MISSING_ARG, (e as Error).message, { extra: { arg: "memory_store_id" } });
          }
          resetIncarnation(id, kernel);
          mem.recallDue.add(id);
          return null;
        }
        case "shutdown":
          return null;
        case "send":
          return await send(id, payload, kernel, cfg);
        case "inquiry":
          return await inquiry(id, payload, kernel, cfg);
        case "status":
          return statusSnapshot(kernel, id, p.client_id ? safeClient(p.client_id) : null);
        case "history":
          return await history(id, payload, kernel);
        case "recall":
          return await recall(id, payload, kernel);
        case "grep":
          return await grep(id, payload, kernel);
        case "context_status":
          return await contextStatus(id, kernel);
        case "context_recall":
          return await contextRecall(
            id, payload, kernel,
            p.dry === true ? null : providerFor(id, kernel, cfg),
            await defaultSystemContent(id, kernel),
          );
        case "context_clear":
          return await contextClear(id, payload, kernel);
        case "watch": {
          // THE ROW EVENT: this agent announces every row it appends to YOUR
          // inbox — content-free; with body:true the row's content too, but only
          // for rows a live grant from this agent gives you. Who may watch is
          // this agent's own gate.
          const caller = kernel.currentCaller;
          let watcher = caller;
          if (caller === null) {
            if (typeof p.watcher !== "string" || p.watcher === "") {
              return refusal(MISSING_ARG, "watch: the operator must name a `watcher` inbox", { extra: { arg: "watcher" } });
            }
            watcher = p.watcher;
          } else if (p.watcher !== undefined && p.watcher !== null && p.watcher !== caller) {
            return refusal(UNAUTHORIZED, "watch: an agent watches for ITSELF — `watcher` is the operator's");
          }
          const w = watcher as string;
          if (!kernel.hasInbox(w)) {
            return refusal(NO_CONSUMER, `watch: '${w}' has no registered inbox, so nothing would read the row events`, {
              hint: `register the reader first — kernel.onInbox('${w}', …) — then watch`,
            });
          }
          const body = p.body === true;
          addWatcher(id, kernel, w, body);
          if (body) await myGrants(id, kernel); // warm: the body check reads the cache
          return { watching: true, watcher: w, body };
        }
        case "unwatch": {
          const watcher = kernel.currentCaller ?? (typeof p.watcher === "string" ? p.watcher : null);
          if (watcher !== null) removeWatcher(id, kernel, watcher);
          return { watching: false, watcher };
        }
        case "fork_from":
          return await forkFrom(id, p as Payload, kernel);
        case "request_share": {
          // Asks THIS agent — the OWNER of `row` — to share it with the caller.
          // The owner's MODEL decides: it calls its store's `share`, or declines.
          const row = asId(p.row);
          if (row === null) return refusal(MISSING_ARG, "request_share: `row` (an int) is required", { extra: { arg: "row" } });
          const caller = kernel.currentCaller ?? "kernel";
          const why = String(p.why ?? "").split(/\s+/).filter(Boolean).join(" ").slice(0, 300);
          const store = kernel.get(id)?.meta["memory_store_id"] ?? null;
          const text =
            `[request_share from ${caller} · row ${row}] ${why}\n` +
            `${caller} asks you to share row ${row} of YOUR conversation. To share it: send ` +
            `your memory store \`${String(store)}\` <type>share</type> <with>${caller}</with> ` +
            `<scope><row>${row}</row></scope> <why>…</why>, then tell ${caller} the grant ` +
            `id. To decline, tell ${caller} so.`;
          deliverSoon(kernel.scopedFor(id, null), id, text, "cli", null);
          return { requested: true, owner: id, row };
        }
        default:
          return { error: `ai_backend: unknown type ${JSON.stringify(verb)}` } as Json;
      }
    } catch (e) {
      // FAIL FAST: the store refused or is gone — no turn, said so.
      if (e instanceof MemoryStoreError) {
        return { error: e.message, reason: "memory_store", client_id: String(p.client_id ?? "cli") };
      }
      throw e;
    }
  };
}

/** Register the bundle under `handlerModule`, with its readme. */
export function registerAIBackend(
  kernel: Kernel,
  handlerModule: string,
  getRecord: (id: string) => AIRecord,
  opts: AIBackendOptions = {},
): void {
  kernel.registerBundle(handlerModule, makeAIBackend(getRecord, opts));
  kernel.setBundleReadme(handlerModule, AI_README);
}

async function forkFrom(id: string, p: Payload, kernel: Kernel): Promise<Json> {
  const owner = typeof p["owner"] === "string" ? p["owner"].trim() : "";
  const message = typeof p["message"] === "string" ? p["message"] : "";
  if (owner === "") return refusal(MISSING_ARG, "fork_from: `owner` is required", { extra: { arg: "owner" } });
  if (message.trim() === "") return refusal(MISSING_ARG, "fork_from: `message` is required", { extra: { arg: "message" } });
  const grant = asId(p["grant"]);
  if (grant === null) return refusal(MISSING_ARG, "fork_from: `grant` (an int) is required", { extra: { arg: "grant" } });
  let shared: Row[];
  try {
    const given = await storeCall(id, kernel, { type: "grants", side: "received" });
    const g = ((given["grants"] as Payload[] | undefined) ?? []).find((x) => x["grant"] === grant);
    if (g === undefined) {
      return refusal(NOT_FOUND, `fork_from: no grant ${grant} was given to you`, { extra: { arg: "grant" } });
    }
    if (g["owner"] !== owner) {
      return refusal(BAD_ARG, `fork_from: grant ${grant} was given by ${String(g["owner"])}, not ${owner}`, {
        extra: { arg: "owner" },
      });
    }
    const res = await storeCall(id, kernel, { type: "read", grant });
    shared = (res["rows"] as Row[] | undefined) ?? [];
  } catch (e) {
    if (e instanceof MemoryStoreError) return refusal(NOT_FOUND, e.message, { extra: { arg: "grant" } });
    throw e;
  }
  const asOf = shared.reduce((m, r) => Math.max(m, typeof r.id === "number" ? r.id : 0), 0);
  const rid = kernel.mintId();
  await saveHistory(id, kernel, [
    {
      id: rid,
      role: "user",
      parent: null,
      kind: "fork",
      owner,
      grant,
      as_of: asOf,
      content: `[fork of ${owner} · grant ${grant} · as of row ${asOf} · ${shared.length} rows shared read-only]`,
    },
  ]);
  const clientId = typeof p["client_id"] === "string" && p["client_id"] !== "" ? p["client_id"] : "cli";
  deliverSoon(kernel.scopedFor(id, null), id, `${forkFraming(owner, asOf)}\n\n${message}`, safeClient(clientId), null);
  return { forked: true, fork: rid, rows: shared.length };
}
