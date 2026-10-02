// THE SECRET SURFACE a ts bundle and a ts view hold (owner-approved, 2026-10-02):
//
//   (a) `kernel.currentSecret` — a handler reads the paths its caller marked on the
//       item that started its dispatch (python `kernel.current_secret()`), so a ts
//       bundle can refuse an unmarked call. Stamped per dispatch view, like
//       `currentCaller`: a handler's own sends never inherit it.
//   (b) `Host.call` / `Host.callHost` / `Host.callBinary` and `Kernel.callHost` /
//       `Kernel.forwardBinary` take an optional `secret` — the item's marks: masked
//       in this kernel's records (the batch path) and carried on the call frame.
//
// The socket is the only stand-in; the kernel, the bridge and the codec are real.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { WsBridge } from "../src/transport/bridge.ts";
import type { WebSocketLike } from "../src/transport/bridge.ts";
import { Host } from "../src/bundles/host.ts";
import { encodeFrame, decodeFrame } from "../src/transport/frame.ts";
import type { Envelope } from "../src/transport/frame.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import type { Item, Outcome } from "../src/kernel/outcome.ts";
import { BAD_ARG, refusal } from "../src/kernel/refusal.ts";
import { PLACEHOLDER } from "../src/kernel/secret.ts";

const ROOT = "kernel_state";
const KEY = "sk-surface-0123456789abcdef";

// ─── a bundle that reports the marks it was called with ─────────────────────

/** What each dispatch read as `currentSecret`, in order: `[agent, verb, marks]`. */
const seen: [string, string, readonly string[]][] = [];

async function probe(id: string, payload: Payload, k: Kernel): Promise<Json> {
  const verb = String(payload["type"]);
  if (verb === "boot" || verb === "shutdown") return null;
  seen.push([id, verb, k.currentSecret]);
  if (verb === "relay") {
    // the handler's OWN sends: one unmarked, one with marks of its own
    const [plain] = await k.send([{ target: "inner", payload: { type: "peek" } }]);
    const [marked] = await k.send([{ target: "inner", payload: { type: "peek", pin: "1" }, secret: ["payload.pin"] }]);
    // its own marks survive its awaits
    return { mine: [...k.currentSecret], plain: plain!.result, marked: marked!.result };
  }
  if (verb === "slow") {
    await new Promise((r) => setTimeout(r, Number(payload["ms"])));
    return { marks: [...k.currentSecret] };
  }
  if (verb === "fail") throw new Error("again");
  return { marks: [...k.currentSecret] };
}

/** A bundle that must only ever be called WITH a mark (python credential_store's
 *  rule): it refuses an unmarked `put` before touching the value. */
const vault: string[] = [];
function strict(_id: string, payload: Payload, k: Kernel): Json {
  const verb = payload["type"];
  if (verb === "boot" || verb === "shutdown") return null;
  if (!k.currentSecret.includes("payload.value")) {
    return refusal(BAD_ARG, "put: mark payload.value secret", { extra: { arg: "secret" } });
  }
  vault.push(String(payload["value"]));
  return { stored: true };
}

function kernel(guid: string | null = null): Kernel {
  const k = new Kernel();
  k.registerBundle("probe.ts", probe);
  k.registerBundle("strict.ts", strict);
  k.setRoot(new Agent({ id: ROOT, meta: guid === null ? {} : { guid } }));
  k.register(new Agent({ id: "outer", parentId: ROOT, handlerModule: "probe.ts", meta: { can_call: { inner: "*" } } }));
  k.register(new Agent({ id: "inner", parentId: ROOT, handlerModule: "probe.ts", meta: { callable_by: { outer: "*" } } }));
  k.register(new Agent({ id: "vault", parentId: ROOT, handlerModule: "strict.ts" }));
  seen.length = 0;
  vault.length = 0;
  return k;
}

async function op(k: Kernel, target: string, payload: Payload, item: Partial<Item> = {}): Promise<Outcome> {
  return (await k.send([{ target, payload, ...item }]))[0]!;
}

// ─── (a) currentSecret ───────────────────────────────────────────────────────

