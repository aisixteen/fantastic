// Wire-frame codec — byte-identical to the shared io_bridge binary-frame codec
// (python `io_bridge/_codec.py` encode_frame/decode_frame): the JSON is python's
// `json.dumps` (`", "` / `": "` separators, ASCII escaping — `pyDumps`), so a
// text frame and a binary head match the other runtimes byte for byte, and the
// decode is python's STRICT `loads` (`strictLoads`). Two formats, auto-selected
// by content:
//
//   text:   JSON (no bytes anywhere in the envelope)
//   binary: [4-byte BE uint32 headLen | JSON head | raw body]
//           head has the first bytes value nulled + `_binary_path`
//           naming where it lived.
//
// `_binary_path` is a JSON ARRAY of steps from the envelope root: a STRING step is an
// object key, taken literally (a key may hold `.`); an INTEGER step is a list index.
// `["payload", "bytes"]`, `["payload", "a.b"]` (the top-level payload key `"a.b"`),
// `["data", "items", 0, "data"]`. A dotted string could not tell the key `"a.b"` from
// the nested `a` → `b`, so a payload whose bytes sat at a literal dotted key (web_rest's
// `X-Fantastic-Bytes-Field` binds one) arrived rebuilt as a nested slot.
//
// This file lives in transport/ (not the DOM-less kernel/) because it
// uses TextEncoder/DataView — platform globals, not view concerns.

import { pyDumps, pyRepr, pyReprAt, strictLoads, wireFloat } from "../kernel/pyjson.ts";

/** An on-wire envelope. May carry one binary blob (ArrayBuffer/typed-array). */
export type Envelope = { [key: string]: unknown };

export type EncodedFrame =
  | { data: string; binary: false }
  | { data: ArrayBuffer; binary: true };

function isBinary(v: unknown): v is ArrayBuffer | ArrayBufferView {
  return v instanceof ArrayBuffer || ArrayBuffer.isView(v);
}

/** One step of a `_binary_path`: an object key (a string, taken literally — it
 *  may hold `.`) or a list index (an integer). */
type BinaryPathStep = string | number;

/** python `find_bytes_path`: walk depth-first; `[path, value]` of the first
 *  binary value — the path as `_binary_path` spells it (a string per object key,
 *  a number per list index) — or null. */
function findBinaryPath(
  obj: unknown,
  prefix: BinaryPathStep[] = [],
): [BinaryPathStep[], ArrayBuffer | ArrayBufferView] | null {
  if (isBinary(obj)) return [prefix, obj];
  if (Array.isArray(obj)) {
    for (let i = 0; i < obj.length; i++) {
      const r = findBinaryPath(obj[i], [...prefix, i]);
      if (r !== null) return r;
    }
  } else if (obj !== null && typeof obj === "object") {
    for (const k of Object.keys(obj as object)) {
      const r = findBinaryPath((obj as Record<string, unknown>)[k], [...prefix, k]);
      if (r !== null) return r;
    }
  }
  return null;
}

/** The JSON name of a scalar, for an error (python `_kind`). */
function kind(v: unknown): string {
  if (v === null || v === undefined) return "null";
  if (typeof v === "boolean") return "boolean";
  if (typeof v === "number") return "number";
  return "string";
}

/** python `set_path`: put `value` at `path` (a `_binary_path`: a non-empty list
 *  of steps) in obj. The path comes off the wire, so it is checked step by step:
 *  at an object the step is a string key (every key before the last must be an
 *  OWN key — never a prototype member, so `constructor`/`__proto__` are ordinary
 *  keys as in python; the last is set), at a list a non-negative integer index
 *  inside it. Anything else throws an `Error` naming the step, with python's text. */
