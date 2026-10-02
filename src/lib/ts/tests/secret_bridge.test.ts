// SECRET MARKS CROSS THE BRIDGE (owner-approved, 2026-10-02) — the ts twin of
// python `io_bridge/tests/test_secret_across_bridges.py` + the peer-route case of
// `tests/kernel/test_peer_addressing.py`.
//
// A batch item's `secret` paths (`src/kernel/secret.ts`) used to stop at the edge
// of the kernel that held them: a call frame carried no marks, so the RECEIVING
// kernel recorded the value verbatim. Now the marks ride the call:
//
//   * a host-bound send puts its item's marks on the `call` frame as `secret`
//     (only when it marked some — never `[]`);
//   * a host's `call` frame is dispatched here WITH its `secret`, verbatim, so
//     this kernel's rows, ledger and memory store show `<secret>`, and a malformed
//     mark is this kernel's own refusal, before dispatch.
//
// Two ts kernels joined back to back by their WsBridges' control sockets (the
// real frame codec, no network) stand for "a kernel on each end": what one sends
// as a host-bound call, the other answers as a host's call. Each keeps every
// record (a memory store), so every record is checked.
//
// Not here, by design: python's `forward` re-rooting (this kernel has no
// `forward` verb — a host-bound send is its only outbound call), web_rest's
// header (no rest leg here), and the credential-store case (no such bundle here;
// a ts handler reading its call's marks — `kernel.currentSecret` — is pinned in
// tests/secret_surface.test.ts).

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { WsBridge } from "../src/transport/bridge.ts";
import type { WebSocketLike } from "../src/transport/bridge.ts";
import { decodeFrame, encodeFrame } from "../src/transport/frame.ts";
import type { Envelope } from "../src/transport/frame.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import type { Item, Outcome } from "../src/kernel/outcome.ts";
import { startKernel } from "../src/kernel/lifecycle.ts";
import { registerRamStore } from "../src/bundles/memory/ram_store.ts";
import { PLACEHOLDER } from "../src/kernel/secret.ts";

const ROOT = "kernel_state";
const KEY = "sk-cross-0123456789abcdef";
const ISSUED = "tok-cross-issued-fedcba98";

// ─── the probe the far kernel serves ─────────────────────────────────────────

const got: Payload[] = [];

async function vault(_id: string, payload: Payload): Promise<Json> {
  const verb = payload["type"];
  if (verb === "boot" || verb === "shutdown") return null;
  got.push(payload);
  if (verb === "set_key") return { ok: true, len: String(payload["key"]).length };
  if (verb === "issue") return { key: ISSUED, note: "issued" };
  if (verb === "refuse_issue") throw new Error(`issued ${ISSUED} but refused it`);
  return { error: `unknown verb ${String(verb)}` };
}

// ─── sockets: a fake one that answers, and a linked pair ─────────────────────

class Sock implements WebSocketLike {
  binaryType = "blob";
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  /** Every frame this end SENT, decoded — the TEST's capture of the wire. */
  readonly wire: Envelope[] = [];
  /** The far end, when linked: what this end sends is delivered there. */
  peer: Sock | null = null;
  readonly url: string;

  constructor(url: string) {
    this.url = url;
    setTimeout(() => this.onopen?.(undefined), 0);
  }
  send(data: string | ArrayBuffer): void {
    this.wire.push(decodeFrame(data));
    const p = this.peer;
    if (p !== null) setTimeout(() => p.onmessage?.({ data }), 0);
  }
  close(): void {}
  /** far side → this end */
  deliver(env: Envelope): void {
    this.onmessage?.({ data: encodeFrame(env).data });
  }
  calls(): Envelope[] {
    return this.wire.filter((f) => f["type"] === "call");
  }
  replyTo(id: string): Envelope | undefined {
    return this.wire.find((f) => f["type"] === "reply" && f["id"] === id);
  }
}

