import type { AIRecord, Message } from "./ai_backend.ts";
import type { Provider } from "./seam.ts";

// ResponsesProvider — the OpenAI Responses API (`POST …/v1/responses`), RAW TEXT.
// The ts port of python `responses_backend.provider` (canonical; rust and swift
// match it). An app picks it for its `ai_backend` agents through
// `registerAIBackend(…, { makeProvider: (_id, rec) => new ResponsesProvider(rec) })`.
//
// STATELESS: `store:false` and the whole conversation as typed `input` items every
// pass — the conversation is the agent's (its memory store), never the endpoint's.
// The system block rides as `instructions`. NO NATIVE TOOLS: the `<call>` grammar is
// taught in the prompt, and a `function_call` output item is a loud error.
//
// `endpoint` is the FULL `…/v1/responses` URL, used verbatim (python's meaning of
// the field), and `model` is required — nothing is guessed. The key is the record's
// `api_key` (a browser has no environment); absent, no Authorization header is sent,
// which is what a keyless local server or proxy expects.

/** Per-turn output budget (`max_output_tokens`) when the record names none. */
export const RESPONSES_MAX_TOKENS = 8192;

/** A CDN/proxy error page on one of these codes (non-JSON body) never reached
 *  anything that could judge the request — retried, bounded. Anything the endpoint
 *  answered deliberately (a 4xx, any JSON body) stays loud. */
const TRANSPORT_CODES = new Set([502, 503, 504, 520, 521, 522, 523, 524]);
const RETRIES = 2;
const BACKOFF = 1.5;

/** Reasoning deltas — raw chain of thought, or OpenAI's own summary: the thought
 *  channel, wrapped, never output. */
const REASONING_DELTAS = new Set(["response.reasoning_text.delta", "response.reasoning_summary_text.delta"]);
/** Output deltas. A refusal is text the model produced. */
const TEXT_DELTAS = new Set(["response.output_text.delta", "response.refusal.delta"]);

/** python `_to_responses`: the LAST system message wins; an empty user/assistant
 *  content is `(no content)`; every other role is dropped. */
export function toResponses(messages: Message[]): { instructions: string | null; items: unknown[] } {
  let instructions: string | null = null;
  const items: unknown[] = [];
  for (const m of messages) {
    const content = typeof m.content === "string" ? m.content : "";
    if (m.role === "system") {
      instructions = content;
    } else if (m.role === "user" || m.role === "assistant") {
      items.push({
        type: "message",
        role: m.role,
        content: [{ type: m.role === "user" ? "input_text" : "output_text", text: content || "(no content)" }],
      });
    }
  }
  return { instructions, items };
}

type FetchLike = (url: string, init: { method: string; headers: Record<string, string>; body: string }) => Promise<Response>;

export class ResponsesProvider implements Provider {
  // Written out rather than as constructor parameter properties: node's
  // strip-only TypeScript rejects those.
  readonly rec: AIRecord;
  readonly fetchImpl: FetchLike;
  lastPromptTokens: number | null = null;
  lastStopReason: string | null = null;

  constructor(rec: AIRecord, fetchImpl?: FetchLike) {
    this.rec = rec;
    this.fetchImpl = fetchImpl ?? ((url, init) => fetch(url, init));
  }

  /** The request body in python's key order: model, input, max_output_tokens,
   *  stream, store, instructions, then the record's sampling knobs. */
  body(messages: Message[], maxTokens?: number): Record<string, unknown> {
    const rec = this.rec;
    const { instructions, items } = toResponses(messages);
    const body: Record<string, unknown> = {
      model: rec.model,
      input: items,
      // A SIDE CALL'S OWN BOUND (a fold summary's budget) — else the record's.
      max_output_tokens: maxTokens ? Math.floor(maxTokens) : rec.max_tokens || RESPONSES_MAX_TOKENS,
      stream: true,
      store: false,
    };
    if (instructions) body["instructions"] = instructions;
    for (const [k, v] of Object.entries(rec.sampling ?? {})) if (v !== null && v !== undefined) body[k] = v;
    return body;
  }

