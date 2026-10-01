// SECRET VALUES on a call (owner, 2026-10-01; `src/kernel/secret.ts`): the caller
// marks paths on the batch item; the target gets the real payload and the caller
// the real reply, while every record and mirror of the call shows `<secret>`.
// Twins of python tests/kernel/test_secret.py, plus the texts python produces,
// pinned byte for byte (measured against python `kernel/_secret.py` 2026-10-01).
//
// The ts kernel publishes no `send` event on its local state stream (only
// added/updated/removed), so python's state-stream leg has no twin here; the
// fanout this kernel does have — the target's own inbox and every watcher's — is
// asserted instead.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import type { Item, Outcome } from "../src/kernel/outcome.ts";
import { startKernel } from "../src/kernel/lifecycle.ts";
import { registerRamStore } from "../src/bundles/memory/ram_store.ts";
import { PLACEHOLDER, check, replayRefused, shownOutcome, shownPayload } from "../src/kernel/secret.ts";

const ROOT = "kernel_state";
const KEY = "sk-live-0123456789abcdef";
const ISSUED = "tok-issued-fedcba9876543210";

const got: Payload[] = [];

function kernel(): Kernel {
  const k = new Kernel();
  k.registerBundle("secret_probe.ts", async (_id, payload) => {
    const verb = payload["type"];
    if (verb === "boot" || verb === "shutdown") return null;
    got.push(payload);
    if (verb === "set_key") return { ok: true, len: String(payload["key"]).length };
    if (verb === "issue") return { key: ISSUED, note: "issued" };
    if (verb === "fail") throw new Error(`rejected key ${String(payload["key"])}`);
    return { error: `unknown verb ${String(verb)}` };
  });
  registerRamStore(k);
  k.setRoot(new Agent({ id: ROOT }));
  return k;
}

function mk(k: Kernel, id = "vault"): string {
  k.register(new Agent({ id, parentId: ROOT, handlerModule: "secret_probe.ts" }));
  return id;
}

async function call(k: Kernel, target: string, payload: Payload, item: Partial<Item> = {}): Promise<Outcome> {
  return (await k.send([{ target, payload, ...item }]))[0]!;
}

async function ledger(k: Kernel): Promise<Payload[]> {
  const r = (await call(k, "kernel", { type: "ledger" })).result as Payload;
  return r["executions"] as Payload[];
}

/** python `value in repr(x)` — every string, number and byte string (as utf-8). */
function mentions(x: unknown, value: string): boolean {
  const text = JSON.stringify(x, (_k, v: unknown) => (v instanceof Uint8Array ? new TextDecoder().decode(v) : v));
  return (text ?? "").includes(value);
}

// ─── the item field ──────────────────────────────────────────────────────────

test("a bad secret field is refused before anything runs", async () => {
  const k = kernel();
  const a = mk(k);
  got.length = 0;
  const cases: [unknown, string][] = [
    ["payload.key", "non-empty list"],
    [[], "non-empty list"],
    [["key"], "must start with 'payload.'"],
    [["payload"], "must start with 'payload.'"],
    [["payload.type"], "cannot hide payload.type"],
    [["payload.nope"], "names nothing in the payload"],
    [["payload..key"], "empty step"],
  ];
  for (const [secret, why] of cases) {
    const out = await call(k, a, { type: "set_key", key: KEY }, { secret: secret as string[] });
    assert.ok(out.status === "failed" && out.pid === null, `${JSON.stringify(secret)}: ${JSON.stringify(out)}`);
    const r = out.result as Payload;
    assert.ok(r["reason"] === "bad_arg" && r["arg"] === "secret", `${JSON.stringify(secret)}: ${JSON.stringify(r)}`);
    assert.ok(String(r["error"]).includes(why), `${JSON.stringify(secret)}: ${JSON.stringify(r)}`);
  }
  assert.deepEqual(got, [], "a refused item must not reach the handler");
});

// ─── a secret payload field ──────────────────────────────────────────────────

