// SECRET VALUES on a call — the caller marks them, the kernel never records them.
// Port of python `kernel/_secret.py` (owner, 2026-10-01); texts byte-identical.
//
// A batch item may carry `secret: [mark, …]` beside `target`/`payload`/`timeout`.
// A mark is rooted at the item: `payload` + a field of the payload, `result` (the
// whole reply), or `result` + a field of it. The TARGET is handed the real payload
// and the CALLER the real reply; every place the kernel RECORDS or SHOWS the call
// carries `<secret>` instead — the call and settle rows (and so the memory store),
// the `ledger` read-back and dedup, the inbox and watcher mirrors, and any error
// text the call's outcome carries.
//
// A mark is spelled one of two ways (one list may mix them):
//   * a DOTTED STRING — `payload.key`, `result`, `result.fields.api_key` — split on
//     its dots; at a list, a step of ASCII digits is an index;
//   * an ARRAY OF STEPS (owner, 2026-10-03) — `["payload", "a.b"]`,
//     `["result", "items", 0]` — exact, with the step rules of the binary frame's
//     `_binary_path`: a STRING step is an object key taken literally (it may hold a
//     `.`), an INTEGER step (non-negative) a list index. It is the only way to mark
//     a key that holds a dot.
//
// Two limits, by design:
//   * masking is not isolation — the receiver holds the value; send secrets to a
//     bundle that keeps them, never to an AI agent (they would enter its prompt
//     and its memory);
//   * a call whose record holds placeholders cannot be answered FROM that record:
//     re-sending a named execution with secrets after it left RAM is refused, loud.
//
// Not secret-able: `payload.type` — the verb is how every row is read; and any
// payload field of `create_agent`/`update_agent` — the kernel writes those into the
// agent's RECORD (its `agent_created`/`agent_updated` events, `agent.yaml`), where
// no mark can hold. A value an agent must keep belongs in a `credential_store`, its
// record naming the ref (owner-approved 2026-10-03, audit S2). This kernel answers
// neither verb itself; its `check` refuses the mark before the call leaves for the
// host, in the host's words.

import type { Json, Payload } from "./json.ts";
import type { Outcome } from "./outcome.ts";
import { BAD_ARG, refusal } from "./refusal.ts";
import { pyRepr, pyReprAt, wireFloat } from "./pyjson.ts";

export const PLACEHOLDER = "<secret>";

/** The verbs whose every payload field (`type` aside) lands in an agent's record —
 *  the id, the handler, the placement, the meta. A mark there is refused
 *  (`check`). Python `_RECORD_VERBS`. */
const RECORD_VERBS: ReadonlySet<string> = new Set(["create_agent", "update_agent"]);

/** One step of an array mark: an object key (taken literally) or a list index. */
export type Step = string | number;

/** One secret mark as given: a dotted string, or an array of steps. */
export type Mark = string | readonly Step[];

/** One mark below its root (`payload` / `result`): the steps, and whether they
 *  are EXACT (an array mark: a string step is only an object key, a number step
 *  only a list index) or DOTTED (a string step is a key, or at a list an index
 *  when it is ASCII digits). */
interface Path {
  keys: readonly Step[];
  exact: boolean;
}

/** What one item asked to be kept: the payload/result paths, the marks as given
 *  (recorded on the call row — marks are not secret), and the payload's secret
 *  VALUES (RAM only — scrubbed out of every recorded string). */
export interface Secret {
  paths: (string | Step[])[];
  payloadPaths: Path[];
  resultPaths: Path[];
  values: string[];
}

type Dict = { [k: string]: unknown };

/** Raw bytes — python `bytes`: not a dict, not a list, immutable. */
function isBytes(x: unknown): x is ArrayBuffer | ArrayBufferView {
  return x instanceof ArrayBuffer || ArrayBuffer.isView(x);
}

function isDict(x: unknown): x is Dict {
  return x !== null && typeof x === "object" && !Array.isArray(x) && !isBytes(x);
}

/** python `type(s) is int and s >= 0` — what an array step names a list index by
 *  (the binary frame's `_binary_path` step rule). */
function isIndex(s: unknown): s is number {
  return typeof s === "number" && Number.isInteger(s) && s >= 0;
}

function step(cur: unknown, key: Step, exact: boolean): [boolean, unknown] {
  if (isDict(cur)) {
    if (exact && typeof key !== "string") return [false, null];
    const k = key as string;
    return Object.hasOwn(cur, k) ? [true, cur[k]] : [false, null];
  }
  if (Array.isArray(cur)) {
    let i: number;
    if (exact) {
      if (!isIndex(key)) return [false, null];
      i = key;
    } else if (/^[0-9]+$/.test(key as string)) {
      // python `key.isascii() and key.isdigit()` — ASCII digits only
      i = Number(key);
    } else {
      return [false, null];
    }
    return i < cur.length ? [true, cur[i]] : [false, null];
  }
  return [false, null];
}