async function until(fn: () => boolean, ms = 2000): Promise<void> {
  const start = Date.now();
  while (!fn()) {
    if (Date.now() - start > ms) throw new Error("until: timeout");
    await new Promise((r) => setTimeout(r, 1));
  }
}

// ─── kernels, each keeping every record ──────────────────────────────────────

async function kernel(guid: string): Promise<Kernel> {
  const k = new Kernel();
  registerRamStore(k);
  k.registerBundle("xsecret_probe.ts", vault);
  k.setRoot(new Agent({ id: ROOT, meta: { guid } }));
  k.register(
    new Agent({ id: "memory", parentId: ROOT, handlerModule: "ram_store.ts", meta: { operator: true, admins: ["kernel"] } }),
  );
  k.updateMeta(ROOT, { memory_store_id: "memory" });
  const out = (await startKernel(k, false)) as Payload;
  assert.equal(out["started"], true, JSON.stringify(out));
  return k;
}

function vaultOn(k: Kernel, callableBy: Payload = { "A-g|*": "*" }): void {
  k.register(new Agent({ id: "vault", parentId: ROOT, handlerModule: "xsecret_probe.ts", meta: { callable_by: callableBy } }));
}

async function op(k: Kernel, target: string, payload: Payload, item: Partial<Item> = {}): Promise<Outcome> {
  return (await k.send([{ target, payload, ...item }]))[0]!;
}

/** Everything a kernel KEEPS of its calls: the rows in RAM, the ledger
 *  read-back, its state events, then — after a flush — the memory store. */
async function records(k: Kernel, events: unknown[]): Promise<{ [where: string]: unknown }> {
  const rows = [...k.store.rows.values()].map((r) => ({ ...r }));
  const ledger = (await op(k, "kernel", { type: "ledger" })).result;
  await k.store.flush();
  const store = ((await op(k, "memory", { type: "dump" })).result as Payload)["tables"];
  return { rows, ledger, events: [...events], store };
}

/** python `value in repr(x)` — every string (bytes as utf-8). */
function mentions(x: unknown, value: string): boolean {
  const text = JSON.stringify(x, (_k, v: unknown) => (v instanceof Uint8Array ? new TextDecoder().decode(v) : v));
  return (text ?? "").includes(value);
}

function assertKeptOut(who: string, recs: { [where: string]: unknown }, ...values: string[]): void {
  for (const [where, data] of Object.entries(recs)) {
    for (const v of values) assert.ok(!mentions(data, v), `${who}: ${v} in ${where}: ${JSON.stringify(data)}`);
  }
}

type Row = Payload & { id: number };

function callRow(recs: { [where: string]: unknown }, agent: string, verb: string): Row {
  const rows = (recs["rows"] as Row[]).filter((r) => r["kind"] === "call" && r["agent"] === agent && r["verb"] === verb);
  assert.equal(rows.length, 1, JSON.stringify(rows));
  return rows[0]!;
}

function settleOf(recs: { [where: string]: unknown }, callId: number): Row {
  const rows = (recs["rows"] as Row[]).filter((r) => r["kind"] === "settle" && r["parent"] === callId);
  assert.equal(rows.length, 1, JSON.stringify(rows));
  return rows[0]!;
}

/**
 * Kernel A (guid `A-g`) and kernel B (guid `B-g`), each with a WsBridge whose
 * control socket is the OTHER's: A's host-bound calls arrive at B as a host's
 * `call` frames (caller `A-g|<sender>`), B's replies resolve them on A.
 */
