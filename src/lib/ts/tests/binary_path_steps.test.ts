// `_binary_path` is a LIST OF STEPS — the ts mirror
// of python's `io_bridge/tests/test_binary_path_steps.py`.
//
// A binary frame's header names where its raw body belongs. The path is a JSON
// array — not a dotted string, which cannot tell a LITERAL key `"a.b"` (which
// web_rest's `X-Fantastic-Bytes-Field: a.b` binds) from a NESTED `a` → `b` slot: a
// string step is an object key, taken literally; an integer step is a list index.
//
//   (a) the codec: literal, nested and list paths round-trip, each head as python
//       writes it; every malformed path off the wire is one Error;
//   (b) the WS bridge, both ways: a page call carrying a literal key goes out as
//       python's head and the host's reply comes back to its literal key; a host
//       call carrying one reaches the page's agent and the reply goes back the
//       same way.
//
// Every head and message below was printed by python's codec.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { WsBridge } from "../src/transport/bridge.ts";
import type { WebSocketLike } from "../src/transport/bridge.ts";
import { encodeFrame, decodeFrame } from "../src/transport/frame.ts";
import type { Envelope } from "../src/transport/frame.ts";
import type { Json, Payload } from "../src/kernel/json.ts";

const RAW = [0, 114, 97, 119, 255]; // b"\x00raw\xff"

/** `[head JSON text, body bytes]` of a binary frame. */
function split(buf: ArrayBuffer): [string, number[]] {
  const n = new DataView(buf).getUint32(0, false);
  return [new TextDecoder().decode(new Uint8Array(buf, 4, n)), [...new Uint8Array(buf, 4 + n)]];
}

/** The parsed head of a binary frame. */
function head(buf: ArrayBuffer): Record<string, unknown> {
  return JSON.parse(split(buf)[0]) as Record<string, unknown>;
}

/** A binary frame as a peer would send it: `[BE uint32 | head JSON | body]`. A
 *  head given as TEXT is sent as spelled (`1.0`, which `JSON.stringify` cannot write). */
function binaryFrame(h: object | string, body: number[] = [1, 2]): ArrayBuffer {
  const t = new TextEncoder().encode(typeof h === "string" ? h : JSON.stringify(h));
  const buf = new Uint8Array(4 + t.length + body.length);
  new DataView(buf.buffer).setUint32(0, t.length, false);
  buf.set(t, 4);
  buf.set(body, 4 + t.length);
  return buf.buffer;
}

/** Bytes as plain numbers, so a decoded envelope compares with `deepEqual`. */
function plain(x: unknown): unknown {
  if (x instanceof Uint8Array) return { bytes: [...x] };
  if (Array.isArray(x)) return x.map(plain);
  if (x !== null && typeof x === "object") {
    return Object.fromEntries(Object.entries(x as Record<string, unknown>).map(([k, v]) => [k, plain(v)]));
  }
  return x;
}

// ─── (a) the codec ───────────────────────────────────────────────────────────

test("a literal dotted key round-trips beside its nested twin", () => {
  const env = { type: "call", id: "1", target: "echo", payload: { type: "take", a: { b: "kept" }, "a.b": new Uint8Array(RAW) } };
  const enc = encodeFrame(env);
  assert.equal(enc.binary, true);
  const buf = enc.data as ArrayBuffer;
  assert.equal(buf.byteLength, 152);
  assert.equal(new DataView(buf).getUint32(0, false), 0x8f);
  // the null sits at the literal key; the nested twin is untouched
  assert.deepEqual(split(buf), [
    '{"type": "call", "id": "1", "target": "echo", "payload": {"type": "take", "a": {"b": "kept"}, "a.b": null}, "_binary_path": ["payload", "a.b"]}',
    RAW,
  ]);
  assert.deepEqual(plain(decodeFrame(buf)), plain(env));
});