function get(obj: unknown, path: Path): [boolean, unknown] {
  let cur = obj;
  for (const k of path.keys) {
    const [ok, next] = step(cur, k, path.exact);
    if (!ok) return [false, null];
    cur = next;
  }
  return [true, cur];
}

/** python `str(bytes)` — its repr: `b'…'`, escapes as python writes them. */
function pyBytesStr(x: ArrayBuffer | ArrayBufferView): string {
  const b = x instanceof ArrayBuffer ? new Uint8Array(x) : new Uint8Array(x.buffer, x.byteOffset, x.byteLength);
  const quote = b.includes(0x27) && !b.includes(0x22) ? 0x22 : 0x27;
  let out = "b" + String.fromCharCode(quote);
  for (const c of b) {
    if (c === quote || c === 0x5c) out += "\\" + String.fromCharCode(c);
    else if (c === 0x09) out += "\\t";
    else if (c === 0x0a) out += "\\n";
    else if (c === 0x0d) out += "\\r";
    else if (c < 0x20 || c >= 0x7f) out += "\\x" + c.toString(16).padStart(2, "0");
    else out += String.fromCharCode(c);
  }
  return out + String.fromCharCode(quote);
}

/** Every string a secret value holds — the value itself, or its leaves. */
function strings(x: unknown): string[] {
  if (typeof x === "string") return x !== "" ? [x] : [];
  if (isDict(x)) return Object.values(x).flatMap(strings);
  if (Array.isArray(x)) return x.flatMap(strings);
  if (x === null || x === undefined || typeof x === "boolean") return [];
  if (typeof x === "number") return [pyRepr(x)]; // python `str(int|float)`
  if (isBytes(x)) return [pyBytesStr(x)];
  return [String(x)];
}

/** A mark split at its root: [root, path below it, why-not]. */
type Parsed = ["payload" | "result", Path, null] | [null, null, string];

/** A dotted-string mark → [root, path below it, why-not]. */
function dotted(p: string): Parsed {
  if (p === "") return [null, null, `path must be a non-empty string, got ${pyRepr(p)}`];
  const keys = p.split(".");
  if (keys.some((k) => k === "")) return [null, null, `path ${pyRepr(p)} has an empty step`];
  if (keys[0] === "result") return ["result", { keys: keys.slice(1), exact: false }, null];
  if (keys[0] === "payload" && keys.length > 1) return ["payload", { keys: keys.slice(1), exact: false }, null];
  return [null, null, `path ${pyRepr(p)} must start with 'payload.' or be 'result[.…]'`];
}

/** An array-of-steps mark → [root, path below it, why-not]. A step off the wire
 *  spelled as a python float (`1.0`, `1e0`) is no index, though JS holds it as
 *  the integer 1 (`wireFloat`). */
function array(p: readonly unknown[]): Parsed {
  if (p.length === 0) return [null, null, "path [] has no steps"];
  for (let i = 0; i < p.length; i++) {
    const s = p[i];
    if (typeof s !== "string" && !(isIndex(s) && !wireFloat(p, i))) {
      return [
        null,
        null,
        `path ${pyRepr(p)} step ${pyReprAt(p, i)} is neither a string (an object key) nor a ` +
          "non-negative integer (a list index)",
      ];
    }
  }
  const steps = p as readonly Step[];
  if (steps[0] === "result") return ["result", { keys: steps.slice(1), exact: true }, null];
  if (steps[0] === "payload" && steps.length > 1) return ["payload", { keys: steps.slice(1), exact: true }, null];
  return [null, null, `path ${pyRepr(p)} must start with 'payload' and a step under it, or with 'result'`];
}

/**
 * Validate an item's `secret`: [Secret | null, refusal | null]. Absent (or null,
 * as python's `item.get` reads it) ⇒ nothing secret. A payload mark the payload
 * does not have is refused — a typo would otherwise record the value it meant to
 * hide.
 */
export function check(item: { secret?: unknown }, payload: Payload): [Secret | null, Payload | null] {
  const raw = item.secret;
  if (raw === undefined || raw === null) return [null, null];
  const bad = (why: string): [null, Payload] => [
    null,
    refusal(BAD_ARG, `send: secret ${why}`, { extra: { arg: "secret" } }),
  ];
  if (!Array.isArray(raw) || raw.length === 0) {
    return bad(`must be a non-empty list of paths, got ${pyRepr(raw)}`);
  }
  const payloadPaths: Path[] = [];
  const resultPaths: Path[] = [];
  const verb = payload["type"];
  const recordVerb = typeof verb === "string" && RECORD_VERBS.has(verb) ? verb : null;
  for (const p of raw as unknown[]) {
    let parsed: Parsed;
    if (typeof p === "string") parsed = dotted(p);
    else if (Array.isArray(p)) parsed = array(p);
    else return bad(`path must be a dotted string or an array of steps, got ${pyRepr(p)}`);
    const [root, path, why] = parsed;
    if (why !== null) return bad(why);
    if (root === "result") {
      resultPaths.push(path);
      continue;
    }
    if (path.keys.length === 1 && path.keys[0] === "type") {
      return bad("cannot hide payload.type — the verb is how every row is read");
    }
    if (recordVerb !== null) {
      return bad(
        `path ${pyRepr(p)} names a field ${recordVerb} writes into the agent record: a record ` +
          "field cannot be secret — use a credential_store ref",
      );
    }
    if (!get(payload, path)[0]) return bad(`path ${pyRepr(p)} names nothing in the payload`);
    payloadPaths.push(path);
  }
  const values = payloadPaths.flatMap((kp) => strings(get(payload, kp)[1]));
  // the marks AS GIVEN, an array copied — what the call row records and what the
  // handler reads back as `currentSecret`
  const marks = (raw as Mark[]).map((p) => (typeof p === "string" ? p : [...p]));
  return [{ paths: marks, payloadPaths, resultPaths, values }, null];
}