async function linked(): Promise<{
  ka: Kernel; kb: Kernel; sa: Sock; sb: Sock; ea: unknown[]; eb: unknown[]; close: () => void;
}> {
  got.length = 0;
  const ka = await kernel("A-g");
  const kb = await kernel("B-g");
  const ea: unknown[] = [];
  const eb: unknown[] = [];
  ka.addStateSubscriber((e) => ea.push(e));
  kb.addStateSubscriber((e) => eb.push(e));
  const socks: { [side: string]: Sock } = {};
  const ba = new WsBridge(ka, { origin: "ws://b", controlEndpoint: "b_ws", makeSocket: (u) => (socks["a"] = new Sock(u)) });
  const bb = new WsBridge(kb, { origin: "ws://a", controlEndpoint: "a_ws", makeSocket: (u) => (socks["b"] = new Sock(u)) });
  ba.announce();
  bb.announce();
  const sa = socks["a"]!;
  const sb = socks["b"]!;
  sa.peer = sb;
  sb.peer = sa;
  await until(() => sa.wire.some((f) => f["type"] === "hello") && sb.wire.some((f) => f["type"] === "hello"));
  return { ka, kb, sa, sb, ea, eb, close: () => (ba.close(), bb.close()) };
}

// ─── outbound: the item's marks ride the call frame ──────────────────────────

test("a host-bound item's marks ride its call frame verbatim; an unmarked call carries none", async () => {
  // python test_peer_addressing `test_the_items_secret_marks_ride_to_the_peer`
  const k = new Kernel();
  k.setRoot(new Agent({ id: ROOT, meta: { guid: "h0" } }));
  let sock: Sock | null = null;
  const bridge = new WsBridge(k, { origin: "ws://host", controlEndpoint: "web_loader", makeSocket: (u) => (sock = new Sock(u)) });
  const answer = async (n: number, data: Json): Promise<Envelope> => {
    await until(() => sock !== null && sock.calls().length === n);
    const f = sock!.calls()[n - 1]!;
    sock!.deliver({ type: "reply", id: f["id"] as string, data });
    return f;
  };
  const p = op(k, "creds", { type: "set", fields: { api_key: KEY } }, { secret: ["payload.fields", "result"] });
  const frame = await answer(1, { ok: true });
  const out = await p;
  assert.equal(out.status, "completed", JSON.stringify(out));
  assert.deepEqual(out.result, { ok: true });
  // the frame: the real payload for the far target, the paths as the item gave them
  assert.deepEqual(frame["payload"], { type: "set", fields: { api_key: KEY } });
  assert.equal(frame["caller"], "h0|kernel");
  assert.deepEqual(frame["secret"], ["payload.fields", "result"]);
  // this kernel masks its own record of the call
  assert.equal((k.store.rows.get(out.pid as number)!["payload"] as Payload)["fields"], PLACEHOLDER);
  // an unmarked call hands the frame no marks — the key is absent, never []
  const bare = op(k, "creds", { type: "reflect" });
  const f2 = await answer(2, { id: "creds" });
  await bare;
  assert.equal(Object.hasOwn(f2, "secret"), false, JSON.stringify(f2));
  // an AGENT's host-bound call carries its marks too (after its own egress gate)
  k.register(new Agent({ id: "view", parentId: ROOT, meta: { can_call: { creds: ["get"] } } }));
  const asView = op(k.scopedFor("view", null), "creds", { type: "get", ref: "openai/main" }, { secret: ["result"] });
  const f3 = await answer(3, { fields: { api_key: KEY } });
  assert.deepEqual((await asView).result, { fields: { api_key: KEY } }, "the caller holds the real reply");
  assert.deepEqual(f3["secret"], ["result"]);
  assert.equal(f3["caller"], "h0|view");
  // a payload with bytes goes as a BINARY frame: the marks ride its JSON head
  const bin = op(k, "pty1", { type: "paste", image: new Uint8Array([1, 2, 3]) as unknown as Json, key: KEY }, { secret: ["payload.key"] });
  const f4 = await answer(4, { pasted: true });
  await bin;
  assert.deepEqual(f4["secret"], ["payload.key"]);
  assert.deepEqual([...(f4["payload"] as { image: Uint8Array }).image], [1, 2, 3]);
  assert.ok(!mentions([...k.store.rows.values()], KEY), "this kernel's rows hold the value");
  bridge.close();
});

// ─── (a) a payload mark across the bridge — both kernels mask it ─────────────