  async *chat(messages: Message[], maxTokens?: number): AsyncIterable<string> {
    const rec = this.rec;
    if (!rec.endpoint) throw new Error("responses: `endpoint` (the full …/v1/responses URL) is required");
    if (!rec.model) throw new Error("responses: `model` is required");
    const headers: Record<string, string> = { "content-type": "application/json", accept: "text/event-stream" };
    if (rec.api_key) headers["authorization"] = `Bearer ${rec.api_key}`;
    const payload = JSON.stringify(this.body(messages, maxTokens));

    let resp: Response;
    for (let attempt = 0; ; ) {
      resp = await this.fetchImpl(rec.endpoint, { method: "POST", headers, body: payload });
      if (resp.status < 400) break;
      const detail = await resp.text();
      const ctype = (resp.headers.get("content-type") ?? "").toLowerCase();
      if (!(TRANSPORT_CODES.has(resp.status) && !ctype.includes("json") && attempt < RETRIES)) {
        throw new Error(`responses ${resp.status}: ${detail}`);
      }
      attempt += 1;
      await new Promise((r) => setTimeout(r, BACKOFF * attempt * 1000));
    }
    if (!resp.body) throw new Error("responses: the endpoint returned no body");

    const reader = resp.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    let thinking = false;
    // The terminal response object — null until one arrives. A stream that ends
    // without one did not finish.
    let final: any = null;
    // What the stream CONTAINED: every output item's type, in order.
    const kinds: unknown[] = [];
    read: while (true) {
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
        let evt: any;
        try {
          evt = JSON.parse(data);
        } catch {
          continue;
        }
        const etype = String(evt?.type ?? "");
        if (REASONING_DELTAS.has(etype)) {
          // A REASONING MODEL SPLITS ITS OUTPUT: reading the output deltas alone
          // returns a turn spent thinking as an EMPTY completion.
          const t = typeof evt.delta === "string" ? evt.delta : "";
          if (t) {
            if (!thinking) {
              thinking = true;
              yield "<think>";
            }
            yield t;
          }
        } else if (TEXT_DELTAS.has(etype)) {
          const t = typeof evt.delta === "string" ? evt.delta : "";
          if (t) {
            if (thinking) {
              thinking = false;
              yield "</think>";
            }
            yield t;
          }
        } else if (etype === "response.output_item.added") {
          const item = evt.item ?? {};
          kinds.push(item.type ?? null);
          if (item.type === "function_call") {
            throw new Error(
              `responses stream carried a native function_call item (${JSON.stringify(item.name ?? null)}): ` +
                "the model wrote a JSON tool call, which this kernel does not parse — calls are `<call name=…>` text",
            );
          }
        } else if (etype === "response.completed" || etype === "response.incomplete") {
          final = evt.response ?? {};
          // PROVIDER-REPORTED USAGE — what the window really held.
          const n = Number(final?.usage?.input_tokens ?? 0);
          this.lastPromptTokens = n > 0 ? n : null;
          break read;
        } else if (etype === "response.failed") {
          throw new Error(`responses stream failed: ${JSON.stringify(evt.response?.error ?? null)}`);
        } else if (etype === "error") {
          throw new Error(`responses stream error: ${JSON.stringify(evt)}`);
        }
      }
    }
    // A turn spent ENTIRELY reasoning ends inside the span — balance it.
    if (thinking) yield "</think>";
    const status = final?.status ?? null;
    const reason = final?.incomplete_details?.reason ?? null;
    this.lastStopReason = reason === "max_output_tokens" ? "max_tokens" : (reason || status || null);
    // A BOUNDED SIDE CALL cut at its bound is an ANSWER: the caller asked for at
    // most `maxTokens` and reads `lastStopReason`.
    if (maxTokens !== undefined && this.lastStopReason === "max_tokens") return;
    if (status !== "completed") {
      throw new Error(
        `responses stream ended without completing: status=${JSON.stringify(status)} ` +
          `reason=${JSON.stringify(reason)} items=${JSON.stringify(kinds)}` +
          (final === null ? " (no terminal event — connection cut mid-stream)" : ""),
      );
    }
  }
}