function setPath(obj: unknown, path: unknown, value: unknown): void {
  if (!Array.isArray(path)) throw new Error("not a list of steps");
  if (path.length === 0) throw new Error("an empty path names no slot");
  let cur: unknown = obj;
  for (let n = 0; n < path.length; n++) {
    const step: unknown = path[n];
    const last = n === path.length - 1;
    if (Array.isArray(cur)) {
      // A FLOAT step (owner 2026-10-03, fixed): python reads `1.0`, `1e0`, `1e20`
      // as a float and refuses it here; JS holds it as the integer 1. The head is
      // parsed by `strictLoads`, which keeps each number's spelling from the
      // parse's source text, so `wireFloat` names the step python refuses.
      if (typeof step !== "number" || !Number.isInteger(step) || step < 0 || wireFloat(path, n)) {
        throw new Error(`step ${pyReprAt(path, n)} at a list is not a non-negative integer index`);
      }
      if (step >= cur.length) throw new Error(`index ${step} is past the end of a list of ${cur.length}`);
      if (last) cur[step] = value;
      else cur = cur[step];
    } else if (cur !== null && typeof cur === "object") {
      if (typeof step !== "string") throw new Error(`step ${pyReprAt(path, n)} at an object is not a string key`);
      // An OWN data property: `__proto__` assigned with `=` would replace the
      // object's prototype instead of setting a key.
      if (last) Object.defineProperty(cur, step, { value, writable: true, enumerable: true, configurable: true });
      else if (Object.hasOwn(cur, step)) cur = (cur as Record<string, unknown>)[step];
      else throw new Error(`no key ${pyRepr(step)}`);
    } else {
      throw new Error(`step ${pyReprAt(path, n)} goes into a ${kind(cur)}, not a container`);
    }
  }
}

function deepClone(obj: unknown): unknown {
  if (obj === null || typeof obj !== "object") return obj;
  if (Array.isArray(obj)) return obj.map(deepClone);
  const out: Record<string, unknown> = {};
  for (const k of Object.keys(obj as object)) {
    out[k] = deepClone((obj as Record<string, unknown>)[k]);
  }
  return out;
}

function asArrayBuffer(v: unknown): ArrayBuffer {
  if (v instanceof ArrayBuffer) return v;
  if (ArrayBuffer.isView(v)) {
    // .buffer is ArrayBufferLike (could be SharedArrayBuffer); we only ever
    // produce real ArrayBuffers here, so narrow it.
    return v.buffer.slice(
      v.byteOffset,
      v.byteOffset + v.byteLength,
    ) as ArrayBuffer;
  }
  throw new Error("asArrayBuffer: not binary");
}

export function encodeFrame(envelope: Envelope): EncodedFrame {
  const found = findBinaryPath(envelope);
  if (found === null) {
    return { data: pyDumps(envelope), binary: false };
  }
  const [path, value] = found;
  const body = asArrayBuffer(value);
  const head = deepClone(envelope) as Record<string, unknown>;
  setPath(head, path, null);
  head["_binary_path"] = path;
  const headBytes = new TextEncoder().encode(pyDumps(head));
  const frame = new ArrayBuffer(4 + headBytes.length + body.byteLength);
  const view = new DataView(frame);
  view.setUint32(0, headBytes.length, false); // big-endian
  new Uint8Array(frame, 4, headBytes.length).set(headBytes);
  new Uint8Array(frame, 4 + headBytes.length).set(new Uint8Array(body));
  return { data: frame, binary: true };
}

/**
 * Parse a wire frame back into an envelope (python `decode_frame`). A frame
 * arrives from a PEER, so every length in it is hostile until checked; every
 * malformed frame — bad JSON, a value no runtime can hold, a header that claims
 * more bytes than follow, a bad `_binary_path` — throws the SAME kind of `Error`,
 * with python's text, so a caller catches one thing and keeps its socket.
 */
export function decodeFrame(data: string | ArrayBuffer): Envelope {
  if (typeof data === "string") {
    try {
      return strictLoads(data) as Envelope;
    } catch (e) {
      throw new Error(`bad text frame: ${errText(e)}`);
    }
  }
  if (data.byteLength < 4) {
    throw new Error(`binary frame shorter than its 4-byte header (${data.byteLength}B)`);
  }
  const headLen = new DataView(data).getUint32(0, false);
  if (4 + headLen > data.byteLength) {
    throw new Error(
      `binary frame header claims ${headLen}B but only ${data.byteLength - 4}B follow the length prefix`,
    );
  }
  let head: unknown;
  try {
    head = strictLoads(new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array(data, 4, headLen)));
  } catch (e) {
    throw new Error(`bad binary frame header: ${errText(e)}`);
  }
  const body = new Uint8Array(data, 4 + headLen);
  if (head === null || typeof head !== "object" || Array.isArray(head)) return head as Envelope;
  const obj = head as Record<string, unknown>;
  const path = obj["_binary_path"];
  delete obj["_binary_path"];
  if (path !== undefined && path !== null) {
    try {
      setPath(obj, path, body);
    } catch (e) {
      throw new Error(`bad _binary_path ${pyRepr(path)}: ${errText(e)}`);
    }
  }
  return obj;
}

function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
