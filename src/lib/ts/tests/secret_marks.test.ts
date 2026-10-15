// SECRET MARKS (python `kernel/_secret.py`): an item's `secret` paths are shown
// as `[secret]` in the call row and the inbox mirrors, the handler gets the
// payload as sent and reads the marks as `currentSecret`, and a host-bound call
// carries them on its `call` frame — both directions of the WS bridge.
//
// Also the egress gate on `emitRemote`: an agent's host-bound emit is checked
// for verb `emit` like any host-bound call.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Bridge } from "../src/kernel/kernel.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import { WsBridge } from "../src/transport/bridge.ts";
import type { WebSocketLike } from "../src/transport/bridge.ts";
import { decodeFrame, encodeFrame } from "../src/transport/frame.ts";
import type { Envelope } from "../src/transport/frame.ts";
import { SHOWN, shown } from "../src/kernel/secret.ts";
import { as } from "./kit.ts";

const ROOT = "kernel_state";

function withRoot(): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: ROOT }));
  return k;
}

function fakeBridge(log: { forwards: [string, Payload, readonly string[] | undefined][]; emits: [string, Payload][] }): Bridge {
  return {
    forward: (t, p, s) => {
      log.forwards.push([t, p, s]);
      return Promise.resolve({ ok: true });
    },
    forwardBinary: () => Promise.resolve(null),
    emitRemote: (t, p) => log.emits.push([t, p]),
    watchRemote: () => {},
    unwatchRemote: () => {},
    onLifecycle: () => () => {},
    subscribeState: () => () => {},
  };
}

test("shown: each marked value replaced, the input untouched, a path naming nothing skipped", () => {
  const p: Payload = { type: "set", fields: { api_key: "k", user: "u" }, top: "t" };
  const out = shown(p, ["fields.api_key", "top", "fields.nope", "missing.deep"]);
  assert.deepEqual(out, { type: "set", fields: { api_key: SHOWN, user: "u" }, top: SHOWN });
  assert.deepEqual(p, { type: "set", fields: { api_key: "k", user: "u" }, top: "t" });
  assert.equal(shown(p, []), p, "no marks: the payload itself");
});

test("the handler gets the payload as sent; the call row and the inbox mirrors show it; the row keeps the marks", async () => {
  const k = withRoot();
  let got: Payload | null = null;
  let marks: readonly string[] = [];
  k.registerBundle("keep.js", (_id, payload, view) => {
    got = payload;
    marks = view.currentSecret;
    return { ok: true };
  });
  k.register(new Agent({ id: "store", parentId: ROOT, handlerModule: "keep.js" }));
  const own: Payload[] = [];
  const mirrored: Payload[] = [];
  k.onInbox("store", (p) => own.push(p));
  k.onInbox("w", (p) => mirrored.push(p));
  k.watch("store", "w");
  const [out] = await k.send([
    { target: "store", payload: { type: "set", fields: { api_key: "sk-1" } }, secret: ["fields.api_key"] },
  ]);
  assert.equal(out!.status, "completed");
  assert.deepEqual(got, { type: "set", fields: { api_key: "sk-1" } });
  assert.deepEqual(marks, ["fields.api_key"]);
  const masked = { type: "set", fields: { api_key: SHOWN } };
  assert.deepEqual(own, [masked]);
  assert.deepEqual(mirrored, [masked]);
  const call = [...k.store.rows.values()].find((r) => r.kind === "call")!;
  assert.deepEqual(call["payload"], masked);
  assert.deepEqual(call["secret"], ["fields.api_key"]);
});

test("an unmarked send has no `secret` on its row, and a nested send does not inherit the marks", async () => {
  const k = withRoot();
  let inner: readonly string[] | null = null;
  k.registerBundle("inner.js", (_id, _p, view) => {
    inner = view.currentSecret;
    return null;
  });
  k.registerBundle("outer.js", async (_id, _p, view) => {
    await view.send([{ target: "b", payload: { type: "go" } }]);
    return null;
  });
  k.register(new Agent({ id: "a", parentId: ROOT, handlerModule: "outer.js", meta: { can_call: { b: "*" } } }));
  k.register(new Agent({ id: "b", parentId: ROOT, handlerModule: "inner.js", meta: { callable_by: { a: "*" } } }));
  await k.send([{ target: "a", payload: { type: "go", key: "x" }, secret: ["key"] }]);
  assert.deepEqual(inner, []);
  const calls = [...k.store.rows.values()].filter((r) => r.kind === "call");
  assert.deepEqual(calls.map((r) => r["secret"]), [["key"], undefined]);
});

test("a malformed `secret` is bad_arg `secret`, before anything is minted; `type` cannot be marked", async () => {
  const k = withRoot();
  k.register(new Agent({ id: "a", parentId: ROOT }));
  for (const bad of ["fields.x", [1], [""], ["a..b"], ["type"]] as unknown[]) {
    const [out] = await k.send([{ target: "a", payload: { type: "x" }, secret: bad as string[] }]);
    assert.equal(out!.status, "failed");
    assert.equal(out!.pid, null);
    const r = out!.result as Payload;
    assert.equal(r["reason"], "bad_arg");
    assert.equal(r["arg"], "secret");
  }
  assert.equal(k.store.rows.size, 0, "a refused item wrote a call row");
});

