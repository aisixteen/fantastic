import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { WsBridge } from "../src/transport/bridge.ts";
import type { WebSocketLike } from "../src/transport/bridge.ts";
import { encodeFrame, decodeFrame } from "../src/transport/frame.ts";
import type { Envelope } from "../src/transport/frame.ts";
import type { Payload } from "../src/kernel/json.ts";

// ─── frame codec ──────────────────────────────────────────────────────────

test("frame codec round-trips a text frame", () => {
  const env = { type: "call", target: "kernel_state", payload: { type: "reflect" }, id: "1" };
  const enc = encodeFrame(env);
  assert.equal(enc.binary, false);
  assert.deepEqual(decodeFrame(enc.data), env);
});

test("frame codec round-trips a binary frame (4-byte BE head + body)", () => {
  const bytes = new Uint8Array([0, 1, 2, 250, 255]);
  const enc = encodeFrame({
    type: "emit",
    target: "pty1",
    payload: { type: "paste_image", image: bytes },
  });
  assert.equal(enc.binary, true);
  const buf = enc.data as ArrayBuffer;
  // header length is a big-endian uint32 prefix
  const headLen = new DataView(buf).getUint32(0, false);
  assert.ok(headLen > 0 && headLen < buf.byteLength);
  const decoded = decodeFrame(buf) as {
    payload: { image: Uint8Array; type: string };
  };
  assert.equal(decoded.payload.type, "paste_image");
  assert.ok(decoded.payload.image instanceof Uint8Array);
  assert.deepEqual([...decoded.payload.image], [0, 1, 2, 250, 255]);
});

// python `io_bridge/_codec.py`: the bytes `encode_frame` writes (captured from
// the python codec) — `json.dumps` separators and ASCII escaping, so a text frame
// and a binary head (and its length prefix) match across runtimes.
test("frame codec writes python's bytes: text frame and binary head", () => {
  const text = encodeFrame({
    type: "call", target: "a",
    payload: { type: "x", n: [1, 2.5, "é", "\u2028"], ok: true, none: null }, id: "p-1",
  });
  assert.equal(
    text.data,
    '{"type": "call", "target": "a", "payload": {"type": "x", "n": [1, 2.5, "\\u00e9", "\\u2028"], "ok": true, "none": null}, "id": "p-1"}',
  );
  const bin = encodeFrame({
    type: "call", target: "a", payload: { type: "paste", bytes: new Uint8Array([0, 1, 2]), name: "ü" }, id: "p-2",
  });
  const buf = bin.data as ArrayBuffer;
  assert.equal(buf.byteLength, 152);
  assert.equal(new DataView(buf).getUint32(0, false), 0x91);
  assert.equal(
    new TextDecoder().decode(new Uint8Array(buf, 4, 0x91)),
    '{"type": "call", "target": "a", "payload": {"type": "paste", "bytes": null, "name": "\\u00fc"}, "id": "p-2", "_binary_path": ["payload", "bytes"]}',
  );
  assert.deepEqual([...new Uint8Array(buf, 4 + 0x91)], [0, 1, 2]);
});

// python `decode_frame` + the strict `loads` (D13): a value no runtime can hold
// is refused, a header that lies about its length is refused, each as one Error.
test("frame codec decodes STRICTLY and checks every length", () => {
  assert.throws(() => decodeFrame('{"a": NaN}'), /^Error: bad text frame: /);
  assert.throws(() => decodeFrame('{"a": 12345678901234567890}'), /bad text frame: integer 12345678901234567890 is outside int64/);
  assert.throws(() => decodeFrame('{"a": "\\udcff"}'), /bad text frame: a lone surrogate/);
  assert.throws(() => decodeFrame('{"a": 1e400}'), /bad text frame: number 1e400 overflows a double/);
  // the int64 bounds are read off the token's DIGITS, so both ends are exact
  // where a double cannot hold the value (what an int past 2^53 decodes TO is an
  // open owner decision, so no decoded value is pinned here)
  assert.doesNotThrow(() => decodeFrame('{"a": 9223372036854775807}'));
  assert.doesNotThrow(() => decodeFrame('{"a": -9223372036854775808}'));
  assert.throws(() => decodeFrame('{"a": 9223372036854775808}'), /^Error: bad text frame: integer 9223372036854775808 is outside int64/);
  assert.throws(() => decodeFrame('{"a": -9223372036854775809}'), /^Error: bad text frame: integer -9223372036854775809 is outside int64/);
  // a surrogate PAIR is one character, not a lone surrogate
  assert.deepEqual(decodeFrame('{"b": "\\ud83d\\ude00"}'), { b: "\u{1f600}" });
  assert.throws(() => decodeFrame(new Uint8Array([0, 1]).buffer), /binary frame shorter than its 4-byte header \(2B\)/);
  assert.throws(
    () => decodeFrame(new Uint8Array([0, 0, 0, 9, 123, 125]).buffer),
    /binary frame header claims 9B but only 2B follow the length prefix/,
  );
});

