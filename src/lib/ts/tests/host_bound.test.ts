// The HOST-BOUND surface of the kernel and the WS bridge under it: what goes out
// as which frame on which socket, what comes back, and what happens with no
// bridge at all — `Kernel.forwardBinary` / `callHost` / `emitRemote` /
// `onLifecycle` / `subscribeState`, and the bridge's per-endpoint sockets:
// connect/disconnect, reconnect, malformed frames, a socket that cannot send,
// the state stream, unwatch, and the default socket factory.
//
// The socket is the only stand-in (a WebSocket-shaped object driven by the test);
// the kernel and the bridge are the real ones.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { WsBridge } from "../src/transport/bridge.ts";
import { Host } from "../src/bundles/host.ts";
import type { WebSocketLike } from "../src/transport/bridge.ts";
import { encodeFrame, decodeFrame } from "../src/transport/frame.ts";
import type { Envelope } from "../src/transport/frame.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import { as } from "./kit.ts";

const ROOT = "kernel_state";

class FakeSocket implements WebSocketLike {
  binaryType = "blob";
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  /** decoded frames, and whether each went out as a binary frame */
  readonly sent: Envelope[] = [];
  readonly binary: boolean[] = [];
  closed = false;
  /** throw from `send` for frames of these types */
  throwOn = new Set<string>();
  readonly url: string;

  constructor(url: string) {
    this.url = url;
    setTimeout(() => this.onopen?.(undefined), 0);
  }
  send(data: string | ArrayBuffer): void {
    const env = decodeFrame(data);
    if (this.throwOn.has(String(env["type"]))) throw new Error(`socket refused a ${String(env["type"])} frame`);
    this.sent.push(env);
    this.binary.push(typeof data !== "string");
  }
  close(): void {
    this.closed = true;
  }
  deliver(env: Envelope): void {
    this.onmessage?.({ data: encodeFrame(env).data });
  }
  deliverRaw(data: string): void {
    this.onmessage?.({ data });
  }
  drop(): void {
    this.onclose?.(undefined);
  }
  frames(type: string): Envelope[] {
    return this.sent.filter((f) => f["type"] === type);
  }
}

function sockets() {
  const all: FakeSocket[] = [];
  return {
    all,
    factory: (url: string): FakeSocket => {
      const s = new FakeSocket(url);
      all.push(s);
      return s;
    },
    of(ep: string): FakeSocket[] {
      return all.filter((s) => s.url.endsWith(`/${ep}/ws`));
    },
  };
}

async function until(fn: () => boolean, ms = 3000): Promise<void> {
  const start = Date.now();
  while (!fn()) {
    if (Date.now() - start > ms) throw new Error("until: timeout");
    await new Promise((r) => setTimeout(r, 1));
  }
}

function wired(guid: string | null = "g1") {
  const fk = sockets();
  const k = new Kernel();
  k.setRoot(new Agent({ id: ROOT, meta: guid === null ? {} : { guid } }));
  const bridge = new WsBridge(k, { origin: "ws://host", controlEndpoint: ROOT, makeSocket: fk.factory, maxReconnectDelay: 5 });
  return { k, bridge, fk };
}

async function warns(fn: () => Promise<unknown>): Promise<string[]> {
  const lines: string[] = [];
  const orig = console.warn;
  console.warn = (m: string) => lines.push(String(m));
  try {
    await fn();
  } finally {
    console.warn = orig;
  }
  return lines;
}

// ── the kernel's host-bound calls ──────────────────────────────────────