test("a host-bound send hands its marks to the bridge; the row shows the payload", async () => {
  const k = withRoot();
  const log = { forwards: [] as [string, Payload, readonly string[] | undefined][], emits: [] as [string, Payload][] };
  k.bridge = fakeBridge(log);
  await k.send([{ target: "creds", payload: { type: "set", fields: { pw: "p" } }, secret: ["fields.pw"] }]);
  assert.deepEqual(log.forwards, [["creds", { type: "set", fields: { pw: "p" } }, ["fields.pw"]]]);
  const call = [...k.store.rows.values()].find((r) => r.kind === "call")!;
  assert.deepEqual(call["payload"], { type: "set", fields: { pw: SHOWN } });
});

test("emitRemote: an agent's emit passes its egress gate for verb `emit`; the operator's is ungated", async () => {
  const k = withRoot();
  const log = { forwards: [] as [string, Payload, readonly string[] | undefined][], emits: [] as [string, Payload][] };
  k.bridge = fakeBridge(log);
  k.register(new Agent({ id: "denied", parentId: ROOT }));
  k.register(new Agent({ id: "granted", parentId: ROOT, meta: { can_call: { pty1: ["emit"] } } }));
  k.register(new Agent({ id: "other_verb", parentId: ROOT, meta: { can_call: { pty1: ["reload_html"] } } }));
  await as(k, "denied", (v) => v.emitRemote("pty1", { type: "reload_html", n: 1 }));
  await as(k, "other_verb", (v) => v.emitRemote("pty1", { type: "reload_html", n: 2 }));
  await as(k, "granted", (v) => v.emitRemote("pty1", { type: "reload_html", n: 3 }));
  k.emitRemote("pty1", { type: "reload_html", n: 4 });
  assert.deepEqual(log.emits.map(([, p]) => p["n"]), [3, 4]);
});

// ── the WS bridge ──────────────────────────────────────────────────────

class FakeSocket implements WebSocketLike {
  binaryType = "blob";
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  readonly sent: Envelope[] = [];
  constructor() {
    setTimeout(() => this.onopen?.(undefined), 0);
  }
  send(data: string | ArrayBuffer): void {
    this.sent.push(decodeFrame(data));
  }
  close(): void {}
  deliver(env: Envelope): void {
    this.onmessage?.({ data: encodeFrame(env).data });
  }
}

async function until(fn: () => boolean, ms = 3000): Promise<void> {
  const start = Date.now();
  while (!fn()) {
    if (Date.now() - start > ms) throw new Error("until: timeout");
    await new Promise((r) => setTimeout(r, 1));
  }
}

function wired() {
  const socks: FakeSocket[] = [];
  const k = new Kernel();
  k.setRoot(new Agent({ id: ROOT, meta: { guid: "g1" } }));
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: ROOT,
    makeSocket: () => {
      const s = new FakeSocket();
      socks.push(s);
      return s;
    },
    maxReconnectDelay: 5,
  });
  return { k, bridge, socks };
}

test("bridge: a marked host-bound call carries `secret` on its frame; an unmarked one carries none", async () => {
  const { k, bridge, socks } = wired();
  void k.send([{ target: "creds", payload: { type: "set", fields: { pw: "p" } }, secret: ["fields.pw"] }]);
  void k.send([{ target: "creds", payload: { type: "list" } }]);
  await until(() => (socks[0]?.sent.filter((f) => f["type"] === "call").length ?? 0) === 2);
  const [marked, plain] = socks[0]!.sent.filter((f) => f["type"] === "call");
  assert.deepEqual(marked!["secret"], ["fields.pw"]);
  assert.deepEqual(marked!["payload"], { type: "set", fields: { pw: "p" } }, "the value itself goes to the host");
  assert.equal("secret" in plain!, false);
  bridge.close();
});

test("bridge: an inbound call frame's `secret` marks the item this kernel runs", async () => {
  const { k, bridge, socks } = wired();
  let got: Payload | null = null;
  k.registerBundle("keep.js", (_id, payload) => {
    got = payload;
    return { ok: true } as Json;
  });
  k.register(new Agent({ id: "store", parentId: ROOT, handlerModule: "keep.js" }));
  bridge.announce();
  await until(() => socks.length === 1);
  await new Promise((r) => setTimeout(r, 5));
  socks[0]!.deliver({ type: "call", target: "store", payload: { type: "set", fields: { pw: "p" } }, id: "c1", secret: ["fields.pw"] });
  await until(() => socks[0]!.sent.some((f) => f["type"] === "reply" && f["id"] === "c1"));
  assert.deepEqual(got, { type: "set", fields: { pw: "p" } });
  const call = [...k.store.rows.values()].find((r) => r.kind === "call")!;
  assert.deepEqual(call["payload"], { type: "set", fields: { pw: SHOWN } });
  assert.deepEqual(call["secret"], ["fields.pw"]);
  bridge.close();
});