test("a handler reads the marks its caller put on the item; none marked is empty", async () => {
  const k = kernel();
  const marked = await op(k, "inner", { type: "peek", key: KEY }, { secret: ["payload.key", "result"] });
  assert.deepEqual(marked.result, { marks: ["payload.key", "result"] });
  const plain = await op(k, "inner", { type: "peek" });
  assert.deepEqual(plain.result, { marks: [] });
  // the marks as the handler holds them: frozen, so no handler can rewrite them
  assert.equal(Object.isFrozen(seen[0]![2]), true);
  // outside any dispatch — the operator, a foreign caller's view — nothing is marked
  assert.deepEqual(k.currentSecret, []);
  assert.deepEqual(k.foreignView("h0|ai").currentSecret, []);
  assert.deepEqual(k.scopedFor("outer", null).currentSecret, []);
});

test("a handler's own sends never inherit its marks; each dispatch reads its own", async () => {
  const k = kernel();
  const out = await op(k, "outer", { type: "relay", key: KEY }, { secret: ["payload.key"] });
  assert.equal(out.status, "completed", JSON.stringify(out));
  assert.deepEqual(out.result, {
    mine: ["payload.key"],
    plain: { marks: [] },
    marked: { marks: ["payload.pin"] },
  });
  assert.deepEqual(seen, [
    ["outer", "relay", ["payload.key"]],
    ["inner", "peek", []],
    ["inner", "peek", ["payload.pin"]],
  ]);
});

test("concurrent dispatches each keep their own marks across awaits", async () => {
  const k = kernel();
  const outs = await k.send([
    { target: "inner", payload: { type: "slow", ms: 15, a: KEY }, secret: ["payload.a"] },
    { target: "inner", payload: { type: "slow", ms: 1 } },
    { target: "inner", payload: { type: "slow", ms: 5 }, secret: ["result"] },
  ]);
  assert.deepEqual(outs.map((o) => o.result), [{ marks: ["payload.a"] }, { marks: [] }, { marks: ["result"] }]);
});

test("every retry attempt reads the marks", async () => {
  const k = kernel();
  const out = await op(k, "inner", { type: "fail", key: KEY }, { retries: 2, secret: ["payload.key"] });
  assert.equal(out.status, "failed");
  assert.equal(out.attempts, 3);
  assert.deepEqual(seen.map((s) => s[2]), [["payload.key"], ["payload.key"], ["payload.key"]]);
});

test("a ts bundle refuses an unmarked call by its marks; a marked one is recorded as shown", async () => {
  const k = kernel();
  const bare = await op(k, "vault", { type: "put", value: KEY });
  assert.equal(bare.status, "errored");
  assert.deepEqual(bare.result, { error: "put: mark payload.value secret", reason: "bad_arg", arg: "secret" });
  assert.deepEqual(vault, [], "the refused call never stored the value");
  const ok = await op(k, "vault", { type: "put", value: KEY }, { secret: ["payload.value"] });
  assert.deepEqual(ok.result, { stored: true });
  assert.deepEqual(vault, [KEY]);
  assert.equal((k.store.rows.get(ok.pid as number)!["payload"] as Payload)["value"], PLACEHOLDER);
  // a mark on another path does not satisfy it
  const other = await op(k, "vault", { type: "put", value: KEY, note: "n" }, { secret: ["payload.note"] });
  assert.equal(other.status, "errored");
  assert.deepEqual(vault, [KEY]);
});

// ─── sockets: the host end of the bridge ─────────────────────────────────────

class Sock implements WebSocketLike {
  binaryType = "blob";
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  readonly sent: Envelope[] = [];
  readonly binary: boolean[] = [];
  readonly url: string;

  constructor(url: string) {
    this.url = url;
    setTimeout(() => this.onopen?.(undefined), 0);
  }
  send(data: string | ArrayBuffer): void {
    this.sent.push(decodeFrame(data));
    this.binary.push(typeof data !== "string");
  }
  close(): void {}
  deliver(env: Envelope): void {
    this.onmessage?.({ data: encodeFrame(env).data });
  }
  calls(): Envelope[] {
    return this.sent.filter((f) => f["type"] === "call");
  }
  replyTo(id: string): Envelope | undefined {
    return this.sent.find((f) => f["type"] === "reply" && f["id"] === id);
  }
}

