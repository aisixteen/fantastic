// Tool-call parsing — the ONE text shape a model uses to name a call. Port of
// python `ai_core/tool_parse.py` (canonical; rust and swift match it).
//
// There is NO agentic loop and NO native provider tool API: the provider streams
// plain text, this module reads it. There is also NO JSON anywhere on this path.
// The grammar is tag-delimited, args INLINE:
//
//     <call name="send">
//     <target_id>notes</target_id>
//     <reply_to>self</reply_to>
//     <type>patch</type>
//     <edits><item><path>hello.py</path><hunks>2</hunks></item></edits>
//     <bytes>…verbatim: real newlines, real quotes, ends at the first </bytes>…
//     </bytes>
//     </call>
//
// Why tags and not JSON (measured 2026-09-09, 4 trials × 3 shapes that had failed
// in e2e): JSON with a verbatim body cut 3/12 correct — escaped quotes, literal
// `\n`, a `\"}` inside content ending the value early; bare child tags 12/12. A
// `"` boundary inside JSON fires the model's escaping reflex; a tag carries none.
//
// Rules (all of them):
// - Envelope `<call name="<tool>">…</call>`. Tools: `send`, `done`, `await`,
//   `inquiry_answer`.
// - `send`: `target_id` (or `to`, a list of recipients — one copy each), `re`,
//   `reply_to`, `title`, `timeout`, `retries`, `exec_id` are CALL-level; every
//   other child IS the payload (`type` names the verb). Other tools: the
//   children are the arguments.
// - An element with child elements → dict; children all named `<item>` → list;
//   text → scalar typed by the ONE rule (true/false, int, float, else str); an
//   empty element → "".
// - `bytes` is VERBATIM: never trimmed, never inspected, ends at the FIRST
//   `</bytes>`, delivered as utf-8 bytes. Inside `<bytes>` the scanner is in
//   verbatim mode, so a `</call>` in a document does not end the call.
// - Nothing is repaired. A call that does not parse is reported with its fault
//   named and the minimal shape to send; the model fixes it.

import type { Json } from "../../kernel/json.ts";

/** A parsed value: JSON, or the verbatim bytes of a `<bytes>` element. */
export type Value = Json | Uint8Array | Value[] | { [k: string]: Value };

export interface ToolCall {
  id: string;
  name: string;
  arguments: { [k: string]: Value };
}

export interface MalformedCall {
  error: string;
  envelope: string;
  your_block: string;
}

export const OPEN = "<call";
export const CLOSE = "</call>";
const BYTES_OPEN = "<bytes>";
const BYTES_CLOSE = "</bytes>";

const HEAD = /^\s*name\s*=\s*"([A-Za-z_]\w*)"\s*>/;
// A tag name is a KEY, not an XML name: grant maps are keyed by agent ids and the
// relative tokens (`$self`, `$parent`), and the model writes those as tags — so
// `$`, `-` and `.` are letters here.
const TAG = /<([A-Za-z_$][\w$.\-]*)>/g;
/** Call-level fields of a `send`. Everything else in the call is the payload.
 *  The execution NAME is `exec_id` (a bare `<id>` would collide with payload ids). */
export const ROUTING = ["target_id", "to", "re", "reply_to", "title", "timeout", "retries", "exec_id"] as const;
/** The tools. Any other `name` is refused with the shape. */
export const TOOLS = ["send", "done", "await", "inquiry_answer"] as const;

/** A call the model MEANT that does not parse; the message is the named fault. */
class Malformed extends Error {}

/** ONE scalar rule for text a caller typed (python `coerce_scalar`): true/false
 *  → bool, an integer → int, a decimal → float, anything else → the string. */
export function coerceScalar(v: string): Json {
  const low = v.toLowerCase();
  if (low === "true" || low === "false") return low === "true";
  if (/^[+-]?\d+$/.test(v.trim()) && v.trim() !== "") return Number.parseInt(v.trim(), 10);
  const t = v.trim();
  if (/^[+-]?(\d+\.\d*|\.\d+|\d+)([eE][+-]?\d+)?$/.test(t) || /^[+-]?(inf|infinity|nan)$/i.test(t)) {
    const f = Number(t.replace(/^([+-]?)inf(inity)?$/i, "$1Infinity"));
    if (!Number.isNaN(f) || /nan/i.test(t)) return f;
  }
  return v;
}

function tagAt(text: string, from: number): RegExpExecArray | null {
  TAG.lastIndex = from;
  return TAG.exec(text);
}

/** Index of the `</call>` that closes a call whose body starts at `start`,
 *  skipping any `</call>` inside a `<bytes>` value; -1 if unterminated. */