test("nested, list and odd-key paths round-trip", () => {
  const cases: [Envelope, unknown[]][] = [
    [{ data: { bytes: new Uint8Array([1]) } }, ["data", "bytes"]],
    [{ data: { items: [{ n: 1 }, { data: new Uint8Array([2]) }] } }, ["data", "items", 1, "data"]],
    [{ payload: [new Uint8Array([3]), "tail"] }, ["payload", 0]],
    // a key that looks like an index stays a string key
    [{ payload: { "0": new Uint8Array([4]) } }, ["payload", "0"]],
    [{ payload: { "": new Uint8Array([5]) } }, ["payload", ""]],
    [{ payload: { "a..b.": { "c.": new Uint8Array([6]) } } }, ["payload", "a..b.", "c."]],
  ];
  for (const [env, path] of cases) {
    const enc = encodeFrame(env);
    assert.equal(enc.binary, true);
    assert.deepEqual(head(enc.data as ArrayBuffer)["_binary_path"], path);
    assert.deepEqual(plain(decodeFrame(enc.data)), plain(env), JSON.stringify(path));
  }
});

// python `set_path`: the path comes off the wire, so every malformed path — the
// old dotted string, an empty list, a step of the wrong kind, an index past the
// end, a missing key, a step into a scalar — is one Error with python's text.
test("a malformed _binary_path off the wire is one Error with python's text", () => {
  const h = { type: "reply", items: [null, null], n: 1 };
  const refused: [unknown, string][] = [
    ["payload.bytes", "'payload.bytes': not a list of steps"], // the old dotted spelling
    [[], "[]: an empty path names no slot"],
    [{ payload: "bytes" }, "{'payload': 'bytes'}: not a list of steps"],
    [5, "5: not a list of steps"],
    [true, "True: not a list of steps"],
    [[0], "[0]: step 0 at an object is not a string key"],
    [[null], "[None]: step None at an object is not a string key"],
    [["items", "0"], "['items', '0']: step '0' at a list is not a non-negative integer index"],
    [["items", -1], "['items', -1]: step -1 at a list is not a non-negative integer index"],
    [["items", true], "['items', True]: step True at a list is not a non-negative integer index"],
    [["items", 1.5], "['items', 1.5]: step 1.5 at a list is not a non-negative integer index"],
    [["items", null], "['items', None]: step None at a list is not a non-negative integer index"],
    [["items", [0]], "['items', [0]]: step [0] at a list is not a non-negative integer index"],
    [["items", "\u00e9\n"], "['items', '\u00e9\\n']: step '\u00e9\\n' at a list is not a non-negative integer index"],
    [["items", 2], "['items', 2]: index 2 is past the end of a list of 2"],
    [["nope", "x"], "['nope', 'x']: no key 'nope'"],
    [["constructor", "x"], "['constructor', 'x']: no key 'constructor'"],
    [["type", "x"], "['type', 'x']: step 'x' goes into a string, not a container"],
    [["items", 0, "x"], "['items', 0, 'x']: step 'x' goes into a null, not a container"],
    [["n", "x"], "['n', 'x']: step 'x' goes into a number, not a container"],
  ];
  for (const [path, tail] of refused) {
    assert.throws(() => decodeFrame(binaryFrame({ ...h, _binary_path: path })), {
      message: `bad _binary_path ${tail}`,
    });
  }
  assert.throws(() => decodeFrame(binaryFrame({ t: true, _binary_path: ["t", 0] })), {
    message: "bad _binary_path ['t', 0]: step 0 goes into a boolean, not a container",
  });
  // absent or null ⇒ no body is placed
  assert.deepEqual(decodeFrame(binaryFrame({ type: "x" })), { type: "x" });
  assert.deepEqual(decodeFrame(binaryFrame({ type: "x", _binary_path: null })), { type: "x" });
  // an index inside the list places the body there: no hole, no stray property
  for (const [path, at] of [[["a", 0], 0], [["a", 2], 2]] as [unknown[], number][]) {
    const out = decodeFrame(binaryFrame({ a: [0, 0, 0], _binary_path: path })) as { a: unknown[] };
    assert.ok(out.a[at] instanceof Uint8Array, String(path));
    assert.deepEqual([...(out.a[at] as Uint8Array)], [1, 2]);
    assert.equal(out.a.length, 3);
    assert.deepEqual(Object.keys(out.a).length, out.a.length);
  }
  // A FLOAT step: python's `json.loads` reads `1.0`, `1e0`, `1e20`, `-0.0` as a
  // float, never an index. JS holds them as integers; the strict wire loads
  // keeps their spelling (`pyjson.ts`), so the same texts come back. `-0` and `1`
  // are integers in python too, and place the body.
  const floats: [string, string][] = [
    ['["a", 1.0]', "['a', 1.0]: step 1.0 at a list is not a non-negative integer index"],
    ['["a", 1e0]', "['a', 1.0]: step 1.0 at a list is not a non-negative integer index"],
    ['["a", 1E0]', "['a', 1.0]: step 1.0 at a list is not a non-negative integer index"],
    ['["a", 1e20]', "['a', 1e+20]: step 1e+20 at a list is not a non-negative integer index"],
    ['["a", -0.0]', "['a', -0.0]: step -0.0 at a list is not a non-negative integer index"],
    ['["a", -1.0]', "['a', -1.0]: step -1.0 at a list is not a non-negative integer index"],
    ['["o", 1.0]', "['o', 1.0]: step 1.0 at an object is not a string key"],
    ['["n", 1.0]', "['n', 1.0]: step 1.0 goes into a number, not a container"],
  ];
  for (const [path, tail] of floats) {
    assert.throws(() => decodeFrame(binaryFrame(`{"a": [0, 0, 0], "o": {"x": 0}, "n": 5, "_binary_path": ${path}}`)), {
      message: `bad _binary_path ${tail}`,
    });
  }
  for (const [path, at] of [['["a", -0]', 0], ['["a", 1]', 1]] as [string, number][]) {
    const out = decodeFrame(binaryFrame(`{"a": [0, 0, 0], "_binary_path": ${path}}`)) as { a: unknown[] };
    assert.ok(out.a[at] instanceof Uint8Array, path);
  }
  // `__proto__` is an ordinary OWN key, read and set as python reads and sets it
  const proto = decodeFrame(binaryFrame({ ["__proto__"]: { x: 1 }, _binary_path: ["__proto__", "y"] }));
  assert.ok(Object.hasOwn(proto, "__proto__"));
  const inner = (proto as Record<string, Record<string, unknown>>)["__proto__"] as Record<string, unknown>;
  assert.deepEqual(Object.keys(inner), ["x", "y"]);
  assert.deepEqual([...(inner["y"] as Uint8Array)], [1, 2]);
});

