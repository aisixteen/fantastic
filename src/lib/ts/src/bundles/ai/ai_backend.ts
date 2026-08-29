import type { Kernel } from "../../kernel/kernel.ts";
import type { Json, Payload } from "../../kernel/json.ts";
import { extractToolCalls, malformedCalls } from "./tool_parse.ts";

// ai_backend — an LLM agent that runs IN THE BROWSER.
//
// Until now every AI backend lived on the host and the browser only carried
// `ai_view`, a chat UI pointing at one by `backend_id`. This is the backend
// itself: the browser kernel talks to a model directly and drives its own
// agentic loop, so a page can be a complete AI system with no host at all.
//
// TOOL-CALLING IS RAW, exactly as in python/rust/swift. No provider's native
// tool API is ever used: the system prompt teaches ONE tool (`send`), the model
// writes it as text, `tool_parse` reads it back, and this loop dispatches it
// through the kernel. That is what lets the same model drive any of the four
// kernels without knowing which it is talking to.
//
// WHERE THE KEY LIVES. A browser backend holds its credential in the browser,
// so this is safe against a local `llama-server` (no key at all), a keyless
// proxy, or a user's own key in their own browser — and is NOT a way to ship a
// shared key to strangers. `endpoint` is required for exactly that reason:
// nothing here guesses a host to send a key to.

/** How many provider passes one `send` may take before the loop gives up. */
const DEFAULT_MAX_PASSES = 12;

export interface AIRecord {
  endpoint?: string;
  model?: string;
  api_key?: string;
  max_passes?: number;
  system_prompt?: string;
  /** llama.cpp gates its thought channel behind the chat template's
   *  `enable_thinking`. Undefined means DO NOT SEND it — a model without a
   *  thinking gate rejects an unknown kwarg. */
  think?: boolean;
}

/** One turn as the wire wants it. */
interface Message {
  role: string;
  content: string;
}

/**
 * The system block that TEACHES the one tool.
 *
 * This is the whole of tool-calling: no schema is sent to the provider, so what
 * the model knows about `send` is what this text says. It names the envelope
 * exactly, because a model that invents its own bracket writes something
 * `tool_parse` will report as malformed.
 */
export function systemPrompt(selfId: string, extra?: string): string {
  return [
    `You are \`${selfId}\`, an agent inside a Fantastic kernel running in a browser.`,
    ``,
    `Everything here is an agent, and you reach one by SENDING it a payload.`,
    `You have exactly ONE tool, \`send\`. Write it as text, like this:`,
    ``,
    `<tool_call>{"name":"send","arguments":{"target_id":"<agent id>","payload":{"type":"<verb>"}}}</tool_call>`,
    ``,
    `The reply comes back to you as a <tool_response> block. You may emit several`,
    `calls in one turn. To learn what an agent does, send it {"type":"reflect"} —`,
    `that is how you discover the tree rather than guessing at it.`,
    ``,
    `A call may carry an "id" naming the execution. Naming one makes the call SAFE`,
    `TO REPEAT: the first send runs, and a repeat with the same id returns that`,
    `first run's recorded outcome instead of running the side effect again. Use it`,
    `whenever running twice would be wrong.`,
    ``,
    `A call may also carry "retries": a re-dispatch budget for a call that fails`,
    `terminally. The kernel re-runs it for you; you do not re-send it yourself.`,
    `Send {"type":"ledger"} to \`kernel\` to see every execution you originated and`,
    `what became of it — terminal non-completed rows are the dead letters.`,
    ``,
    `Write the JSON on ONE line. If a value needs a newline or a quote, escape it;`,
    `an envelope whose string is left open swallows every brace after it and the`,
    `whole call is lost.`,
    extra ? `\n${extra}` : ``,
  ].join("\n");
}

/** What a provider must do: stream a completion as plain text pieces. */
export interface Provider {
  chat(messages: Message[]): AsyncIterable<string>;
}

/**
 * An OpenAI-compatible streaming provider — `llama-server`, and anything else
 * that speaks `/chat/completions`.
 *
 * READS BOTH CHANNELS. A reasoning model streams its thought as
 * `reasoning_content` and only its conclusion as `content`; reading `content`
 * alone silently drops whole turns. The thought is wrapped once in
 * `<think>…</think>` so it can be parsed for calls and stripped before display.
 */
export class OpenAIProvider implements Provider {
  // Written out rather than as a constructor parameter property: node's
  // strip-only TypeScript rejects those, and these tests run under `node --test`
  // with no build step.
  readonly rec: AIRecord;

  constructor(rec: AIRecord) {
    this.rec = rec;
  }