test("the target gets the value and every record shows the placeholder", async () => {
  const k = kernel();
  const a = mk(k);
  const events: unknown[] = [];
  k.addStateSubscriber((e) => events.push(e));
  const own: Payload[] = [];
  k.onInbox(a, (p) => own.push(p));
  got.length = 0;
  const out = await call(k, a, { type: "set_key", ref: "openai", key: KEY }, { secret: ["payload.key"] });
  // the handler and the caller see the real thing
  assert.deepEqual(got, [{ type: "set_key", ref: "openai", key: KEY }]);
  assert.equal(out.status, "completed");
  assert.deepEqual(out.result, { ok: true, len: KEY.length });
  // the call row: placeholder, the other fields intact, the PATHS recorded
  const row = k.store.rows.get(out.pid as number)!;
  assert.deepEqual(row["payload"], { type: "set_key", ref: "openai", key: PLACEHOLDER });
  assert.deepEqual(row["secret"], ["payload.key"]);
  // the dispatch mirror on the target's own inbox
  assert.ok(own.length > 0 && !own.some((m) => mentions(m, KEY)), JSON.stringify(own));
  assert.equal(own[0]!["key"], PLACEHOLDER);
  assert.ok(!mentions(events, KEY), JSON.stringify(events));
  // the ledger read-back
  assert.ok(!mentions(await ledger(k), KEY));
});

test("a watcher mirror shows the placeholder", async () => {
  const k = kernel();
  const a = mk(k);
  const mirrored: Payload[] = [];
  k.onInbox("spy", (p) => mirrored.push(p));
  assert.equal(k.watch(a, "spy"), null);
  await call(k, a, { type: "set_key", key: KEY }, { secret: ["payload.key"] });
  assert.ok(mirrored.length > 0 && !mirrored.some((m) => mentions(m, KEY)), JSON.stringify(mirrored));
});

test("every retry attempt hands the handler the value and mirrors the placeholder", async () => {
  const k = kernel();
  const a = mk(k);
  const mirrored: Payload[] = [];
  k.onInbox("spy", (p) => mirrored.push(p));
  assert.equal(k.watch(a, "spy"), null);
  got.length = 0;
  const out = await call(k, a, { type: "fail", key: KEY }, { retries: 1, secret: ["payload.key"] });
  assert.equal(out.status, "failed");
  assert.equal(out.attempts, 2);
  assert.deepEqual(got, [{ type: "fail", key: KEY }, { type: "fail", key: KEY }]);
  assert.deepEqual(mirrored, [{ type: "fail", key: PLACEHOLDER }, { type: "fail", key: PLACEHOLDER }]);
});

test("an error text quoting the value is scrubbed in the record", async () => {
  const k = kernel();
  const a = mk(k);
  const out = await call(k, a, { type: "fail", key: KEY }, { secret: ["payload.key"] });
  assert.equal(out.status, "failed");
  assert.ok(String((out.result as Payload)["error"]).includes(KEY), "the caller holds the real outcome");
  const led = await ledger(k);
  assert.ok(!mentions(led, KEY), JSON.stringify(led));
  const mine = led.find((e) => e["id"] === out.pid)!;
  assert.ok(String((mine["outcome"] as Payload)["error"]).includes(PLACEHOLDER), JSON.stringify(mine));
  // the live call row's stamp and the settle row too
  assert.ok(!mentions([...k.store.rows.values()], KEY));
});

// ─── a secret result ─────────────────────────────────────────────────────────

test("a secret result reaches the caller and is kept as the placeholder", async () => {
  const k = kernel();
  const a = mk(k);
  // named: the settle keeps the whole result
  const out = await call(k, a, { type: "issue" }, { id: "issue-1", secret: ["result.key"] });
  assert.deepEqual(out.result, { key: ISSUED, note: "issued" });
  const led = await ledger(k);
  assert.ok(!mentions(led, ISSUED), JSON.stringify(led));
  const mine = led.find((e) => e["name"] === "issue-1")!;
  assert.deepEqual((mine["outcome"] as Payload)["result"], { key: PLACEHOLDER, note: "issued" });
});

test("the whole result can be secret", async () => {
  const k = kernel();
  const a = mk(k);
  const out = await call(k, a, { type: "issue" }, { id: "issue-2", secret: ["result"] });
  assert.equal((out.result as Payload)["key"], ISSUED);
  const mine = (await ledger(k)).find((e) => e["name"] === "issue-2")!;
  assert.equal((mine["outcome"] as Payload)["result"], PLACEHOLDER);
});

// ─── the record cannot answer a replay ───────────────────────────────────────

async function bootWithStore(): Promise<Kernel> {
  const k = kernel();
  k.register(
    new Agent({
      id: "memory",
      parentId: ROOT,
      handlerModule: "ram_store.ts",
      meta: { operator: true, admins: ["kernel"] },
    }),
  );
  k.updateMeta(ROOT, { memory_store_id: "memory" });
  const out = (await startKernel(k, false)) as Payload;
  assert.equal(out["started"], true, JSON.stringify(out));
  return k;
}