function findClose(text: string, start: number): number {
  let i = start;
  for (;;) {
    const c = text.indexOf(CLOSE, i);
    const b = text.indexOf(BYTES_OPEN, i);
    if (b === -1 || (c !== -1 && c < b)) return c;
    const e = text.indexOf(BYTES_CLOSE, b + BYTES_OPEN.length);
    if (e === -1) return -1; // an open document runs to the end: unterminated
    i = e + BYTES_CLOSE.length;
  }
}

/** The closer of `<name>` opened just before `start`, honouring nesting of the
 *  SAME name and skipping `<bytes>` values inside. */
function matchingClose(body: string, name: string, start: number): number {
  const opener = `<${name}>`;
  const closer = `</${name}>`;
  let depth = 1;
  let i = start;
  while (depth > 0) {
    const c = body.indexOf(closer, i);
    if (c === -1) return -1;
    const o = body.indexOf(opener, i);
    const b = body.indexOf(BYTES_OPEN, i);
    const nxt = Math.min(...[c, o, b].filter((x) => x !== -1));
    if (nxt === b) {
      const e = body.indexOf(BYTES_CLOSE, b + BYTES_OPEN.length);
      if (e === -1) return -1;
      i = e + BYTES_CLOSE.length;
    } else if (nxt === o) {
      depth += 1;
      i = o + opener.length;
    } else {
      depth -= 1;
      if (depth === 0) return c;
      i = c + closer.length;
    }
  }
  return -1;
}

/** `[name, raw]` for every child element of `body`, in order. Text between
 *  elements must be whitespace. `bytes` runs verbatim to its first closer. */
function elements(body: string): [string, string][] {
  const out: [string, string][] = [];
  let pos = 0;
  let tail: string;
  for (;;) {
    const m = tagAt(body, pos);
    if (m === null) {
      tail = body.slice(pos);
      break;
    }
    const between = body.slice(pos, m.index);
    if (between.trim() !== "") {
      throw new Malformed(
        `text outside a tag (\`${between.trim().slice(0, 40)}\`) — every argument is its own \`<name>…</name>\` element`,
      );
    }
    const name = m[1] as string;
    const closer = `</${name}>`;
    const after = m.index + m[0].length;
    if (name === "bytes") {
      const end = body.indexOf(closer, after);
      if (end === -1) {
        throw new Malformed("`<bytes>` opened and never closed — close the document with `</bytes>`, then the call with `</call>`");
      }
      out.push([name, body.slice(after, end)]);
      pos = end + closer.length;
      continue;
    }
    const end = matchingClose(body, name, after);
    if (end === -1) throw new Malformed(`\`<${name}>\` opened and never closed with \`${closer}\``);
    out.push([name, body.slice(after, end)]);
    pos = end + closer.length;
  }
  if (tail.trim() !== "") {
    throw new Malformed(
      `text after the last tag (\`${tail.trim().slice(0, 40)}\`) — every argument is its own \`<name>…</name>\` element`,
    );
  }
  return out;
}

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: false });

/** One element's value: verbatim bytes, a nested dict/list, or a typed scalar. */
function value(name: string, raw: string): Value {
  if (name === "bytes") return encoder.encode(raw);
  if (tagAt(raw, 0) === null || !raw.trim().startsWith("<")) return coerceScalar(raw.trim());
  const kids = elements(raw);
  if (kids.length === 0) return coerceScalar(raw.trim());
  // A LIST is a tag whose children are all `<item>` — any count, one included.
  if (kids.every(([k]) => k === "item")) return kids.map(([k, v]) => value(k, v));
  // `<item>` beside OTHER tags is an ordinary field named item.
  const out: { [k: string]: Value } = {};
  for (const [k, v] of kids) {
    if (k in out) {
      throw new Malformed(`duplicate \`<${k}>\` — a list is one tag whose children are all \`<item>\`: \`<${name}><item>…</item><item>…</item></${name}>\``);
    }
    out[k] = value(k, v);
  }
  return out;
}