// python `encode_frame` writes floats and DEL as `json.dumps` does:
// `json.dumps({"n": 1e-05, "s": "\x7f"})` == '{"n": 1e-05, "s": "\u007f"}'.
test("frame codec writes python's float repr and escapes DEL", () => {
  assert.equal(encodeFrame({ n: 0.00001, s: "\x7f" }).data, '{"n": 1e-05, "s": "\\u007f"}');
});

// ─── fake socket harness ────────────────────────────────────────────────────

class FakeSocket implements WebSocketLike {
  binaryType = "blob";
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  readonly sent: Envelope[] = [];
  closed = false;
  readonly url: string;

  constructor(url: string) {
    this.url = url;
    // open on a later tick — after Conn assigns onopen and awaits ready
    setTimeout(() => this.onopen?.(undefined), 0);
  }
  send(data: string | ArrayBuffer): void {
    this.sent.push(decodeFrame(data));
  }
  close(): void {
    this.closed = true;
  }
  /** server→client push */
  deliver(env: Envelope): void {
    const f = encodeFrame(env);
    this.onmessage?.({ data: f.data });
  }
  /** simulate the socket dropping */
  drop(): void {
    this.onclose?.(undefined);
  }
  lastCall(): Envelope | undefined {
    return [...this.sent].reverse().find((f) => f["type"] === "call");
  }
}

function fakeFactory() {
  const sockets: FakeSocket[] = [];
  return {
    factory: (url: string) => {
      const s = new FakeSocket(url);
      sockets.push(s);
      return s;
    },
    forEndpoint(ep: string): FakeSocket[] {
      return sockets.filter((s) => s.url.endsWith(`/${ep}/ws`));
    },
  };
}

async function until(fn: () => boolean, ms = 1000): Promise<void> {
  const start = Date.now();
  while (!fn()) {
    if (Date.now() - start > ms) throw new Error("until: timeout");
    await new Promise((r) => setTimeout(r, 1));
  }
}

function kernelWithRoot(): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  return k;
}

// ─── WsBridge ───────────────────────────────────────────────────────────────

test("forward sends a call on the control socket and resolves the reply by id", async () => {
  const fk = fakeFactory();
  const k = kernelWithRoot();
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: "kernel_state",
    makeSocket: fk.factory,
  });

  const p = bridge.forward("file_x", { type: "list", path: "." });
  await until(() => (fk.forEndpoint("kernel_state")[0]?.lastCall() ?? undefined) !== undefined);
  const sock = fk.forEndpoint("kernel_state")[0]!;
  const call = sock.lastCall()!;
  assert.equal(call["target"], "file_x");
  assert.deepEqual(call["payload"], { type: "list", path: "." });

  sock.deliver({ type: "reply", id: call["id"], data: { entries: ["a", "b"] } });
  assert.deepEqual(await p, { entries: ["a", "b"] });
  bridge.close();
});

test("an error frame rejects the matching call", async () => {
  const fk = fakeFactory();
  const k = kernelWithRoot();
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: "kernel_state",
    makeSocket: fk.factory,
  });
  const p = bridge.forward("kernel_state", { type: "nope" });
  await until(() => (fk.forEndpoint("kernel_state")[0]?.lastCall() ?? undefined) !== undefined);
  const sock = fk.forEndpoint("kernel_state")[0]!;
  sock.deliver({ type: "error", id: sock.lastCall()!["id"], error: "boom" });
  await assert.rejects(p, /boom/);
  bridge.close();
});