test("a marked payload crosses the bridge and BOTH kernels mask it", async () => {
  const { ka, kb, sa, ea, eb, close } = await linked();
  vaultOn(kb);
  const out = await op(ka, "vault", { type: "set_key", ref: "openai", key: KEY }, { secret: ["payload.key"] });
  // the far TARGET got the value; the CALLER the real reply
  assert.deepEqual(got, [{ type: "set_key", ref: "openai", key: KEY }]);
  assert.equal(out.status, "completed", JSON.stringify(out));
  assert.deepEqual(out.result, { ok: true, len: KEY.length });
  assert.deepEqual(sa.calls().map((f) => f["secret"]), [["payload.key"]]);
  // the RECEIVING kernel: its call row is masked and names the paths
  const rb = await records(kb, eb);
  const row = callRow(rb, "vault", "set_key");
  assert.deepEqual(row["payload"], { type: "set_key", ref: "openai", key: PLACEHOLDER });
  assert.deepEqual(row["secret"], ["payload.key"]);
  assert.equal(row["caller"], "A-g|kernel");
  assertKeptOut("B", rb, KEY);
  // the calling kernel masks its own record of the call
  const ra = await records(ka, ea);
  assert.equal((callRow(ra, "vault", "set_key")["payload"] as Payload)["key"], PLACEHOLDER);
  assertKeptOut("A", ra, KEY);
  close();
});

// ─── (b) a result mark masks the RECEIVING kernel's records ──────────────────

test("a result mark masks the receiving kernel's records; the caller gets the real result", async () => {
  const { ka, kb, ea, eb, close } = await linked();
  vaultOn(kb);
  const part = await op(ka, "vault", { type: "issue" }, { secret: ["result.key"] });
  assert.deepEqual(part.result, { key: ISSUED, note: "issued" });
  const whole = await op(ka, "vault", { type: "issue" }, { secret: ["result"] });
  assert.deepEqual(whole.result, { key: ISSUED, note: "issued" });
  // what B keeps of each outcome (the ledger reads back exactly this)
  const shown = [...kb.ledger.executions.values()].filter((e) => e.target === "vault").map((e) => e.shownOut?.result);
  assert.deepEqual(shown, [{ key: PLACEHOLDER, note: "issued" }, PLACEHOLDER]);
  // a DEAD ending keeps its error text — the marked one keeps no value in it
  const dead = await op(ka, "vault", { type: "refuse_issue" }, { secret: ["result"] });
  assert.ok(String((dead.result as Payload)["error"]).includes(ISSUED), "the caller holds the real outcome");
  const rb = await records(kb, eb);
  const issues = (rb["rows"] as Row[]).filter((r) => r["kind"] === "call" && r["agent"] === "vault" && r["verb"] === "issue");
  assert.deepEqual(issues.map((r) => r["secret"]), [["result.key"], ["result"]]);
  const refused = callRow(rb, "vault", "refuse_issue");
  assert.deepEqual(refused["secret"], ["result"]);
  // the dead ending's settle keeps no error text: its whole result is the mark
  assert.deepEqual(settleOf(rb, refused.id)["outcome"], { status: "failed" });
  assertKeptOut("B", rb, ISSUED);
  assertKeptOut("A", await records(ka, ea), ISSUED);
  close();
});

test("without a mark the receiving kernel keeps the value — what the marks prevent is real", async () => {
  const { ka, kb, eb, close } = await linked();
  vaultOn(kb);
  await op(ka, "vault", { type: "refuse_issue" });
  const rb = await records(kb, eb);
  const row = callRow(rb, "vault", "refuse_issue");
  assert.equal(Object.hasOwn(row, "secret"), false);
  assert.ok(String((settleOf(rb, row.id)["outcome"] as Payload)["error"]).includes(ISSUED), JSON.stringify(rb["rows"]));
  close();
});

// ─── (c) a malformed mark on an inbound frame is refused before dispatch ─────

const BAD: [unknown, string][] = [
  ["payload.key", "must be a non-empty list of paths"],
  [[], "must be a non-empty list of paths"],
  [["payload.nope"], "names nothing in the payload"],
  [["payload.type"], "cannot hide payload.type"],
  [["payload"], "must start with 'payload.'"],
];