function mintId(): string {
  const c = (globalThis as { crypto?: { getRandomValues?: (a: Uint8Array) => Uint8Array } }).crypto;
  if (c === undefined || typeof c.getRandomValues !== "function") {
    throw new Error("tool_parse: this runtime has no crypto.getRandomValues — cannot mint a call id");
  }
  const bytes = new Uint8Array(4);
  c.getRandomValues(bytes);
  return "call_" + Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/** Parse the text between `<call` and `</call>` into {id, name, arguments}.
 *  Throws Malformed (fault named) for a call-shaped block that does not parse;
 *  null only for a block that is not a call at all (no `name="…">` head). */
function parseCall(block: string): ToolCall | null {
  const m = HEAD.exec(block);
  if (m === null) {
    const head = (block.split(">", 1)[0] as string).trim();
    if (!head.startsWith("name=")) return null;
    throw new Malformed('the call head must be exactly `name="<tool>"` — nothing else before `>`');
  }
  const name = m[1] as string;
  if (!(TOOLS as readonly string[]).includes(name)) {
    throw new Malformed(
      `\`${name}\` is not a tool — the tools are ${TOOLS.join(", ")}. A VERB goes in ` +
        `\`<type>\`: \`<call name="send"><target_id>…</target_id><type>${name}</type></call>\``,
    );
  }
  const fields: { [k: string]: Value } = {};
  for (const [k, v] of elements(block.slice(m[0].length))) {
    if (k in fields) {
      throw new Malformed(`duplicate \`<${k}>\` — a list is one tag whose children are all \`<item>\`: \`<${k}><item>…</item><item>…</item></${k}>\``);
    }
    fields[k] = value(k, v);
  }
  if (name !== "send") return { id: mintId(), name, arguments: fields };
  const target = fields["target_id"];
  delete fields["target_id"];
  let to = fields["to"];
  delete fields["to"];
  if (typeof to === "string" && to.trim() !== "") to = [to.trim()];
  if (Array.isArray(to)) to = to.map((t) => String(t).trim()).filter((t) => t !== "");
  const hasTarget = typeof target === "string" && target.trim() !== "";
  if (!hasTarget && !(Array.isArray(to) && to.length > 0)) {
    throw new Malformed(
      "a send names its target: `<target_id>agent id</target_id>`, or several recipients as `<to><item>id</item><item>id</item></to>`",
    );
  }
  const args: { [k: string]: Value } = {};
  if (hasTarget) args["target_id"] = (target as string).trim();
  if (Array.isArray(to) && to.length > 0) args["to"] = to;
  for (const key of ROUTING.slice(2)) {
    if (key in fields) {
      args[key] = fields[key] as Value;
      delete fields[key];
    }
  }
  args["payload"] = fields;
  return { id: mintId(), name: "send", arguments: args };
}

function isPlain(v: unknown): v is { [k: string]: Value } {
  return v !== null && typeof v === "object" && !Array.isArray(v) && !(v instanceof Uint8Array);
}

/** The inverse of the parser: {name, arguments} as the ONE grammar's text.
 *  `send` arguments are the parser's own shape and come out inline. */
export function renderCall(name: string, argumentsIn: { [k: string]: Value } = {}): string {
  let args: { [k: string]: Value } = { ...argumentsIn };
  if (name === "send") {
    const payload = isPlain(args["payload"]) ? args["payload"] : {};
    const flat: { [k: string]: Value } = {};
    for (const k of ROUTING) if (k in args) flat[k] = args[k] as Value;
    args = { ...flat, ...payload };
  }
  const body = Object.entries(args).map(([k, v]) => renderField(k, v)).join("");
  return `<call name="${name}">\n${body}</call>`;
}

function renderField(name: string, v: Value): string {
  if (name === "bytes" || v instanceof Uint8Array) {
    const text = v instanceof Uint8Array ? decoder.decode(v) : String(v);
    return `<bytes>${text}</bytes>\n`;
  }
  if (Array.isArray(v)) return `<${name}>${v.map((x) => renderField("item", x)).join("")}</${name}>\n`;
  if (isPlain(v)) return `<${name}>${Object.entries(v).map(([k, x]) => renderField(k, x)).join("")}</${name}>\n`;
  if (typeof v === "boolean") return `<${name}>${v ? "true" : "false"}</${name}>\n`;
  return `<${name}>${v === null ? "None" : String(v)}</${name}>\n`;
}

/** A REPLY to the model, in the same tag notation it writes calls in — one
 *  notation on the whole model↔harness path, no JSON either way. A dict is one
 *  `<k>…</k>` per line; a list is its outer tag with one `<item>` per element;
 *  text with newlines spans lines verbatim; bytes are decoded utf-8; null is an
 *  empty tag. */
export function renderReply(v: Value | undefined): string {
  if (isPlain(v)) return Object.entries(v).map(([k, x]) => replyField(k, x)).join("").replace(/\n+$/, "");
  if (Array.isArray(v)) return replyField("items", v).replace(/\n+$/, "");
  return replyText(v);
}

function replyText(v: Value | undefined): string {
  if (v === null || v === undefined) return "";
  if (typeof v === "boolean") return v ? "true" : "false";
  if (v instanceof Uint8Array) return decoder.decode(v);
  return String(v);
}

function replyField(name: string, v: Value): string {
  if (isPlain(v)) return `<${name}>\n${Object.entries(v).map(([k, x]) => replyField(k, x)).join("")}</${name}>\n`;
  if (Array.isArray(v)) {
    if (v.every((x) => isPlain(x) || Array.isArray(x))) {
      return `<${name}>\n${v.map((x) => replyField("item", x)).join("")}</${name}>\n`;
    }
    return `<${name}>\n${v.map((x) => `<item>${replyText(x)}</item>\n`).join("")}</${name}>\n`;
  }
  const text = replyText(v);
  if (text.includes("\n")) return `<${name}>\n${text}\n</${name}>\n`;
  return `<${name}>${text}</${name}>\n`;
}

/** [start, bodyStart, end] for every `<call …>` envelope in document order — end
 *  is the index of its `</call>` or -1 when unterminated. `<call` must be
 *  followed by whitespace and `name`, so `<caller>` or "a <call soon" is prose. */
function* envelopes(text: string): Generator<[number, number, number]> {
  let i = 0;
  while (i < text.length) {
    const pos = text.indexOf(OPEN, i);
    if (pos === -1) return;
    const after = pos + OPEN.length;
    if (after >= text.length || !/\s/.test(text[after] as string) || !text.slice(after).trimStart().startsWith("name")) {
      i = after;
      continue;
    }
    const end = findClose(text, after);
    yield [pos, after, end];
    if (end === -1) return;
    i = end + CLOSE.length;
  }
}

/** Every COMPLETE call envelope in `text`, verbatim, in source order — what a
 *  history writer keeps out of a dropped thought span (an action taken is not
 *  reasoning). An unterminated opener is not kept. */
export function callSpans(text: string): string[] {
  const out: string[] = [];
  for (const [pos, after, end] of envelopes(text)) {
    if (end === -1) break;
    let call: ToolCall | null;
    try {
      call = parseCall(text.slice(after, end));
    } catch {
      call = null;
    }
    if (call !== null) out.push(text.slice(pos, end + CLOSE.length));
  }
  return out;
}

/** Every envelope found, as [block, call|null, fault|null]. A null call with no
 *  fault is a MENTION of `<call` in prose — stepped over. */
function scan(text: string): [string, ToolCall | null, string | null][] {
  const out: [string, ToolCall | null, string | null][] = [];
  for (const [, after, end] of envelopes(text)) {
    // UNTERMINATED is not a truncation guess: the envelope is the LAST thing in
    // the completion, so there is nothing after it to misread.
    const block = text.slice(after, end !== -1 ? end : text.length);
    let call: ToolCall | null = null;
    let fault: string | null = null;
    try {
      call = parseCall(block);
    } catch (e) {
      if (!(e instanceof Malformed)) throw e;
      fault = e.message;
    }
    if (call === null && fault === null) continue;
    out.push([block, call, fault]);
  }
  return out;
}

/** Every finalized `<call>` in a complete string. */
export function extractToolCalls(text: string): ToolCall[] {
  return scan(text).map(([, c]) => c).filter((c): c is ToolCall => c !== null);
}

/** The shape appended to every refusal: literal, minimal, copyable. */
const HINT =
  'The shape: <call name="send">\n<target_id>agent id</target_id>\n' +
  "<type>verb</type>\n<path>a.md</path>\n<bytes>document, verbatim, to the first " +
  "</bytes>\n</bytes>\n</call> — one child tag per argument, a list is one tag " +
  "whose children are all <item>, nothing is escaped.";

/** Neutralize envelope delimiters so quoted text cannot re-arm the scanner. */
export function defang(text: string): string {
  let out = text;
  for (const tag of [OPEN, CLOSE, BYTES_OPEN, BYTES_CLOSE]) {
    out = out.split(tag).join(tag.replace(/</g, "‹").replace(/>/g, "›"));
  }
  return out;
}

/** Envelopes that opened as a call and did not parse, as {error, envelope,
 *  your_block}. A call that cannot be read is the one failure a model cannot
 *  debug — `extractToolCalls` returns [] for it, identical to a turn that
 *  emitted nothing — so it gets the same voice every other failure has. */
export function malformedCalls(text: string): MalformedCall[] {
  const out: MalformedCall[] = [];
  for (const [block, call, fault] of scan(text)) {
    if (call !== null) continue;
    out.push({
      error: `call NOT dispatched: ${fault}. Nothing ran. ` + HINT,
      envelope: OPEN,
      // BOTH ENDS, shown WHOLE and DEFANGED so quoting it cannot arm the scanner.
      your_block: defang(block.trim()),
    });
  }
  return out;
}