test("watchRemote dials /<src>/ws; its events route to emit(src) only", async () => {
  const fk = fakeFactory();
  const k = kernelWithRoot();
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: "kernel_state",
    makeSocket: fk.factory,
  });

  const pty1: Envelope[] = [];
  const pty2: Envelope[] = [];
  k.onInbox("pty1", (pl) => pty1.push(pl));
  k.onInbox("pty2", (pl) => pty2.push(pl));
  // a local view-agent watches each backend
  k.register(new Agent({ id: "view1", parentId: "kernel_state" }));
  k.register(new Agent({ id: "view2", parentId: "kernel_state" }));
  k.onInbox("view1", (pl) => pty1.push(pl));
  k.onInbox("view2", (pl) => pty2.push(pl));
  k.watch("pty1", "view1"); // remote → bridge.watchRemote("pty1")
  k.watch("pty2", "view2");

  await until(() => fk.forEndpoint("pty1").length > 0 && fk.forEndpoint("pty2").length > 0);
  fk.forEndpoint("pty1")[0]!.deliver({
    type: "event",
    payload: { type: "output", text: "hello" },
  });

  await until(() => pty1.length > 0);
  // pty1's stream reached pty1 watchers; pty2 untouched (per-endpoint isolation)
  assert.ok(pty1.some((p) => p["text"] === "hello"));
  assert.equal(pty2.length, 0);
  bridge.close();
});

test("a pending call rejects when its socket drops", async () => {
  const fk = fakeFactory();
  const k = kernelWithRoot();
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: "kernel_state",
    makeSocket: fk.factory,
    maxReconnectDelay: 5,
  });
  const p = bridge.forward("kernel_state", { type: "slow" });
  await until(() => (fk.forEndpoint("kernel_state")[0]?.lastCall() ?? undefined) !== undefined);
  fk.forEndpoint("kernel_state")[0]!.drop();
  await assert.rejects(p, /disconnected/);
  bridge.close();
});

// ─── #38: the wire id IS the execution id ───────────────────────────────────

test("two bridges mint DISJOINT correlator ids — a second tab must not collide", async () => {
  // MEASURED 2026-08-27 against the real python kernel. The host puts a frame's
  // `id` straight into the batch item, and an id is BOUND to its first target:
  //
  //   tab A  id "1" → alpha   completed
  //   tab B  id "1" → beta    FAILED  bad_arg
  //     "send: id '1' already names an execution on 'alpha', not 'beta'"
  //
  // Aimed at the same target it was worse than refused — it silently JOINED
  // tab A's execution and returned tab A's result. A per-socket counter
  // starting at 1 in every tab cannot be a global execution name.
  const idsFrom = async (): Promise<string> => {
    const fk = fakeFactory();
    const k = kernelWithRoot();
    const bridge = new WsBridge(k, {
      origin: "ws://host",
      controlEndpoint: "kernel_state",
      makeSocket: fk.factory,
    });
    bridge.forward("agent_x", { type: "go" }).catch(() => {}); // close() rejects it
    await until(() => (fk.forEndpoint("kernel_state")[0]?.lastCall() ?? undefined) !== undefined);
    const id = String(fk.forEndpoint("kernel_state")[0]!.lastCall()!["id"]);
    bridge.close();
    return id;
  };

  const a = await idsFrom();
  const b = await idsFrom();
  assert.notEqual(a, b, "two connections minted the SAME first correlator");

  // A STRING, `{prefix}-{n}`: on the host a frame id is the execution's NAME
  // (message ids are the kernel's ints, which a bridge never mints).
  for (const id of [a, b]) {
    assert.match(id, /^[0-9a-z]+-\d+$/, `unexpected correlator shape: ${id}`);
  }
});

test("correlators stay unique across many calls on one socket", async () => {
  const fk = fakeFactory();
  const k = kernelWithRoot();
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: "kernel_state",
    makeSocket: fk.factory,
  });
  const seen = new Set<string>();
  for (let i = 0; i < 5; i++) {
    bridge.forward("agent_x", { type: "go", i }).catch(() => {}); // close() rejects it
    await until(() => {
      const c = fk.forEndpoint("kernel_state")[0]?.lastCall();
      return c !== undefined && !seen.has(String(c["id"]));
    });
    seen.add(String(fk.forEndpoint("kernel_state")[0]!.lastCall()!["id"]));
  }
  assert.equal(seen.size, 5, "a correlator was reused on one socket");
  bridge.close();
});