test("a malformed mark on an inbound call frame is this kernel's refusal, before dispatch", async () => {
  got.length = 0;
  const kb = await kernel("B-g");
  const eb: unknown[] = [];
  kb.addStateSubscriber((e) => eb.push(e));
  vaultOn(kb, { "h0|*": "*" });
  let sock: Sock | null = null;
  const bridge = new WsBridge(kb, { origin: "ws://host", controlEndpoint: "web_loader", makeSocket: (u) => (sock = new Sock(u)) });
  bridge.announce();
  await until(() => sock !== null && sock.wire.some((f) => f["type"] === "hello"));
  for (const [n, [secret, why]] of BAD.entries()) {
    const id = `bad-${n}`;
    sock!.deliver({ type: "call", id, target: "vault", payload: { type: "set_key", key: KEY }, caller: "h0|kernel", secret });
    await until(() => sock!.replyTo(id) !== undefined);
    const data = sock!.replyTo(id)!["data"] as Payload;
    assert.deepEqual([data["reason"], data["arg"]], ["bad_arg", "secret"], JSON.stringify([secret, data]));
    assert.ok(String(data["error"]).includes(why) && !String(data["error"]).includes(KEY), JSON.stringify([secret, data]));
  }
  assert.deepEqual(got, [], "a refused item must not reach the handler");
  const rb = await records(kb, eb);
  assert.deepEqual((rb["rows"] as Row[]).filter((r) => r["agent"] === "vault"), []);
  assertKeptOut("B", rb, KEY);
  // a well-formed mark on the same wire is dispatched with it
  sock!.deliver({ type: "call", id: "good", target: "vault", payload: { type: "set_key", key: KEY }, caller: "h0|kernel", secret: ["payload.key"] });
  await until(() => sock!.replyTo("good") !== undefined);
  assert.deepEqual(sock!.replyTo("good")!["data"], { ok: true, len: KEY.length });
  assert.deepEqual(got, [{ type: "set_key", key: KEY }]);
  assertKeptOut("B", await records(kb, eb), KEY);
  bridge.close();
});

// ─── (f) nothing on the way prints, logs or mirrors the value ────────────────

test("no bridge, codec or mirror prints, logs or keeps the value", async () => {
  const { ka, kb, ea, eb, close } = await linked();
  vaultOn(kb);
  const printed: string[] = [];
  const con = globalThis.console as unknown as { [m: string]: (...a: unknown[]) => void };
  const saved: { [m: string]: (...a: unknown[]) => void } = {};
  for (const m of ["log", "info", "warn", "error", "debug", "trace"]) {
    saved[m] = con[m]!;
    con[m] = (...a: unknown[]) => void printed.push(a.map((x) => (typeof x === "string" ? x : JSON.stringify(x))).join(" "));
  }
  try {
    // a watcher on the far target: its mirror is a record of the call
    const mirrored: Payload[] = [];
    kb.onInbox("spy", (p) => mirrored.push(p));
    assert.equal(kb.watch("vault", "spy"), null);
    const out = await op(ka, "vault", { type: "set_key", key: KEY }, { secret: ["payload.key", "result"] });
    assert.deepEqual(out.result, { ok: true, len: KEY.length });
    assert.deepEqual(mirrored, [{ type: "set_key", key: PLACEHOLDER }]);
    // the bridge holds no frame once the call settled
    for (const k of [ka, kb]) {
      const conns = (k.bridge as unknown as { conns: Map<string, { pending: Map<string, unknown> }> }).conns;
      for (const c of conns.values()) assert.equal(c.pending.size, 0);
    }
    assertKeptOut("A", await records(ka, ea), KEY);
    assertKeptOut("B", await records(kb, eb), KEY);
  } finally {
    for (const m of Object.keys(saved)) con[m] = saved[m]!;
    close();
  }
  assert.ok(!printed.some((s) => s.includes(KEY)), JSON.stringify(printed));
});
