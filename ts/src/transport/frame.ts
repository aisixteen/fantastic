// Wire-frame codec — byte-identical to python web_ws/_proxy.py
// (encode_outbound/decode_inbound) and the legacy transport.js
// (encodeFrame/decodeFrame). Two formats, auto-selected by content:
//
//   text:   JSON (no bytes anywhere in the envelope)
//   binary: [4-byte BE uint32 headLen | JSON head | raw body]
//           head has the first bytes value nulled + `_binary_path`
//           naming its dotted location.
//
// This file lives in transport/ (not the DOM-less kernel/) because it
// uses TextEncoder/DataView — platform globals, not view concerns.

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
    cur = Array.isArray(cur)
      ? cur[Number(part)]
      : (cur as Record<string, unknown>)[part];
  }
  return cur;
}

// Reject prototype-polluting keys in a dotted path before any assignment — a
// remote peer could otherwise smuggle `__proto__` / `constructor` / `prototype`
// through a state-patch path and poison Object.prototype.
function isUnsafeKey(k: string): boolean {
  return k === "__proto__" || k === "constructor" || k === "prototype";
}

function setPath(obj: unknown, path: string, value: unknown): void {
  const parts = path.split(".");
  if (parts.some(isUnsafeKey)) return;
  let cur: unknown = obj;
  for (let i = 0; i < parts.length - 1; i++) {
    const part = parts[i] as string;
    cur = Array.isArray(cur)
      ? cur[Number(part)]
      : (cur as Record<string, unknown>)[part];
  }
  const last = parts[parts.length - 1] as string;
  if (Array.isArray(cur)) cur[Number(last)] = value;
  else (cur as Record<string, unknown>)[last] = value;
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
    return { data: JSON.stringify(envelope), binary: false };
  }
  const body = asArrayBuffer(getPath(envelope, path));
  const head = deepClone(envelope) as Record<string, unknown>;
  setPath(head, path, null);
  head["_binary_path"] = path;
  const headBytes = new TextEncoder().encode(JSON.stringify(head));
  const frame = new ArrayBuffer(4 + headBytes.length + body.byteLength);
  const view = new DataView(frame);
  view.setUint32(0, headBytes.length, false); // big-endian
  new Uint8Array(frame, 4, headBytes.length).set(headBytes);
  new Uint8Array(frame, 4 + headBytes.length).set(new Uint8Array(body));
  return { data: frame, binary: true };
}

export function decodeFrame(data: string | ArrayBuffer): Envelope {
  if (typeof data === "string") return JSON.parse(data) as Envelope;
  const view = new DataView(data);
  const headLen = view.getUint32(0, false);
  const headStr = new TextDecoder().decode(new Uint8Array(data, 4, headLen));
  const head = JSON.parse(headStr) as Record<string, unknown>;
  const body = new Uint8Array(data, 4 + headLen);
  const path = head["_binary_path"];
  delete head["_binary_path"];
  if (typeof path === "string") setPath(head, path, body);
  return head;
}