// ─── (b) the WS bridge, both ways ────────────────────────────────────────────

/** A WebSocket-shaped stand-in that keeps every frame RAW, as the wire carries it. */
class RawSocket implements WebSocketLike {
  binaryType = "blob";
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  readonly sent: (string | ArrayBuffer)[] = [];
  constructor() {
    setTimeout(() => this.onopen?.(undefined), 0);
  }
  send(data: string | ArrayBuffer): void {
    this.sent.push(data);
  }
  close(): void {}
  deliver(env: Envelope): void {
    this.onmessage?.({ data: encodeFrame(env).data });
  }
  /** decoded frames of a type, with the raw frame each came from */
  frames(type: string): [Envelope, string | ArrayBuffer][] {
    return this.sent.map((raw) => [decodeFrame(raw), raw] as [Envelope, string | ArrayBuffer]).filter(([f]) => f["type"] === type);
  }
}

async function until(fn: () => boolean, ms = 3000): Promise<void> {
  const start = Date.now();
  while (!fn()) {
    if (Date.now() - start > ms) throw new Error("until: timeout");
    await new Promise((r) => setTimeout(r, 1));
  }
}

const ROOT = "kernel_state";

function wired(guid: string) {
  const socks: RawSocket[] = [];
  const k = new Kernel();
  k.setRoot(new Agent({ id: ROOT, meta: { guid } }));
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: ROOT,
    makeSocket: () => {
      const s = new RawSocket();
      socks.push(s);
      return s;
    },
    maxReconnectDelay: 5,
  });
  return { k, bridge, socks };
}

test("a page call with bytes at a literal key goes out as python's head; the reply's literal key comes back", async () => {
  const { k, bridge, socks } = wired("g1");
  const p = k.forwardBinary("echo", { type: "take", a: { b: "kept" }, "a.b": new Uint8Array(RAW) });
  await until(() => (socks[0]?.frames("call").length ?? 0) === 1);
  const sock = socks[0]!;
  const [call, raw] = sock.frames("call")[0]!;
  assert.ok(raw instanceof ArrayBuffer, "bytes ride a binary frame");
  const h = head(raw);
  assert.deepEqual(h["_binary_path"], ["payload", "a.b"]);
  assert.deepEqual(h["payload"], { type: "take", a: { b: "kept" }, "a.b": null });
  assert.deepEqual(split(raw)[1], RAW);
  const reply = encodeFrame({ type: "reply", id: call["id"], data: { keys: ["a", "a.b", "type"], "x.y": new Uint8Array(RAW), x: { y: "kept" } } });
  assert.deepEqual(head(reply.data as ArrayBuffer)["_binary_path"], ["data", "x.y"]);
  sock.onmessage?.({ data: reply.data });
  assert.deepEqual(plain(await p), { keys: ["a", "a.b", "type"], "x.y": { bytes: RAW }, x: { y: "kept" } });
  bridge.close();
});

