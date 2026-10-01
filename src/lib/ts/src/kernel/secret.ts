// SECRET VALUES on a call — the caller marks them, the kernel never records them.
// Port of python `kernel/_secret.py` (owner, 2026-10-01); texts byte-identical.
//
// A batch item may carry `secret: [path, …]` beside `target`/`payload`/`timeout`.
// A path is dotted and rooted at the item: `payload.<field…>` names a field of the
// payload, `result` the whole reply, `result.<field…>` a field of it. The TARGET
// is handed the real payload and the CALLER the real reply; every place the kernel
// RECORDS or SHOWS the call carries `<secret>` instead — the call and settle rows
// (and so the memory store), the `ledger` read-back and dedup, the inbox and
// watcher mirrors, and any error text the call's outcome carries.
//
// Two limits, by design:
//   * masking is not isolation — the receiver holds the value; send secrets to a
//     bundle that keeps them, never to an AI agent (they would enter its prompt
//     and its memory);
//   * a call whose record holds placeholders cannot be answered FROM that record:
//     re-sending a named execution with secrets after it left RAM is refused, loud.
//
// Not secret-able: `payload.type` — the verb is how every row is read.

import type { Json, Payload } from "./json.ts";
import type { Outcome } from "./outcome.ts";
import { BAD_ARG, refusal } from "./refusal.ts";
import { pyRepr } from "./pyjson.ts";

export const PLACEHOLDER = "<secret>";

/** What one item asked to be kept: the payload/result paths (as key lists), the
 *  paths as given (recorded on the call row — paths are not secret), and the
 *  payload's secret VALUES (RAM only — scrubbed out of every recorded string). */
export interface Secret {
  paths: string[];
  payloadPaths: string[][];
  resultPaths: string[][];
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

function step(cur: unknown, key: string): [boolean, unknown] {
  if (isDict(cur)) return Object.hasOwn(cur, key) ? [true, cur[key]] : [false, null];
  // python `key.isascii() and key.isdigit()` — ASCII digits only
  if (Array.isArray(cur) && /^[0-9]+$/.test(key)) {
    const i = Number(key);
    return i < cur.length ? [true, cur[i]] : [false, null];
  }
  return [false, null];
}

function get(obj: unknown, keys: readonly string[]): [boolean, unknown] {
  let cur = obj;
  for (const k of keys) {
    const [ok, next] = step(cur, k);
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

/**
 * Validate an item's `secret`: [Secret | null, refusal | null]. Absent (or null,
 * as python's `item.get` reads it) ⇒ nothing secret. A path the payload does not
 * have is refused — a typo would otherwise record the value it meant to hide.
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
  const payloadPaths: string[][] = [];
  const resultPaths: string[][] = [];
  for (const p of raw as unknown[]) {
    if (typeof p !== "string" || p === "") return bad(`path must be a non-empty string, got ${pyRepr(p)}`);
    const keys = p.split(".");
    if (keys.some((k) => k === "")) return bad(`path ${pyRepr(p)} has an empty step`);
    if (keys[0] === "result") {
      resultPaths.push(keys.slice(1));
    } else if (keys[0] === "payload" && keys.length > 1) {
      const rest = keys.slice(1);
      if (rest.length === 1 && rest[0] === "type") {
        return bad("cannot hide payload.type — the verb is how every row is read");
      }
      if (!get(payload, rest)[0]) return bad(`path ${pyRepr(p)} names nothing in the payload`);
      payloadPaths.push(rest);
    } else {
      return bad(`path ${pyRepr(p)} must start with 'payload.' or be 'result[.…]'`);
    }
  }
  const values = payloadPaths.flatMap((kp) => strings(get(payload, kp)[1]));
  return [{ paths: [...(raw as string[])], payloadPaths, resultPaths, values }, null];
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

/** `obj` with the value at `keys` replaced by the placeholder (a copy; a missing
 *  path leaves it as is). An empty `keys` is the whole value. */
function put(obj: unknown, keys: readonly string[]): unknown {
  if (keys.length === 0) return PLACEHOLDER;
  if (!get(obj, keys)[0]) return obj;
  const out = deepCopy(obj);
  let cur = out as Dict | unknown[];
  for (const k of keys.slice(0, -1)) {
    cur = (Array.isArray(cur) ? cur[Number(k)] : cur[k]) as Dict | unknown[];
  }
  const last = keys[keys.length - 1] as string;
  if (Array.isArray(cur)) cur[Number(last)] = PLACEHOLDER;
  else cur[last] = PLACEHOLDER;
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
    for (const [k, v] of Object.entries(x)) out[k] = scrubSorted(v, values);
    return out;
  }
  if (Array.isArray(x)) return x.map((v) => scrubSorted(v, values));
  return x;
}

/** Every occurrence of a secret value inside any string of `x` → placeholder —
 *  an error text that quotes the key must not carry it into a row or a log.
 *  Longest value first (python sorts by `len`, stable). */
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
    const [ok, v] = kp.length > 0 ? get(res, kp) : [true, res];
    if (ok) values.push(...strings(v));
    res = put(res, kp);
  }
  return scrub({ ...out, result: res as Json }, values) as Outcome;
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