test("the store never holds the value and a replay is refused", async () => {
  const k = await bootWithStore();
  const a = mk(k);
  got.length = 0;
  const first = await call(k, a, { type: "set_key", key: KEY }, { id: "key-1", secret: ["payload.key"] });
  assert.equal(first.status, "completed");
  // in RAM the execution still answers a re-send (the same process holds it)
  const again = await call(k, a, { type: "set_key", key: KEY }, { id: "key-1", secret: ["payload.key"] });
  assert.deepEqual(again.result, first.result);
  assert.equal(got.length, 1);
  await k.store.flush();
  const tables = ((await call(k, "memory", { type: "dump" })).result as Payload)["tables"];
  assert.ok(!mentions(tables, KEY), "the memory store holds the secret value");
  // once it lives only in the store, the record cannot answer — refused, loud
  k.ledger.executions.clear();
  k.ledger.names.clear();
  const replay = await call(k, a, { type: "set_key", key: KEY }, { id: "key-1", secret: ["payload.key"] });
  assert.equal(replay.status, "failed", JSON.stringify(replay));
  const r = replay.result as Payload;
  assert.ok(r["reason"] === "bad_arg" && String(r["error"]).includes("carried secrets"), JSON.stringify(replay));
  assert.equal(got.length, 1, "the refused replay must not run the handler");
});

// ─── python's texts, byte for byte ───────────────────────────────────────────

test("refusal texts and masks are python's, byte for byte", () => {
  const P: Payload = {
    type: "set_key",
    key: KEY,
    n: 42,
    f: 1.5,
    nested: { a: ["x", "y"] },
    b: new Uint8Array([0x61, 0x62, 0x27, 0x63, 0x0a, 0x00, 0xff]) as unknown as Json,
  };
  const bad = (error: string): Payload => ({ error, reason: "bad_arg", arg: "secret" });
  const cases: [unknown, Payload][] = [
    ["payload.key", bad("send: secret must be a non-empty list of paths, got 'payload.key'")],
    [[], bad("send: secret must be a non-empty list of paths, got []")],
    [["key"], bad("send: secret path 'key' must start with 'payload.' or be 'result[.…]'")],
    [["payload"], bad("send: secret path 'payload' must start with 'payload.' or be 'result[.…]'")],
    [["payload.type"], bad("send: secret cannot hide payload.type — the verb is how every row is read")],
    [["payload.nope"], bad("send: secret path 'payload.nope' names nothing in the payload")],
    [["payload..key"], bad("send: secret path 'payload..key' has an empty step")],
    [[""], bad("send: secret path must be a non-empty string, got ''")],
    [[3], bad("send: secret path must be a non-empty string, got 3")],
    [{ a: 1 }, bad("send: secret must be a non-empty list of paths, got {'a': 1}")],
  ];
  for (const [secret, want] of cases) {
    const [s, r] = check({ secret }, P);
    assert.equal(s, null);
    assert.deepEqual(r, want);
  }
  assert.deepEqual(check({ secret: null }, P), [null, null]);
  assert.deepEqual(check({ secret: ["payload.nested.a.1"] }, P)[0]!.values, ["y"]);
  // python `str()` of a number and of bytes (their repr)
  assert.deepEqual(
    check({ secret: ["result.x", "payload.n", "payload.f", "payload.b"] }, P)[0]!.values,
    ["42", "1.5", `b"ab'c\\n\\x00\\xff"`],
  );
  // a value is scrubbed out of EVERY string — the verb's text included
  const s = check({ secret: ["payload.nested.a.1", "payload.key"] }, P)[0]!;
  const { b: _b, ...noBytes } = P;
  assert.deepEqual(shownPayload(noBytes, s), {
    type: "set_ke<secret>",
    key: "<secret>",
    n: 42,
    f: 1.5,
    nested: { a: ["x", "<secret>"] },
  });
  assert.deepEqual(noBytes["nested"], { a: ["x", "y"] }, "the real payload is untouched");
  const o = check({ secret: ["payload.key", "result.tok"] }, P)[0]!;
  assert.deepEqual(
    shownOutcome({ pid: 7, status: "failed", result: { tok: "T0K", error: `bad ${KEY} and T0K` }, attempts: 1 }, o),
    { pid: 7, status: "failed", result: { tok: "<secret>", error: "bad <secret> and <secret>" }, attempts: 1 },
  );
  const tail = " carried secrets — its record holds <secret> placeholders, not values, so it cannot be answered from the record; send again under a new id";
  assert.deepEqual(replayRefused("key-1"), { error: `send: execution 'key-1'${tail}`, reason: "bad_arg", arg: "id" });
  assert.deepEqual(replayRefused(42), { error: `send: execution 42${tail}`, reason: "bad_arg", arg: "id" });
});