/** python `copy.deepcopy` of a JSON value: containers copied, bytes shared (they
 *  are immutable in python, and a payload's bytes can be large). */
function deepCopy(x: unknown): unknown {
  if (Array.isArray(x)) return x.map(deepCopy);
  if (isDict(x)) {
    const out: Dict = {};
    for (const [k, v] of Object.entries(x)) out[k] = deepCopy(v);
    return out;
  }
  return x;
}

/** `obj` with the value at `path` replaced by the placeholder (a copy; a missing
 *  path leaves it as is). No steps is the whole value. */
function put(obj: unknown, path: Path): unknown {
  const keys = path.keys;
  if (keys.length === 0) return PLACEHOLDER;
  if (!get(obj, path)[0]) return obj;
  const out = deepCopy(obj);
  let cur = out as Dict | unknown[];
  for (const k of keys.slice(0, -1)) {
    cur = (Array.isArray(cur) ? cur[Number(k)] : cur[k as string]) as Dict | unknown[];
  }
  const last = keys[keys.length - 1] as Step;
  if (Array.isArray(cur)) cur[Number(last)] = PLACEHOLDER;
  else cur[last as string] = PLACEHOLDER;
  return out;
}

/** python `len(str)` — code points, not UTF-16 units. */
function cpLen(s: string): number {
  let n = 0;
  for (const _ of s) n++;
  return n;
}

function scrubSorted(x: unknown, values: readonly string[]): unknown {
  if (typeof x === "string") {
    let s = x;
    for (const v of values) if (s.includes(v)) s = s.split(v).join(PLACEHOLDER);
    return s;
  }
  if (isDict(x)) {
    const out: Dict = {};
    for (const [k, v] of Object.entries(x)) out[values.includes(k) ? PLACEHOLDER : k] = scrubSorted(v, values);
    return out;
  }
  if (Array.isArray(x)) return x.map((v) => scrubSorted(v, values));
  // python `str(int|float|bool)` — a number's text, a bool's `True`/`False`
  if (typeof x === "number" || typeof x === "boolean") {
    return values.includes(pyRepr(x)) ? PLACEHOLDER : x;
  }
  return x;
}

/** Every occurrence of a secret value inside any string of `x` → placeholder —
 *  an error text that quotes the key must not carry it into a row or a log.
 *  Longest value first (python sorts by `len`, stable). A dict KEY, a NUMBER and
 *  a BOOL whose text EQUALS a secret value are the placeholder too: a reply keyed by the
 *  token it was given, a PIN echoed as an int (owner-approved 2026-10-03, audit
 *  S7). Equal, not contained: a key is structure, and `type` must survive a
 *  one-letter secret. Two keys that scrub alike keep the later value. */
export function scrub(x: unknown, values: readonly string[]): unknown {
  if (values.length === 0) return x;
  return scrubSorted(x, [...values].sort((a, b) => cpLen(b) - cpLen(a)));
}

/** The payload as recorded and shown: secret fields → placeholder. */
export function shownPayload(payload: Payload, s: Secret): Payload {
  let out: unknown = payload;
  for (const kp of s.payloadPaths) out = put(out, kp);
  return scrub(out, s.values) as Payload;
}

/** An outcome as recorded and shown: secret result fields → placeholder, and
 *  every secret value (the payload's and the result's own) scrubbed from its text. */
export function shownOutcome(out: Outcome, s: Secret): Outcome {
  let res: unknown = out.result;
  const values = [...s.values];
  for (const kp of s.resultPaths) {
    const [ok, v] = get(res, kp);
    if (ok) values.push(...strings(v));
    res = put(res, kp);
  }
  // the RESULT only: `status`, `pid` and the rest are the kernel's own words — a
  // short secret must not mangle "completed" into "compl<secret>t<secret>d"
  return { ...out, result: scrub(res, values) as Json };
}

/** The answer to re-sending an execution whose record holds placeholders. */
export function replayRefused(nameOrId: string | number): Payload {
  return refusal(
    BAD_ARG,
    `send: execution ${pyRepr(nameOrId)} carried secrets — its record holds ` +
      `${PLACEHOLDER} placeholders, not values, so it cannot be answered from ` +
      "the record; send again under a new id",
    { extra: { arg: "id" } },
  );
}