test("a call frame carries the QUALIFIED caller once the kernel has a guid", async () => {
  // `<this kernel's guid>|<the agent that asked>` (`kernel` for the operator):
  // the far side keys what the call writes by it. No guid yet ⇒ no field.
  const fk = fakeFactory();
  const k = kernelWithRoot();
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: "kernel_state",
    makeSocket: fk.factory,
  });
  void k.send([{ target: "file_x", payload: { type: "list" } }]).catch(() => undefined);
  await until(() => fk.forEndpoint("kernel_state")[0]?.lastCall() !== undefined);
  const sock = fk.forEndpoint("kernel_state")[0]!;
  assert.equal(sock.lastCall()!["caller"], undefined, "no guid, no caller");

  k.updateMeta(k.rootId!, { guid: "g-local" });
  void k.send([{ target: "file_x", payload: { type: "list", n: 2 } }]).catch(() => undefined);
  await until(() => (sock.lastCall()!["payload"] as { n?: number }).n === 2);
  assert.equal(sock.lastCall()!["caller"], "g-local|kernel");

  // an AGENT's host-bound call: it passes its own egress gate first, so the
  // sender is a real agent holding the edge
  k.register(new Agent({ id: "frontend", parentId: "kernel_state", meta: { can_call: { file_x: ["list"] } } }));
  void k.scopedFor("frontend", null)
    .send([{ target: "file_x", payload: { type: "list", n: 3 } }])
    .catch(() => undefined);
  await until(() => (sock.lastCall()!["payload"] as { n?: number }).n === 3);
  assert.equal(sock.lastCall()!["caller"], "g-local|frontend");
  bridge.close();
});

test("the state stream has no body option — row events are not on it", async () => {
  const fk = fakeFactory();
  const k = kernelWithRoot();
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: "kernel_state",
    makeSocket: fk.factory,
  });
  bridge.forward("file_x", { type: "list", path: "." }).catch(() => undefined);
  await until(() => (fk.forEndpoint("kernel_state")[0] ?? undefined) !== undefined);
  const sock = fk.forEndpoint("kernel_state")[0]!;
  const off = bridge.subscribeState(() => undefined);
  await until(() => sock.sent.some((f) => f["type"] === "state_subscribe"));
  const sub = sock.sent.find((f) => f["type"] === "state_subscribe")!;
  assert.deepEqual(sub, { type: "state_subscribe" });
  off();
  await until(() => sock.sent.some((f) => f["type"] === "state_unsubscribe"));
  bridge.close();
});

// ─── host-bound sends pass the SENDER's half of the gate here ───────────────
// python `authorize_call` (egress, `enforce_privileged=True`) decides whether an
// agent may SEND; the target's `callable_by` lives on the host and is the host's
// to apply. Refused locally means NOTHING reaches the transport.

function hostKernel(): { k: Kernel; bridge: WsBridge; fk: ReturnType<typeof fakeFactory>; frames: () => Envelope[] } {
  const fk = fakeFactory();
  const k = kernelWithRoot();
  const bridge = new WsBridge(k, { origin: "ws://host", controlEndpoint: "kernel_state", makeSocket: fk.factory });
  const frames = (): Envelope[] => fk.forEndpoint("kernel_state").flatMap((s) => s.sent.filter((f) => f["type"] === "call"));
  return { k, bridge, fk, frames };
}

test("an agent WITHOUT can_call on a host agent is refused locally — no frame reaches the bridge", async () => {
  const { k, bridge, frames } = hostKernel();
  k.register(new Agent({ id: "view", parentId: "kernel_state" }));
  // `$parent: "*"` (the baseline) names the LOCAL root, never a host agent
  const [out] = await k.scopedFor("view", null).send([{ target: "pty1", payload: { type: "write", data: "ls" } }]);
  assert.equal(out!.status, "failed");
  assert.equal((out!.result as Payload)["reason"], "unauthorized");
  assert.equal((out!.result as Payload)["side"], "egress");
  assert.equal(out!.pid, null, "refused before a message id was minted");
  await new Promise((r) => setTimeout(r, 10));
  assert.deepEqual(frames(), [], "the transport saw no frame");
  bridge.close();
});