  async *chat(messages: Message[]): AsyncIterable<string> {
    const rec = this.rec;
    if (!rec.endpoint) throw new Error("ai_backend: `endpoint` is required");
    const body: Record<string, unknown> = { messages, stream: true };
    if (rec.model) body.model = rec.model;
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
    while (true) {
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
        if (data === "[DONE]") return;
        let obj: any;
        try {
          obj = JSON.parse(data);
        } catch {
          continue;
        }
        const delta = obj?.choices?.[0]?.delta;
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

/** Strip the thought span before a turn is shown or persisted. */
export function withoutThinking(text: string): string {
  return text.replace(/<think>[\s\S]*?<\/think>/g, "").trim();
}

/**
 * One agentic pass: send the conversation, read the answer, dispatch any calls,
 * and feed their replies back. Repeats until the model emits no new call.
 *
 * THE LOOP IS THE POINT. A provider that streams text is not an agent; what
 * makes it one is that its calls actually reach other agents and the results
 * come back into its context. That dispatch goes through `kernel.send`, so it
 * is subject to the same two-sided gate as any other caller — an agent that was
 * not granted a peer simply cannot reach it, and the refusal lands in the
 * transcript where the model can read it.
 */
export async function runLoop(
  selfId: string,
  kernel: Kernel,
  provider: Provider,
  history: Message[],
  opts: { maxPasses?: number; onToken?: (t: string) => void } = {},
): Promise<{ text: string; passes: number; calls: number }> {
  const maxPasses = opts.maxPasses ?? DEFAULT_MAX_PASSES;
  let passes = 0;
  let calls = 0;
  let last = "";

  while (passes < maxPasses) {
    passes++;
    let text = "";
    for await (const piece of provider.chat(history)) {
      text += piece;
      opts.onToken?.(piece);
    }
    last = text;
    history.push({ role: "assistant", content: text });

    const found = extractToolCalls(text);
    if (found.length === 0) {
      // A model that TRIED and got the envelope wrong must not look like one
      // that chose to stop — those are different facts and only one is a bug.
      const bad = malformedCalls(text);
      if (bad.length > 0) {
        history.push({
          role: "user",
          content:
            `[${bad.length} tool_call envelope(s) could not be parsed: ` +
            bad.map((b) => b.reason).join("; ") +
            `. Rewrite the call on one line with every string properly closed.]`,
        });
        continue;
      }
      break;
    }

    for (const call of found) {
      calls++;
      const args = call.arguments as any;
      const target = String(args.target_id ?? "");
      const payload = (args.payload ?? {}) as Payload;
      let reply: Json;
      try {
        // FORWARD THE MODEL'S `id`. This is ensure-semantics: an unknown id
        // dispatches, an in-flight one JOINs, and a settled one returns the
        // RECORDED outcome without running the handler again. Dropping it —
        // which this loop did until a double-fire probe caught it — makes a
        // "safe to repeat" call impossible to express: the model names the
        // execution, the kernel never hears the name, and the second call runs
        // the side effect a second time.
        //
        // The colon namespace is kernel-minted, so a caller id carrying one is
        // not forwarded rather than being refused mid-turn.
        const named =
          typeof args.id === "string" && args.id && !args.id.includes(":")
            ? { id: args.id }
            : {};
        // `retries` is the RE-DISPATCH BUDGET for a terminal failure under the
        // same id, and `timeout` bounds the wait. Both are the model's to set —
        // it is the one that knows whether a call is worth retrying — so a loop
        // that swallows them decides on the model's behalf and silently removes
        // the kernel's dead-letter behaviour from reach.
        const budget: Record<string, unknown> = {};
        if (typeof args.retries === "number") budget.retries = args.retries;
        if (typeof args.timeout === "number") budget.timeout = args.timeout;
        const outcomes = await kernel.send([
          { target, payload, ...named, ...budget },
        ]);
        reply = (outcomes[0] as any)?.result ?? null;
      } catch (e) {
        // A refusal is INFORMATION for the model, not a crash for the loop.
        reply = { error: String(e) } as Json;
      }
      history.push({
        role: "user",
        content: `<tool_response>${JSON.stringify(reply)}</tool_response>`,
      });
    }
  }
  return { text: withoutThinking(last), passes, calls };
}

/**
 * The bundle handler. Verbs mirror the host backends so `ai_view` can front
 * this one unchanged — it already speaks send/history/status.
 */
export function makeAIBackend(getRecord: (id: string) => AIRecord) {
  const histories = new Map<string, Message[]>();

  return async function handler(
    id: string,
    payload: Payload,
    kernel: Kernel,
  ): Promise<Json> {
    const verb = String((payload as any).type ?? "");
    const rec = getRecord(id);
    switch (verb) {
      case "boot":
      case "shutdown":
        return null;
      case "reflect":
        return {
          id,
          sentence:
            "Browser-resident LLM agent (OpenAI-compatible; raw prompt-and-parse tool-calling).",
          endpoint: rec.endpoint ?? null,
          model: rec.model ?? null,
          has_api_key: Boolean(rec.api_key),
          verbs: {
            reflect: "Identity + endpoint + model + has_api_key. No args.",
            send: "args: text:str, client_id?:str. Runs the agentic loop and returns the final text.",
            history: "args: client_id?:str. Returns prior turns.",
            status: "args: client_id?:str. Whether a turn is in flight.",
          },
          note:
            "The key lives in the browser: safe against a local llama-server, a keyless proxy, or your own key in your own browser — never a shared one.",
        } as Json;
      case "history": {
        const cid = String((payload as any).client_id ?? "cli");
        return { messages: histories.get(cid) ?? [], client_id: cid } as Json;
      }
      case "send": {
        const cid = String((payload as any).client_id ?? "cli");
        const text = String((payload as any).text ?? "");
        let hist = histories.get(cid);
        if (!hist) {
          hist = [{ role: "system", content: systemPrompt(id, rec.system_prompt) }];
          histories.set(cid, hist);
        }
        hist.push({ role: "user", content: text });
        const out = await runLoop(id, kernel, new OpenAIProvider(rec), hist, {
          maxPasses: rec.max_passes,
        });
        return { response: out.text, passes: out.passes, calls: out.calls, client_id: cid } as Json;
      }
      default:
        return { error: `ai_backend: unknown type ${JSON.stringify(verb)}` } as Json;
    }
  };
}