async function until(fn: () => boolean, ms = 2000): Promise<void> {
  const start = Date.now();
  while (!fn()) {
    if (Date.now() - start > ms) throw new Error("until: timeout");
    await new Promise((r) => setTimeout(r, 1));
  }
}

function wired(): { k: Kernel; bridge: WsBridge; sock: () => Sock } {
  const k = kernel("g1");
  let s: Sock | null = null;
  const bridge = new WsBridge(k, { origin: "ws://host", controlEndpoint: "web_loader", makeSocket: (u) => (s = new Sock(u)) });
  bridge.announce();
  return { k, bridge, sock: () => s as unknown as Sock };
}

/** Answer the n-th call frame on the socket with `data`; returns that frame. */
async function answer(sock: () => Sock, n: number, data: Json): Promise<Envelope> {
  await until(() => sock() !== null && sock().calls().length === n);
  const f = sock().calls()[n - 1]!;
  sock().deliver({ type: "reply", id: f["id"] as string, data });
  return f;
}

test("a host's call frame hands its marks to the handler here; a frame without marks hands none", async () => {
  const { k, bridge, sock } = wired();
  k.updateMeta("inner", { callable_by: { "h0|*": "*" } });
  await until(() => sock() !== null && sock().sent.some((f) => f["type"] === "hello"));
  sock().deliver({
    type: "call", id: "c1", target: "inner", caller: "h0|ai", payload: { type: "peek", key: KEY }, secret: ["payload.key"],
  });
  sock().deliver({ type: "call", id: "c2", target: "inner", caller: "h0|ai", payload: { type: "peek" } });
  await until(() => sock().replyTo("c1") !== undefined && sock().replyTo("c2") !== undefined);
  assert.deepEqual(sock().replyTo("c1")!["data"], { marks: ["payload.key"] });
  assert.deepEqual(sock().replyTo("c2")!["data"], { marks: [] });
  bridge.close();
});

// ─── (b) the host-bound surface takes `secret` ───────────────────────────────

test("Host.call: the marks become the item's — masked here, on the frame for a host target, read by a local one", async () => {
  const { k, bridge, sock } = wired();
  const host = new Host(k, "outer");
  // a LOCAL target: the handler reads the marks, this kernel's record shows <secret>
  assert.deepEqual(await host.call("inner", { type: "peek", key: KEY }, ["payload.key"]), { marks: ["payload.key"] });
  // a HOST target: the frame carries the marks verbatim, the caller the real reply
  const p = host.call("creds", { type: "set", fields: { api_key: KEY } }, ["payload.fields", "result"]);
  const f1 = await answer(sock, 1, { ok: true, key: KEY });
  assert.deepEqual(await p, { ok: true, key: KEY });
  assert.deepEqual(f1["secret"], ["payload.fields", "result"]);
  assert.deepEqual(f1["payload"], { type: "set", fields: { api_key: KEY } });
  // no secret → the frame has no `secret` key at all
  const bare = host.call("creds", { type: "reflect" });
  const f2 = await answer(sock, 2, { id: "creds" });
  await bare;
  assert.equal(Object.hasOwn(f2, "secret"), false, JSON.stringify(f2));
  // this kernel's records never held the value
  const text = JSON.stringify([...k.store.rows.values()]);
  assert.ok(!text.includes(KEY), text);
  assert.ok(text.includes(PLACEHOLDER), text);
  // a malformed mark is the item's own refusal: nothing dispatched, no frame
  assert.deepEqual(await host.call("creds", { type: "set", key: KEY }, ["payload.nope"]), {
    error: "send: secret path 'payload.nope' names nothing in the payload",
    reason: "bad_arg",
    arg: "secret",
  });
  assert.deepEqual((await host.call("creds", { type: "set", key: KEY }, [])) as Payload, {
    error: "send: secret must be a non-empty list of paths, got []",
    reason: "bad_arg",
    arg: "secret",
  });
  assert.equal(sock().calls().length, 2);
  bridge.close();
});

