// ANY verb whose reply carries raw bytes answers on the binary channel — the ts
// mirror of python `io_bridge/tests/test_any_verb_bytes_reply.py`.
//
// There is no list of binary verbs. A bundle's own verb — `events`, the shape of
// a terminal-output poll — is called by a TEXT call frame (no bytes in the call);
// its reply holds raw bytes, so the reply goes out BINARY, found by the codec
// (`encodeFrame` → `findBinaryPath`) and nothing else:
//
//   (d) the PEER role: a host's TEXT call to one of this kernel's agents is
//       answered by a BINARY reply frame, the body at its `_binary_path` array —
//       byte-identical to python's head; the same verb answering no bytes is a
//       TEXT frame;
//   (e) bytes at a NESTED key / inside a list: the full path rides the head;
//   (c) the CALLER role: this kernel's host-bound send crosses as TEXT, the host's
//       BINARY reply comes back, and the outcome's result holds the bytes.
//
// The frames are captured RAW (string vs ArrayBuffer) — the frame kind is the
// assertion, so nothing here decodes before checking it.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import { WsBridge } from "../src/transport/bridge.ts";
import type { WebSocketLike } from "../src/transport/bridge.ts";

// python: CHUNK = b"\x1b[31mred\x1b[0m\x00\xff\r\n"
const CHUNK = new Uint8Array([
  0x1b, 0x5b, 0x33, 0x31, 0x6d, 0x72, 0x65, 0x64, 0x1b, 0x5b, 0x30, 0x6d, 0x00, 0xff, 0x0d, 0x0a,
]);

/** One frame as it crossed the socket: its kind, and its raw data. */
type Wire = { kind: "text"; text: string } | { kind: "binary"; head: string; body: Uint8Array };

/** A binary frame → (its JSON head as written, its raw body). */
function split(buf: ArrayBuffer): { head: string; body: Uint8Array } {
  const n = new DataView(buf).getUint32(0, false);
  return {
    head: new TextDecoder().decode(new Uint8Array(buf, 4, n)),
    body: new Uint8Array(buf.slice(4 + n)),
  };
}

/** A python-built binary frame: [4-byte BE len | head | body]. */
function binaryFrame(head: string, body: Uint8Array): ArrayBuffer {
  const h = new TextEncoder().encode(head);
  const out = new ArrayBuffer(4 + h.length + body.length);
  new DataView(out).setUint32(0, h.length, false);
  new Uint8Array(out, 4, h.length).set(h);
  new Uint8Array(out, 4 + h.length).set(body);
  return out;
}

class RawSocket implements WebSocketLike {
  binaryType = "blob";
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  readonly wire: Wire[] = [];
  constructor() {
    setTimeout(() => this.onopen?.(undefined), 0);
  }
  send(data: string | ArrayBuffer): void {
    if (typeof data === "string") this.wire.push({ kind: "text", text: data });
    else this.wire.push({ kind: "binary", ...split(data) });
  }
  close(): void {}
  /** host → this kernel, raw (a TEXT string or a BINARY ArrayBuffer) */
  deliver(data: string | ArrayBuffer): void {
    this.onmessage?.({ data });
  }
}

async function until(fn: () => boolean, ms = 1000): Promise<void> {
  const start = Date.now();
  while (!fn()) {
    if (Date.now() - start > ms) throw new Error("until: timeout");
    await new Promise((r) => setTimeout(r, 1));
  }
}

/** The probe: a bundle's own verbs, nothing declared. Bytes are not in the
 *  `Json` lattice, so a reply holding them is cast — the wire carries them. */
const calls: string[] = [];
function probe(_id: string, payload: Payload): Json {
  const verb = String(payload["type"]);
  calls.push(verb);
  const replies: Record<string, unknown> = {
    events: { seq: 3, chunk: CHUNK },
    idle: { seq: 3 },
    nested: { seq: 3, out: { chunk: CHUNK } },
    listed: { events: [{ n: 0 }, { n: 1, bytes: CHUNK }] },
  };
  return (replies[verb] ?? { error: `probe: no verb '${verb}'` }) as Json;
}

async function peer(): Promise<{ bridge: WsBridge; sock: RawSocket }> {
  calls.length = 0;
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state", meta: { guid: "P-g" } }));
  k.register(
    new Agent({ id: "probe", parentId: "kernel_state", meta: { callable_by: { "H-g|h_rest": "*" } }, handler: probe }),
  );
  let sock: RawSocket | null = null;
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: "kernel_state",
    makeSocket: () => (sock = new RawSocket()),
  });
  bridge.announce();
  await until(() => sock?.wire.some((w) => w.kind === "text" && w.text.includes('"hello"')) ?? false);
  return { bridge, sock: sock! };
}

