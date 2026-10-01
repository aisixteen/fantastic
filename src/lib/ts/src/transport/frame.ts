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
//           naming its dotted location.
//
// This file lives in transport/ (not the DOM-less kernel/) because it
// uses TextEncoder/DataView — platform globals, not view concerns.

import { pyDumps, pyInt, pyRepr, strictLoads } from "../kernel/pyjson.ts";

/** An on-wire envelope. May carry one binary blob (ArrayBuffer/typed-array). */
export type Envelope = { [key: string]: unknown };

export type EncodedFrame =
  | { data: string; binary: false }
  | { data: ArrayBuffer; binary: true };

function isBinary(v: unknown): v is ArrayBuffer | ArrayBufferView {
  return v instanceof ArrayBuffer || ArrayBuffer.isView(v);
}

function findBinaryPath(obj: unknown, prefix = ""): string | null {
  if (isBinary(obj)) return prefix;
  if (Array.isArray(obj)) {
    for (let i = 0; i < obj.length; i++) {
      const p = prefix ? `${prefix}.${i}` : `${i}`;
      const r = findBinaryPath(obj[i], p);
      if (r !== null) return r;
    }
  } else if (obj !== null && typeof obj === "object") {
    for (const k of Object.keys(obj as object)) {
      const p = prefix ? `${prefix}.${k}` : k;
      const r = findBinaryPath((obj as Record<string, unknown>)[k], p);
      if (r !== null) return r;
    }
  }
  return null;
}

function getPath(obj: unknown, path: string): unknown {
  let cur: unknown = obj;
  for (const part of path.split(".")) {
    cur = Array.isArray(cur) ? cur[Number(part)] : ownStep(cur, part);
  }
  return cur;
}

const PY_SSIZE_MIN = -(2n ** 63n);
const PY_SSIZE_MAX = 2n ** 63n - 1n;

/** A list step of a dotted path, as python `obj[int(part)]` reads it: `int()`'s
 *  literal (a sign, `_`, spaces, any decimal digit), a negative index counting
 *  from the end, and python's error for a part that is no int, an int no index
 *  can hold, or an index past either end — never a stray property or a hole. */
function listIndex(list: unknown[], part: string, assign: boolean): number {
  const i = pyInt(part);
  if (i < PY_SSIZE_MIN || i > PY_SSIZE_MAX) throw new Error("cannot fit 'int' into an index-sized integer");
  const at = i < 0n ? i + BigInt(list.length) : i;
  if (at < 0n || at >= BigInt(list.length)) {
    throw new Error(assign ? "list assignment index out of range" : "list index out of range");
  }
  return Number(at);
}

/** python `set_path`. */
/** A dict step as python's `cur[part]` reads it: the OWN key or KeyError —
 *  never a prototype member, so `constructor`/`__proto__`/`prototype` are
 *  ordinary keys exactly as in python (a refusal of them was a divergence). */
function ownStep(cur: unknown, part: string): unknown {
  if (cur === null || typeof cur !== "object" || !Object.hasOwn(cur, part)) {
    throw new Error(pyRepr(part));
  }
  return (cur as Record<string, unknown>)[part];
}

function setPath(obj: unknown, path: string, value: unknown): void {
  const parts = path.split(".");
  let cur: unknown = obj;
  for (let i = 0; i < parts.length - 1; i++) {
    const part = parts[i] as string;
    cur = Array.isArray(cur) ? cur[listIndex(cur, part, false)] : ownStep(cur, part);
  }
  const last = parts[parts.length - 1] as string;
  if (Array.isArray(cur)) cur[listIndex(cur, last, true)] = value;
  // An OWN data property: `__proto__` assigned with `=` would replace the
  // object's prototype instead of setting a key.
  else Object.defineProperty(cur, last, { value, writable: true, enumerable: true, configurable: true });
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
  const path = findBinaryPath(envelope);
  if (path === null) {
    return { data: pyDumps(envelope), binary: false };
  }
  const body = asArrayBuffer(getPath(envelope, path));
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
      if (typeof path !== "string") throw new Error("not a dotted path");
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