test("forwardBinary goes out as ONE binary call frame and resolves its reply; `kernel` names the LOCAL root", async () => {
  const { k, bridge, fk } = wired();
  const p = k.forwardBinary("pty1", { type: "paste_image", image: new Uint8Array([1, 2, 3]), name: "a.png" });
  await until(() => (fk.of(ROOT)[0]?.frames("call").length ?? 0) === 1);
  const sock = fk.of(ROOT)[0]!;
  const call = sock.frames("call")[0]!;
  assert.equal(sock.binary[sock.sent.indexOf(call)], true, "a payload carrying bytes rides a binary frame");
  assert.deepEqual(Object.keys(call), ["type", "target", "payload", "id"]);
  assert.equal(call["target"], "pty1");
  const payload = call["payload"] as Record<string, unknown>;
  assert.equal(payload["type"], "paste_image");
  assert.equal(payload["name"], "a.png");
  assert.deepEqual([...(payload["image"] as Uint8Array)], [1, 2, 3]);
  sock.deliver({ type: "reply", id: call["id"], data: { pasted: true } });
  assert.deepEqual(await p, { pasted: true });
  // `kernel` resolves to THIS kernel's root for forwardBinary, stays literal for callHost
  void k.forwardBinary("kernel", { type: "paste_image", image: new Uint8Array([4]) }).catch(() => undefined);
  void k.callHost("kernel", { type: "reflect" }).catch(() => undefined);
  await until(() => sock.frames("call").length === 3);
  assert.deepEqual(sock.frames("call").slice(1).map((f) => f["target"]), [ROOT, "kernel"]);
  assert.deepEqual(sock.binary.slice(-2), [true, false], "a payload without bytes rides a text frame");
  bridge.close();
});

test("with no bridge: forwardBinary and callHost answer an error naming the target; the rest are no-ops", async () => {
  const k = new Kernel();
  k.setRoot(new Agent({ id: ROOT }));
  assert.deepEqual(await k.forwardBinary("pty1", { type: "paste_image", image: new Uint8Array([1]) }), {
    error: "no bridge to forward to 'pty1'",
  });
  assert.deepEqual(await k.callHost("kernel", { type: "reflect" }), { error: "no bridge to reach host 'kernel'" });
  // the missing bridge is reported before the sender's grant is looked at
  k.register(new Agent({ id: "view", parentId: ROOT }));
  assert.deepEqual(await as(k, "view", (v) => v.forwardBinary("pty1", { type: "paste_image" })), { error: "no bridge to forward to 'pty1'" });
  assert.deepEqual(await as(k, "view", (v) => v.callHost("h", { type: "x" })), { error: "no bridge to reach host 'h'" });
  assert.doesNotThrow(() => k.emitRemote("pty1", { type: "reload_html" }));
  const offL = k.onLifecycle("pty1", () => assert.fail("no bridge, no lifecycle"));
  const offS = k.subscribeState(() => assert.fail("no bridge, no state"));
  assert.equal(typeof offL, "function");
  assert.equal(typeof offS, "function");
  assert.equal(offL(), undefined);
  assert.equal(offS(), undefined);
});

test("emitRemote: an emit frame on the target's own socket when watched, else the control one; never buffered", async () => {
  const { k, bridge, fk } = wired();
  k.register(new Agent({ id: "view", parentId: ROOT }));
  // the control socket is not open yet: the emit is dropped, not queued
  k.emitRemote("pty1", { type: "reload_html", n: 0 });
  await until(() => fk.of(ROOT).length === 1);
  const control = fk.of(ROOT)[0]!;
  await new Promise((r) => setTimeout(r, 5));
  assert.deepEqual(control.frames("emit"), [], "an emit made while not connected was buffered");
  k.emitRemote("pty1", { type: "reload_html", n: 1 });
  k.emitRemote("kernel", { type: "poke" });
  assert.deepEqual(control.frames("emit"), [
    { type: "emit", target: "pty1", payload: { type: "reload_html", n: 1 } },
    { type: "emit", target: ROOT, payload: { type: "poke" } },
  ]);
  // watched: pty1 has its own socket, and calls and emits to it ride that socket
  assert.equal(k.watch("pty1", "view"), null);
  await until(() => fk.of("pty1").length === 1);
  const own = fk.of("pty1")[0]!;
  await new Promise((r) => setTimeout(r, 5));
  k.emitRemote("pty1", { type: "reload_html", n: 2 });
  void k.send([{ target: "pty1", payload: { type: "write", data: "ls" } }]).catch(() => undefined);
  void k.forwardBinary("pty1", { type: "paste_image", image: new Uint8Array([7]) }).catch(() => undefined);
  await until(() => own.frames("call").length === 2);
  assert.deepEqual(own.frames("emit"), [{ type: "emit", target: "pty1", payload: { type: "reload_html", n: 2 } }]);
  assert.deepEqual(own.frames("call").map((f) => f["target"]), ["pty1", "pty1"]);
  assert.equal(control.frames("emit").length, 2, "nothing more went to the control socket");
  assert.equal(control.frames("call").length, 0);
  bridge.close();
});