test("a host call with bytes at a literal key reaches the page's agent there; the reply goes back the same way", async () => {
  const { k, bridge, socks } = wired("p1");
  const got: Payload[] = [];
  k.registerBundle("binpath_echo.ts", (_id, payload) => {
    got.push(payload);
    return { keys: Object.keys(payload).sort(), "x.y": payload["a.b"] ?? null, x: { y: "kept" } } as unknown as Json;
  });
  k.register(new Agent({ id: "echo", parentId: ROOT, handlerModule: "binpath_echo.ts", meta: { callable_by: { "h0|*": "*" } } }));
  bridge.announce();
  await until(() => (socks[0]?.frames("hello").length ?? 0) === 1);
  const sock = socks[0]!;
  const call = encodeFrame({ type: "call", id: "h1", target: "echo", payload: { type: "take", a: { b: "kept" }, "a.b": new Uint8Array(RAW) }, caller: "h0|cell" });
  assert.deepEqual(head(call.data as ArrayBuffer)["_binary_path"], ["payload", "a.b"]);
  sock.onmessage?.({ data: call.data });
  await until(() => sock.frames("reply").length === 1);
  assert.deepEqual(plain(got), [{ type: "take", a: { b: "kept" }, "a.b": { bytes: RAW } }]);
  const [reply, raw] = sock.frames("reply")[0]!;
  assert.ok(raw instanceof ArrayBuffer, "the reply's bytes ride a binary frame");
  assert.deepEqual(head(raw)["_binary_path"], ["data", "x.y"]);
  assert.deepEqual(plain(reply), { type: "reply", id: "h1", data: { keys: ["a", "a.b", "type"], "x.y": { bytes: RAW }, x: { y: "kept" } } });
  bridge.close();
});

// ─── ONE binary body per frame ─────────────────────────────────────────────
// Python `io_bridge` text for text: an envelope holding two binary values is
// refused naming both paths; a reply
// or event a sender cannot carry goes as its routing keys and the refusal.

test("an envelope with two binary bodies is refused naming both paths", async () => {
  const { encodeFrame, FrameError } = await import("../src/transport/frame.ts");
  const env = { type: "reply", id: "1", data: { a: new Uint8Array([1]).buffer, more: [{ b: new Uint8Array([2]).buffer }] } };
  assert.throws(() => encodeFrame(env), (e: unknown) => {
    assert.ok(e instanceof FrameError);
    assert.equal(
      (e as Error).message,
      'a frame carries ONE binary body — this one holds 2, at ["data", "a"], ["data", "more", 0, "b"]',
    );
    return true;
  });
});

test("sendable answers a reply or an event it cannot carry with the refusal; a call is refused", async () => {
  const { encodeFrame, sendable, FrameError } = await import("../src/transport/frame.ts");
  const one = { type: "reply", id: "1", data: { chunk: new Uint8Array([1]).buffer } };
  assert.equal(sendable(one), one, "one body: sent as it is");
  const out = sendable({ type: "reply", id: "7", data: { a: new Uint8Array([1]).buffer, b: new Uint8Array([2]).buffer } });
  assert.equal(out["type"], "reply");
  assert.equal(out["id"], "7");
  const data = out["data"] as { reason: string; error: string };
  assert.equal(data.reason, "transport_error");
  assert.ok(data.error.includes("ONE binary body"));
  assert.equal(encodeFrame(out).binary, false);
  const ev = sendable({ type: "event", src: "s", payload: [new Uint8Array([1]).buffer, new Uint8Array([2]).buffer] });
  assert.equal(ev["src"], "s");
  assert.equal((ev["payload"] as { reason: string }).reason, "transport_error");
  assert.throws(
    () => sendable({ type: "call", id: "1", target: "t", payload: { a: new Uint8Array([1]).buffer, b: new Uint8Array([2]).buffer } }),
    FrameError,
  );
});
