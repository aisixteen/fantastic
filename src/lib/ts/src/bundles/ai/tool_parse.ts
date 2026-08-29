// The RAW tool-call envelope — parsing, in the browser kernel.
//
// Tool-calling here is prompt-and-parse: no provider's native tool API is ever
// used. The system prompt teaches ONE tool (`send`), the model writes it as text
// inside `<tool_call>…</tool_call>`, and this file turns that text back into a
// call. That is the same contract python, rust and swift hold, which is why a
// model can drive any of the four kernels without knowing which it is talking to.
//
// Every leniency below was earned by a measured failure, not by taste. Tiny
// models drift, and a parser that only accepts the ideal shape throws away real
// instructions:
//
//   * `tool` as an alias for `name`
//   * a FLATTENED object — no `arguments` key, the rest of the object is it
//   * double-encoded (stringified) `arguments`
//   * strays like `reply_to` written one level too high
//   * trailing junk after a complete object (gpt-oss closes `arguments` early,
//     then emits `,"reply_to":"self"}}` — one brace too many)
//   * an UNTERMINATED envelope as the last thing in the completion
//     (nemotron emits a valid call object and simply stops)
//   * the VERB COMPRESSED INTO THE TOOL NAME —
//     `{"name":"reflect","arguments":{"target_id":"clock"}}` means "reflect the
//     clock"; untranslated it dispatched `send("clock", null)` and the handler
//     raised on a null payload.

export const OPEN = "<tool_call>";
export const CLOSE = "</tool_call>";

export interface ToolCall {
  id: string;
  name: string;
  arguments: Record<string, unknown>;
}

/** A call the model TRIED to make and got wrong — reported, never silently dropped. */
export interface MalformedCall {
  block: string;
  reason: string;
}

function mintId(): string {
  // Opaque and unguessable enough to correlate one call with its reply. Not a
  // security boundary: the kernel mints the real execution ids.
  const bytes = new Uint8Array(4);
  (globalThis.crypto ?? require("node:crypto").webcrypto).getRandomValues(bytes);
  return "call_" + Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * Take the first complete JSON value from `s`, returning it and the rest.
 *
 * `JSON.parse` is all-or-nothing, so a complete object followed by junk fails
 * entirely — and that junk usually holds the model's real intent. This walks
 * the string tracking string/escape state so the object that IS there survives.
 */
function rawDecode(s: string): { value: unknown; rest: string } | null {
  const t = s.trimStart();
  if (!t.startsWith("{") && !t.startsWith("[")) {
    try {
      return { value: JSON.parse(t), rest: "" };
    } catch {
      return null;
    }
  }
  let depth = 0;
  let inString = false;
  let escaped = false;
  for (let i = 0; i < t.length; i++) {
    const c = t[i];
    if (escaped) {
      escaped = false;
      continue;
    }
    if (c === "\\") {
      if (inString) escaped = true;
      continue;
    }
    if (c === '"') {
      inString = !inString;
      continue;
    }
    if (inString) continue;
    if (c === "{" || c === "[") depth++;
    else if (c === "}" || c === "]") {
      depth--;
      if (depth === 0) {
        try {
          return { value: JSON.parse(t.slice(0, i + 1)), rest: t.slice(i + 1) };
        } catch {
          return null;
        }
      }
    }
  }
  return null;
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/**
 * A `send` whose payload was written FLAT among the args.
 *
 * `target_id` names the destination and `reply_to` says where the answer goes —
 * both are ROUTING, so they stay at the top level. Everything else is payload.
 */
function inlineSend(
  target: unknown,
  args: Record<string, unknown>,
): ToolCall | null {
  if (typeof target !== "string" || !target.trim()) return null;
  const payload: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(args)) {
    if (k !== "target_id" && k !== "reply_to") payload[k] = v;
  }
  const out: Record<string, unknown> = { target_id: target.trim(), payload };
  if ("reply_to" in args) out.reply_to = args.reply_to;
  return { id: mintId(), name: "send", arguments: out };
}

/** Parse the JSON between one tag pair into `{id, name, arguments}`. */
export function parseOne(inner: string): ToolCall | null {
  const decoded = rawDecode(inner.trim());
  if (!decoded || !isObject(decoded.value)) return null;
  const obj = decoded.value;
  const rest = decoded.rest;

  const name = String(obj.name ?? obj.tool ?? "send");
  let args: unknown = obj.arguments;

  if (args === undefined || args === null) {
    // FLATTENED: no `arguments`, so the rest of the object IS the arguments.
    const flat: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(obj)) {
      if (k !== "name" && k !== "tool" && k !== "arguments") flat[k] = v;
    }
    args = flat;
  } else if (isObject(args)) {
    // Top-level keys that belong INSIDE arguments. Models routinely put
    // `reply_to` one level too high; `arguments` wins on conflict.
    const strays: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(obj)) {
      if (k !== "name" && k !== "tool" && k !== "arguments") strays[k] = v;
    }
    if (rest.trim()) {
      // `,"reply_to":"self"}}` → recover the pairs the stray tail held.
      try {
        const tail = JSON.parse(
          "{" + rest.trim().replace(/^,/, "").replace(/}+$/, "") + "}",
        );
        if (isObject(tail)) Object.assign(strays, tail);
      } catch {
        /* the tail was not recoverable; the object that parsed still stands */
      }
    }
    args = { ...strays, ...args };
  }

  if (typeof args === "string") {
    // DOUBLE-ENCODED: `"arguments": "{\"target_id\":…}"`.
    try {
      args = JSON.parse(args);
    } catch {
      args = {};
    }
  }
  if (!isObject(args)) args = {};
  const argsObj = args as Record<string, unknown>;

  // THE VERB IN THE TOOL NAME. `send` is the only tool there is, so a model that
  // writes any other name has compressed verb and tool into one. An explicit
  // `type` in the args still wins, since the args decide.
  if (name !== "send" && !("payload" in argsObj) && "target_id" in argsObj) {
    const inline = inlineSend(argsObj.target_id, { type: name, ...argsObj });
    if (inline) return inline;
  }
  return { id: mintId(), name, arguments: argsObj };
}