test("onLifecycle: connected on open, disconnected on a drop, per listener until it unsubscribes; `kernel` is the local root", async () => {
  const { k, bridge, fk } = wired();
  const a: string[] = [];
  const b: string[] = [];
  const offA = k.onLifecycle("pty1", (s) => a.push(s));
  k.onLifecycle("pty1", (s) => b.push(s));
  await until(() => a.length === 1 && b.length === 1);
  assert.deepEqual([a, b], [["connected"], ["connected"]]);
  offA();
  fk.of("pty1")[0]!.drop();
  assert.deepEqual([a, b], [["connected"], ["connected", "disconnected"]]);
  const r: string[] = [];
  k.onLifecycle("kernel", (s) => r.push(s));
  await until(() => r.length === 1);
  assert.deepEqual(r, ["connected"]);
  assert.deepEqual(fk.all.map((s) => s.url).sort(), [`ws://host/${ROOT}/ws`, "ws://host/pty1/ws"]);
  bridge.close();
});

test("a dropped socket reconnects to its URL, re-announces, resubscribes the state stream, and carries calls made while down", async () => {
  const { k, bridge, fk } = wired();
  const life: string[] = [];
  k.onLifecycle(ROOT, (s) => life.push(s));
  const state: Record<string, unknown>[] = [];
  k.subscribeState((f) => state.push(f));
  await until(() => fk.of(ROOT)[0]?.frames("state_subscribe").length === 1);
  const first = fk.of(ROOT)[0]!;
  assert.deepEqual(first.frames("hello"), [{ type: "hello", guid: "g1" }]);
  const pending = bridge.forward("file_x", { type: "list" });
  await until(() => first.frames("call").length === 1);
  first.drop();
  await assert.rejects(pending, { message: "disconnected" });
  // a call made while down waits for the new socket
  const later = bridge.forward("file_x", { type: "list", n: 2 });
  await until(() => fk.of(ROOT).length === 2 && fk.of(ROOT)[1]!.frames("call").length === 1);
  const second = fk.of(ROOT)[1]!;
  assert.equal(second.url, first.url);
  assert.deepEqual(second.frames("state_subscribe"), [{ type: "state_subscribe" }]);
  assert.deepEqual(second.frames("hello"), [{ type: "hello", guid: "g1" }]);
  const call = second.frames("call")[0]!;
  assert.deepEqual(call["payload"], { type: "list", n: 2 });
  second.deliver({ type: "reply", id: call["id"], data: { entries: [] } });
  assert.deepEqual(await later, { entries: [] });
  second.deliver({ type: "state_event", kind: "added", agent_id: "x" });
  assert.deepEqual(state, [{ type: "state_event", kind: "added", agent_id: "x" }]);
  assert.deepEqual(life, ["connected", "disconnected", "connected"]);
  bridge.close();
});

test("a malformed or non-object frame is skipped loudly and the socket kept; replies by unknown id are ignored", async () => {
  const { bridge, fk } = wired();
  const p = bridge.forward("file_x", { type: "list" });
  await until(() => fk.of(ROOT)[0]?.frames("call").length === 1);
  const sock = fk.of(ROOT)[0]!;
  const lines = await warns(async () => {
    sock.deliverRaw("{bad");
    sock.deliverRaw('{"a": 12345678901234567890}');
    sock.deliverRaw("[1, 2]");
    sock.deliverRaw("null");
    sock.deliver({ type: "reply", id: "nobody-1", data: { stray: true } });
  });
  assert.equal(lines.length, 4, JSON.stringify(lines));
  assert.ok(lines[0]!.startsWith("ws_bridge: skipping malformed frame: bad text frame: "), lines[0]);
  assert.ok(
    lines[1]!.startsWith("ws_bridge: skipping malformed frame: bad text frame: integer 12345678901234567890 is outside int64"),
    lines[1],
  );
  assert.deepEqual(lines.slice(2), ["ws_bridge: skipping non-object frame", "ws_bridge: skipping non-object frame"]);
  assert.equal(sock.closed, false);
  const call = sock.frames("call")[0]!;
  sock.deliver({ type: "reply", id: call["id"], data: { ok: 1 } });
  assert.deepEqual(await p, { ok: 1 });
  // an error frame without text rejects with "error"
  const q = bridge.forward("file_x", { type: "list" });
  await until(() => sock.frames("call").length === 2);
  sock.deliver({ type: "error", id: sock.frames("call")[1]!["id"] });
  await assert.rejects(q, { message: "error" });
  bridge.close();
});