test("WITH the grant the call is forwarded, carrying the agent as its caller", async () => {
  const { k, bridge, fk } = hostKernel();
  k.updateMeta(k.rootId!, { guid: "g1" });
  k.register(new Agent({ id: "view", parentId: "kernel_state", meta: { can_call: { pty1: ["write"] } } }));
  const p = k.scopedFor("view", null).send([{ target: "pty1", payload: { type: "write", data: "ls" } }]);
  await until(() => fk.forEndpoint("kernel_state")[0]?.lastCall() !== undefined);
  const sock = fk.forEndpoint("kernel_state")[0]!;
  const call = sock.lastCall()!;
  assert.equal(call["target"], "pty1");
  assert.equal(call["caller"], "g1|view");
  sock.deliver({ type: "reply", id: call["id"], data: { ok: true } });
  const [out] = await p;
  assert.equal(out!.status, "completed");
  // the grant is per VERB: another verb to the same host agent is refused here
  const [other] = await k.scopedFor("view", null).send([{ target: "pty1", payload: { type: "resize" } }]);
  assert.equal((other!.result as Payload)["side"], "egress");
  bridge.close();
});

test("a privileged verb to a host agent crosses only on a NAMED grant (a12)", async () => {
  const { k, bridge, frames } = hostKernel();
  // no grant naming host_root: refused locally, nothing leaves the kernel
  k.register(new Agent({ id: "view", parentId: "kernel_state" }));
  for (const verb of ["create_agent", "update_agent", "delete_agent", "state_subscribe", "allow", "disallow"]) {
    const [out] = await k.scopedFor("view", null).send([{ target: "host_root", payload: { type: verb } }]);
    assert.equal((out!.result as Payload)["side"], "egress", verb);
  }
  await new Promise((r) => setTimeout(r, 10));
  assert.deepEqual(frames(), [], "no privileged call left the kernel");
  // a NAMED id's `*` is the operator's own statement — it covers the verb
  k.updateMeta("view", { can_call: { host_root: "*" } });
  void k.scopedFor("view", null).send([{ target: "host_root", payload: { type: "delete_agent", id: "x" } }]).catch(() => undefined);
  await until(() => frames().length === 1);
  assert.equal(frames()[0]!["target"], "host_root");
  bridge.close();
});

test("the operator forwards ungated; callHost and forwardBinary are host-bound sends too", async () => {
  const { k, bridge, frames } = hostKernel();
  void k.send([{ target: "pty1", payload: { type: "write" } }]).catch(() => undefined);
  void k.callHost("kernel", { type: "reflect" }).catch(() => undefined);
  await until(() => frames().length === 2);
  k.register(new Agent({ id: "view", parentId: "kernel_state" }));
  const asView = k.scopedFor("view", null);
  const viaCallHost = await asView.callHost("kernel", { type: "create_agent", handler_module: "x.tools" });
  assert.equal((viaCallHost as Payload)["side"], "egress");
  const viaBinary = await asView.forwardBinary("pty1", { type: "paste_image", image: new Uint8Array([1]) });
  assert.equal((viaBinary as Payload)["side"], "egress");
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(frames().length, 2, "the refused agent calls never reached the bridge");
  bridge.close();
});

// ─── the PEER role: the host calling THIS kernel's agents ───────────────────
// (cross-kernel addressing, owner 2026-09-30: the host addresses a page agent as
// `<page guid>|<agent>`; the page kernel answers `call` frames on the conn that
// said `hello`, gated by the target's `callable_by` against the qualified caller)

function pageKernel(guid: string | null): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state", meta: guid === null ? {} : { guid } }));
  k.register(
    new Agent({
      id: "canvas",
      parentId: "kernel_state",
      meta: { callable_by: { "h0|*": ["around"] } },
      handler: (_id, payload) =>
        payload["type"] === "around" ? { cells: ["a", "b"], radius: payload["radius"] ?? null } : { error: "no" },
    }),
  );
  k.register(new Agent({ id: "secret", parentId: "kernel_state", handler: () => ({ ok: true }) }));
  return k;
}

function repliesTo(sock: FakeSocket, id: string): Envelope | undefined {
  return sock.sent.find((f) => f["type"] === "reply" && f["id"] === id);
}