/** Did the model TRY to make a call here, or merely mention the tag in prose? */
function looksLikeAnAttempt(block: string): boolean {
  // A block containing ANOTHER opener is the giveaway that this one was prose:
  // the stray opener paired with the closing tag of the GENUINE call further
  // down, so the whole real call is sitting inside this "block". Testing for
  // `"name"` anywhere would match that swallowed call and consume it.
  if (block.includes(OPEN)) return false;
  return block.trimStart().startsWith("{");
}

interface Scanned {
  block: string;
  call: ToolCall | null;
  terminated: boolean;
}

/**
 * Every envelope found, in document order.
 *
 * A `null` call means an envelope WAS opened and its body did not parse. Those
 * are kept rather than dropped so a caller can tell the two outcomes apart —
 * see `malformedCalls`. ONE implementation of the scan rules; two copies would
 * drift the moment a second envelope dialect arrives.
 */
function scan(text: string): Scanned[] {
  const out: Scanned[] = [];
  let i = 0;
  while (i < text.length) {
    const pos = text.indexOf(OPEN, i);
    if (pos === -1) break;
    const end = text.indexOf(CLOSE, pos + OPEN.length);
    // UNTERMINATED is not a truncation guess: the envelope is the LAST thing in
    // the completion, so there is nothing after it to misread.
    const block = text.slice(pos + OPEN.length, end === -1 ? text.length : end);
    const call = parseOne(block);
    if (call === null && !looksLikeAnAttempt(block)) {
      // A MENTION, not a call. Step over the OPENER ONLY — consuming through the
      // closer would swallow whatever follows, and what follows is often the
      // real call: a stray opener in prose pairs with the closing tag of the
      // genuine call further down, and the genuine call is then never seen.
      i = pos + OPEN.length;
      continue;
    }
    out.push({ block, call, terminated: end !== -1 });
    if (end === -1) break;
    i = end + CLOSE.length;
  }
  return out;
}

/** Pull every finalized call out of a complete string. */
export function extractToolCalls(text: string): ToolCall[] {
  return scan(text)
    .map((s) => s.call)
    .filter((c): c is ToolCall => c !== null);
}

/**
 * Envelopes the model OPENED but got wrong.
 *
 * Reported rather than dropped: a model that tried three times and produced
 * nothing looks identical to a model that chose to stop, and only one of those
 * is a bug you can act on.
 */
export function malformedCalls(text: string): MalformedCall[] {
  return scan(text)
    .filter((s) => s.call === null)
    .map((s) => ({
      block: s.block.slice(0, 400),
      reason: s.terminated
        ? "envelope closed but its body did not parse as JSON"
        : "envelope never closed",
    }));
}