test("a socket that cannot send: the call rejects with its error and is forgotten; a frame that cannot go is dropped", async () => {
  const { k, bridge, fk } = wired();
  await new Promise((r) => setTimeout(r, 0));
  bridge.announce();
  await until(() => fk.of(ROOT)[0]?.frames("hello").length === 1);
  const sock = fk.of(ROOT)[0]!;
  sock.throwOn = new Set(["call", "emit", "state_subscribe"]);
  await assert.rejects(bridge.forward("file_x", { type: "list" }), { message: "socket refused a call frame" });
  assert.doesNotThrow(() => k.emitRemote("pty1", { type: "reload_html" }));
  const off = k.subscribeState(() => undefined);
  assert.deepEqual(sock.sent.map((f) => f["type"]), ["hello"], "nothing that failed to send was recorded as sent");
  off();
  // the failed call left nothing pending: a later reply carrying any id is ignored
  sock.deliver({ type: "reply", id: "x-1", data: null });
  sock.throwOn = new Set();
  const p = bridge.forward("file_x", { type: "list" });
  await until(() => sock.frames("call").length === 1);
  sock.deliver({ type: "reply", id: sock.frames("call")[0]!["id"], data: 5 });
  assert.equal(await p, 5);
  bridge.close();
});

test("the host state stream: one subscribe for many handlers, every handler gets every state frame, the last one out unsubscribes", async () => {
  const { k, bridge, fk } = wired();
  const a: Record<string, unknown>[] = [];
  const b: Record<string, unknown>[] = [];
  const offA = k.subscribeState((f) => a.push(f));
  const offB = bridge.subscribeState((f) => b.push(f));
  await until(() => fk.of(ROOT)[0]?.frames("state_subscribe").length === 1);
  const sock = fk.of(ROOT)[0]!;
  sock.deliver({ type: "state_snapshot", agents: [] });
  sock.deliver({ type: "event", payload: { type: "not_state" } });
  sock.deliver({ type: "state_event", kind: "removed", agent_id: "y" });
  const want = [{ type: "state_snapshot", agents: [] }, { type: "state_event", kind: "removed", agent_id: "y" }];
  assert.deepEqual(a, want);
  assert.deepEqual(b, want);
  offA();
  assert.deepEqual(sock.frames("state_unsubscribe"), [], "a handler remains");
  sock.deliver({ type: "state_event", kind: "added", agent_id: "z" });
  assert.equal(a.length, 2);
  assert.equal(b.length, 3);
  offB();
  assert.deepEqual(sock.frames("state_unsubscribe"), [{ type: "state_unsubscribe" }]);
  sock.deliver({ type: "state_event", kind: "added", agent_id: "w" });
  assert.equal(b.length, 3, "an unsubscribed handler kept receiving");
  // subscribing again opens the stream again
  k.subscribeState(() => undefined);
  assert.equal(sock.frames("state_subscribe").length, 2);
  bridge.close();
});

test("unwatchRemote closes and forgets a watched socket — never the control one; a later call rides the control socket", async () => {
  const { k, bridge, fk } = wired();
  k.register(new Agent({ id: "view", parentId: ROOT }));
  k.watch("pty1", "view");
  bridge.announce();
  await until(() => fk.of("pty1").length === 1 && fk.of(ROOT).length === 1);
  await new Promise((r) => setTimeout(r, 5));
  const own = fk.of("pty1")[0]!;
  const control = fk.of(ROOT)[0]!;
  k.unwatch("pty1", "view");
  assert.equal(own.closed, true);
  bridge.unwatchRemote(ROOT);
  assert.equal(control.closed, false, "the control socket was closed");
  void bridge.forward("pty1", { type: "write" }).catch(() => undefined);
  await until(() => control.frames("call").length === 1);
  assert.equal(control.frames("call")[0]!["target"], "pty1");
  assert.equal(own.frames("call").length, 0);
  assert.equal(fk.of("pty1").length, 1, "no new socket was dialed for the unwatched target");
  bridge.close();
});