/** The host's TEXT call frame, as python's web_ws writes it. */
function hostCall(id: string, verb: string): string {
  return `{"type": "call", "id": "${id}", "target": "probe", "payload": {"type": "${verb}"}, "caller": "H-g|h_rest"}`;
}

function replyTo(sock: RawSocket, id: string): Wire | undefined {
  return sock.wire.find((w) =>
    (w.kind === "text" ? w.text : w.head).startsWith(`{"type": "reply", "id": "${id}"`),
  );
}

// ─── (d) the peer role ──────────────────────────────────────────────────────

test("(d) a host's TEXT call to a custom verb is answered by a BINARY reply frame; no bytes ⇒ TEXT", async () => {
  const { bridge, sock } = await peer();

  sock.deliver(hostCall("e1", "events"));
  await until(() => replyTo(sock, "e1") !== undefined);
  const r1 = replyTo(sock, "e1")!;
  assert.equal(r1.kind, "binary");
  assert.equal(
    (r1 as { head: string }).head,
    '{"type": "reply", "id": "e1", "data": {"seq": 3, "chunk": null}, "_binary_path": ["data", "chunk"]}',
  );
  assert.deepEqual((r1 as { body: Uint8Array }).body, CHUNK);

  // the SAME verb family with no bytes in its reply: a plain TEXT frame
  sock.deliver(hostCall("e2", "idle"));
  await until(() => replyTo(sock, "e2") !== undefined);
  assert.deepEqual(replyTo(sock, "e2"), { kind: "text", text: '{"type": "reply", "id": "e2", "data": {"seq": 3}}' });

  assert.deepEqual(calls, ["events", "idle"]);
  bridge.close();
});

// ─── (e) bytes at any depth ─────────────────────────────────────────────────

for (const [verb, data, path] of [
  ["nested", '{"seq": 3, "out": {"chunk": null}}', '["data", "out", "chunk"]'],
  ["listed", '{"events": [{"n": 0}, {"n": 1, "bytes": null}]}', '["data", "events", 1, "bytes"]'],
] as const) {
  test(`(e) a reply's bytes ${verb === "nested" ? "at a nested key" : "inside a list"} ride the head's full _binary_path`, async () => {
    const { bridge, sock } = await peer();
    sock.deliver(hostCall("n1", verb));
    await until(() => replyTo(sock, "n1") !== undefined);
    const r = replyTo(sock, "n1")!;
    assert.equal(r.kind, "binary");
    assert.equal(
      (r as { head: string }).head,
      `{"type": "reply", "id": "n1", "data": ${data}, "_binary_path": ${path}}`,
    );
    assert.deepEqual((r as { body: Uint8Array }).body, CHUNK);
    assert.deepEqual(calls, [verb]);
    bridge.close();
  });
}

// ─── (c) the caller role ────────────────────────────────────────────────────

test("(c) a host-bound send crosses as TEXT; the host's BINARY reply lands in the outcome with its bytes", async () => {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state", meta: { guid: "P-g" } }));
  let sock: RawSocket | null = null;
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: "kernel_state",
    makeSocket: () => (sock = new RawSocket()),
  });
  // `probe` is not held here: the send is FORWARDED to the host
  const pending = k.send([{ target: "probe", payload: { type: "events" } }]);
  const isCall = (w: Wire): boolean => w.kind === "text" && w.text.startsWith('{"type": "call"');
  await until(() => sock?.wire.some(isCall) ?? false);
  const call = sock!.wire.find(isCall) as { kind: "text"; text: string };
  const frame = JSON.parse(call.text) as Payload;
  assert.equal(frame["target"], "probe");
  assert.deepEqual(frame["payload"], { type: "events" });

  // the host answers as python's web_ws does: the bytes at their `_binary_path`
  sock!.deliver(
    binaryFrame(
      `{"type": "reply", "id": "${String(frame["id"])}", "data": {"seq": 3, "chunk": null}, "_binary_path": ["data", "chunk"]}`,
      CHUNK,
    ),
  );
  const [out] = await pending;
  assert.equal(out!.status, "completed");
  const result = out!.result as unknown as { seq: number; chunk: Uint8Array };
  assert.equal(result.seq, 3);
  assert.ok(result.chunk instanceof Uint8Array);
  assert.deepEqual(new Uint8Array(result.chunk), CHUNK);
  // the call crossed as TEXT — no empty body to make it binary
  assert.equal(sock!.wire.filter((w) => w.kind === "binary").length, 0);
  bridge.close();
});