test("the control socket says hello with the kernel's guid — on open, and when the guid first appears", async () => {
  const fk = fakeFactory();
  const k = pageKernel(null);
  const bridge = new WsBridge(k, { origin: "ws://host", controlEndpoint: "kernel_state", makeSocket: fk.factory });
  bridge.announce();
  await until(() => fk.forEndpoint("kernel_state").length === 1);
  const sock = fk.forEndpoint("kernel_state")[0]!;
  await new Promise((r) => setTimeout(r, 5)); // opened, but no guid yet → no hello
  assert.equal(sock.sent.filter((f) => f["type"] === "hello").length, 0);
  k.updateMeta("kernel_state", { guid: "p1" }); // what the root's boot does
  await until(() => sock.sent.some((f) => f["type"] === "hello"));
  assert.deepEqual(sock.sent.find((f) => f["type"] === "hello"), { type: "hello", guid: "p1" });
  bridge.close();
});

test("a host call is answered by the page kernel, gated by the target's callable_by", async () => {
  const fk = fakeFactory();
  const k = pageKernel("p1");
  const bridge = new WsBridge(k, { origin: "ws://host", controlEndpoint: "kernel_state", makeSocket: fk.factory });
  bridge.announce();
  await until(() => fk.forEndpoint("kernel_state")[0]?.sent.some((f) => f["type"] === "hello") ?? false);
  const sock = fk.forEndpoint("kernel_state")[0]!;

  // accepted: `h0|*` on canvas's callable_by names every agent of kernel h0
  sock.deliver({ type: "call", id: "h1", target: "canvas", payload: { type: "around", radius: 3 }, caller: "h0|cell_7" });
  await until(() => repliesTo(sock, "h1") !== undefined);
  assert.deepEqual(repliesTo(sock, "h1")!["data"], { cells: ["a", "b"], radius: 3 });

  // a verb canvas does not accept from h0, and an agent that accepts nobody there
  sock.deliver({ type: "call", id: "h2", target: "canvas", payload: { type: "clear" }, caller: "h0|cell_7" });
  sock.deliver({ type: "call", id: "h3", target: "secret", payload: { type: "read" }, caller: "h0|cell_7" });
  await until(() => repliesTo(sock, "h2") !== undefined && repliesTo(sock, "h3") !== undefined);
  for (const id of ["h2", "h3"]) {
    const d = repliesTo(sock, id)!["data"] as Payload;
    assert.equal(d["reason"], "unauthorized", id);
    assert.equal(d["side"], "ingress", id);
  }

  // only THIS kernel's agents are reachable — never through to the host again
  sock.deliver({ type: "call", id: "h4", target: "file_x", payload: { type: "list" }, caller: "h0|cell_7" });
  await until(() => repliesTo(sock, "h4") !== undefined);
  assert.equal((repliesTo(sock, "h4")!["data"] as Payload)["reason"], "unknown_agent");

  // a caller claiming THIS kernel's own guid would borrow a local agent's grants
  sock.deliver({ type: "call", id: "h5", target: "canvas", payload: { type: "around" }, caller: "p1|canvas" });
  await until(() => repliesTo(sock, "h5") !== undefined);
  assert.equal((repliesTo(sock, "h5")!["data"] as Payload)["reason"], "bad_arg");
  bridge.close();
});

test("a binary path through an own key named constructor / __proto__ round-trips, as in python", () => {
  for (const key of ["constructor", "__proto__", "prototype"]) {
    const payload: Record<string, unknown> = {};
    Object.defineProperty(payload, key, { value: new Uint8Array([1, 2]), enumerable: true, writable: true, configurable: true });
    const enc = encodeFrame({ type: "x", payload } as unknown as Envelope);
    assert.equal(enc.binary, true, key);
    const dec = decodeFrame(enc.data) as Record<string, Record<string, unknown>>;
    assert.ok(Object.hasOwn(dec["payload"]!, key), key);
    assert.deepEqual([...new Uint8Array(dec["payload"]![key] as ArrayBuffer)], [1, 2], key);
    // the prototype of the decoded object is untouched
    assert.equal(Object.getPrototypeOf(dec["payload"]), Object.prototype, key);
  }
});