test("the default socket factory dials the global WebSocket at <origin>/<endpoint>/ws; with none it fails loud", () => {
  const saved = Object.getOwnPropertyDescriptor(globalThis, "WebSocket")!;
  const made: FakeSocket[] = [];
  try {
    Object.defineProperty(globalThis, "WebSocket", {
      value: class extends FakeSocket {
        constructor(url: string) {
          super(url);
          made.push(this);
        }
      },
      configurable: true,
      writable: true,
    });
    const k = new Kernel();
    k.setRoot(new Agent({ id: ROOT }));
    const bridge = new WsBridge(k, { origin: "ws://h//", controlEndpoint: ROOT });
    bridge.announce();
    assert.deepEqual(made.map((s) => s.url), [`ws://h/${ROOT}/ws`]);
    assert.equal(made[0]!.binaryType, "arraybuffer");
    bridge.close();
    Object.defineProperty(globalThis, "WebSocket", { value: undefined, configurable: true, writable: true });
    const k2 = new Kernel();
    k2.setRoot(new Agent({ id: ROOT }));
    const b2 = new WsBridge(k2, { origin: "ws://h", controlEndpoint: ROOT });
    assert.throws(() => b2.announce(), { message: "no global WebSocket available; pass opts.makeSocket" });
  } finally {
    Object.defineProperty(globalThis, "WebSocket", saved);
  }
});

test("a host call answered by this kernel: the reply carries the outcome's result, a refusal included", async () => {
  const { k, bridge, fk } = wired("p1");
  k.registerBundle("echo.ts", (_id, payload) =>
    payload["type"] === "echo" ? { echoed: payload["v"] ?? null } : { error: "echo: no", reason: "unhandled" });
  k.register(new Agent({ id: "canvas", parentId: ROOT, handlerModule: "echo.ts" }));
  bridge.announce();
  await until(() => fk.of(ROOT)[0]?.frames("hello").length === 1);
  const sock = fk.of(ROOT)[0]!;
  const replyTo = (id: string): Envelope | undefined => sock.frames("reply").find((f) => f["id"] === id);
  sock.deliver({ type: "call", id: "h1", target: "canvas", payload: { type: "echo", v: 3 } });
  sock.deliver({ type: "call", id: "h2", target: "canvas", payload: [1] });
  sock.deliver({ type: "call", payload: { type: "echo" } });
  sock.deliver({ type: "call", id: "h4", target: "canvas", payload: { type: "nope" } });
  await until(() => replyTo("h1") !== undefined && replyTo("h2") !== undefined && replyTo("") !== undefined && replyTo("h4") !== undefined);
  assert.deepEqual(replyTo("h1"), { type: "reply", id: "h1", data: { echoed: 3 } });
  assert.deepEqual(replyTo("h2")!["data"], { error: "call: needs a target id and an object payload", reason: "bad_arg", arg: "payload" });
  assert.deepEqual(
    replyTo("")!["data"],
    { error: "call: needs a target id and an object payload", reason: "bad_arg", arg: "target" },
    "a call frame without an id is answered under id ''",
  );
  assert.deepEqual(replyTo("h4")!["data"], { error: "echo: no", reason: "unhandled" });
  bridge.close();
});

test("a binary frame from the host is decoded before it reaches the inbox: events carry their bytes", async () => {
  const { k, bridge, fk } = wired();
  k.register(new Agent({ id: "view", parentId: ROOT }));
  const got: Payload[] = [];
  k.onInbox("view", (p) => got.push(p));
  k.watch("pty1", "view");
  await until(() => fk.of("pty1").length === 1);
  await new Promise((r) => setTimeout(r, 5));
  fk.of("pty1")[0]!.deliver({ type: "event", payload: { type: "chunk", bytes: new Uint8Array([5, 6]) } });
  fk.of("pty1")[0]!.deliver({ type: "event", payload: "not a dict" as unknown as Json });
  assert.equal(got.length, 1, "a non-object event payload is not delivered");
  assert.equal(got[0]!["type"], "chunk");
  assert.deepEqual([...(got[0]!["bytes"] as unknown as Uint8Array)], [5, 6]);
  bridge.close();
});