test("callHost / callBinary (Host and Kernel): the marks ride the frame; a bad mark is refused before a byte leaves", async () => {
  const { k, bridge, sock } = wired();
  const host = new Host(k, "outer");
  const lit = host.callHost("kernel", { type: "create_agent", token: KEY }, ["payload.token"]);
  const f1 = await answer(sock, 1, { id: "x" });
  assert.deepEqual(await lit, { id: "x" });
  assert.equal(f1["target"], "kernel", "callHost forwards the LITERAL target");
  assert.deepEqual(f1["secret"], ["payload.token"]);
  const bin = host.callBinary("pty1", { type: "paste_image", image: new Uint8Array([1, 2]), key: KEY }, ["payload.key", "result"]);
  const f2 = await answer(sock, 2, { pasted: true });
  assert.deepEqual(await bin, { pasted: true });
  assert.equal(sock().binary[sock().sent.indexOf(f2)], true, "bytes ride a binary frame; the marks ride its JSON head");
  assert.deepEqual(f2["secret"], ["payload.key", "result"]);
  assert.deepEqual([...((f2["payload"] as Payload)["image"] as unknown as Uint8Array)], [1, 2]);
  // the kernel entry points directly, with and without marks
  const viaK = k.callHost("kernel", { type: "reflect" });
  const f3 = await answer(sock, 3, { id: "host_root" });
  await viaK;
  assert.equal(Object.hasOwn(f3, "secret"), false, JSON.stringify(f3));
  const viaKB = k.forwardBinary("pty1", { type: "paste_image", image: new Uint8Array([3]), pin: "4321" }, ["payload.pin"]);
  const f4 = await answer(sock, 4, { pasted: true });
  await viaKB;
  assert.deepEqual(f4["secret"], ["payload.pin"]);
  // malformed marks: refused bad_arg, the value never put on a frame
  const bad: [Promise<Json>, string][] = [
    [k.callHost("kernel", { type: "set", key: KEY }, ["payload.nope"]), "send: secret path 'payload.nope' names nothing in the payload"],
    [k.callHost("kernel", { type: "set", key: KEY }, []), "send: secret must be a non-empty list of paths, got []"],
    [k.callHost("kernel", { type: "set", key: KEY }, ["payload.type"]), "send: secret cannot hide payload.type — the verb is how every row is read"],
    [k.forwardBinary("pty1", { type: "paste_image", image: new Uint8Array([5]) }, ["key"]), "send: secret path 'key' must start with 'payload.' or be 'result[.…]'"],
    [host.callBinary("pty1", { type: "paste_image" }, ["payload.image"]), "send: secret path 'payload.image' names nothing in the payload"],
  ];
  for (const [p, error] of bad) assert.deepEqual(await p, { error, reason: "bad_arg", arg: "secret" });
  assert.equal(sock().calls().length, 4, "no refused call reached the wire");
  bridge.close();
});

test("an agent's host-bound call: a bad mark is refused before its egress gate, a good one still needs the grant", async () => {
  const { k, bridge, sock } = wired();
  const asOuter = k.scopedFor("outer", null); // can_call reaches `inner` only
  // the item's own fields first, as `launch` validates them before the gate
  assert.equal(((await asOuter.callHost("creds", { type: "set" }, ["payload.key"])) as Payload)["reason"], "bad_arg");
  // a well-formed mark does not open the gate
  assert.equal(((await asOuter.callHost("creds", { type: "set", key: KEY }, ["payload.key"])) as Payload)["reason"], "unauthorized");
  assert.equal(((await asOuter.forwardBinary("pty1", { type: "paste_image", key: KEY }, ["payload.key"])) as Payload)["reason"], "unauthorized");
  // granted, it goes out with its marks
  k.updateMeta("outer", { can_call: { inner: "*", creds: ["set"] } });
  const p = asOuter.callHost("creds", { type: "set", key: KEY }, ["payload.key"]);
  const f = await answer(sock, 1, { ok: true });
  assert.deepEqual(await p, { ok: true });
  assert.deepEqual(f["secret"], ["payload.key"]);
  assert.equal(f["caller"], "g1|outer");
  bridge.close();
});