test("an error event alone changes nothing (the close that follows does); a socket that throws on close is still closed", async () => {
  const { bridge, fk } = wired();
  const life: string[] = [];
  bridge.onLifecycle(ROOT, (s) => life.push(s));
  const p = bridge.forward("file_x", { type: "list" });
  await until(() => fk.of(ROOT)[0]?.frames("call").length === 1);
  const sock = fk.of(ROOT)[0]!;
  sock.onerror?.(new Error("network blip"));
  assert.deepEqual(life, ["connected"]);
  sock.deliver({ type: "reply", id: sock.frames("call")[0]!["id"], data: { ok: true } });
  assert.deepEqual(await p, { ok: true }, "the error event rejected the pending call");
  const q = bridge.forward("file_x", { type: "list" });
  await until(() => sock.frames("call").length === 2);
  sock.close = () => {
    throw new Error("already closing");
  };
  assert.doesNotThrow(() => bridge.close());
  await assert.rejects(q, { message: "closed" });
});

test("Host: the view's facade — call, callHost, emit, callBinary, on/unwatch, onLifecycle, subscribeState — over the same frames", async () => {
  const { k, bridge, fk } = wired();
  k.registerBundle("local.ts", (_id, payload) => ({ local: payload["type"] as Json }));
  k.register(new Agent({ id: "view", parentId: ROOT, handlerModule: "local.ts" }));
  const host = new Host(k, "view");
  // a local target answers here; anything else is forwarded, its result unwrapped
  assert.deepEqual(await host.call("view", { type: "ping" }), { local: "ping" });
  const life: string[] = [];
  host.onLifecycle("pty1", (s) => life.push(s));
  const state: Record<string, unknown>[] = [];
  host.subscribeState((f) => state.push(f));
  const outputs: Payload[] = [];
  const off = host.on("pty1", "output", (p) => outputs.push(p));
  await until(() => life.length === 1 && (fk.of(ROOT)[0]?.frames("state_subscribe").length ?? 0) === 1);
  const pty = fk.of("pty1")[0]!;
  const control = fk.of(ROOT)[0]!;
  const forwarded = host.call("pty1", { type: "write", data: "ls" });
  const literal = host.callHost("kernel", { type: "reflect" });
  const binary = host.callBinary("pty1", { type: "paste_image", image: new Uint8Array([1]) });
  host.emit("pty1", { type: "reload_html" });
  await until(() => pty.frames("call").length === 2 && control.frames("call").length === 1);
  assert.deepEqual(pty.frames("call").map((f) => (f["payload"] as Payload)["type"]), ["write", "paste_image"]);
  assert.equal(control.frames("call")[0]!["target"], "kernel");
  assert.deepEqual(pty.frames("emit"), [{ type: "emit", target: "pty1", payload: { type: "reload_html" } }]);
  for (const f of pty.frames("call")) pty.deliver({ type: "reply", id: f["id"], data: { done: (f["payload"] as Payload)["type"] as Json } });
  control.deliver({ type: "reply", id: control.frames("call")[0]!["id"], data: { id: "host_root" } });
  assert.deepEqual(await forwarded, { done: "write" });
  assert.deepEqual(await binary, { done: "paste_image" });
  assert.deepEqual(await literal, { id: "host_root" });
  // `on` hands over only its event type
  pty.deliver({ type: "event", payload: { type: "output", text: "a" } });
  pty.deliver({ type: "event", payload: { type: "exit", code: 0 } });
  assert.deepEqual(outputs, [{ type: "output", text: "a" }]);
  // dropping the listener leaves the watch (and the socket) open
  off();
  pty.deliver({ type: "event", payload: { type: "output", text: "b" } });
  assert.equal(outputs.length, 1);
  assert.equal(pty.closed, false);
  control.deliver({ type: "state_event", kind: "added", agent_id: "x" });
  assert.deepEqual(state, [{ type: "state_event", kind: "added", agent_id: "x" }]);
  // unwatch closes the socket once nobody watches
  host.unwatch("pty1");
  assert.equal(pty.closed, true);
  bridge.close();
});
