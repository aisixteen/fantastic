// Two edges of SECRET marks (owner-approved 2026-10-03). Twin of python
// tests/kernel/test_secret_record_and_scrub.py.
//
// - S2 — a mark on a `create_agent`/`update_agent` payload field is refused: the
//   kernel writes every such field into the agent's RECORD, where the mark cannot
//   hold. This kernel answers neither verb; its `check` refuses before the call
//   leaves for the host, in the host's words.
// - S7 — the scrub replaces a dict KEY, a number or a bool whose text equals a secret
//   value: a reply keyed by the token it was given, or a PIN echoed as an int, used
//   to land in every record. Equal, not contained: a key is structure.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Payload } from "../src/kernel/json.ts";
import { PLACEHOLDER, check, scrub, shownPayload } from "../src/kernel/secret.ts";

const ROOT = "kernel_state";
const KEY = "sk-record-0123456789abcdef";
const PIN = 987654321;
const WHY = "a record field cannot be secret — use a credential_store ref";

function kernel(): Kernel {
  const k = new Kernel();
  k.registerBundle("scrub_probe.ts", async (id, payload) => {
    const verb = payload["type"];
    if (verb === "boot" || verb === "shutdown" || verb === "reflect") {
      return { id, sentence: "scrub probe", verbs: {} };
    }
    if (verb === "open_session") return { sessions: { [payload["token"] as string]: { open: true } }, count: 1 };
    if (verb === "echo_pin") return { pin: payload["pin"] ?? null, tries: 3 };
    return { error: `unknown verb ${String(verb)}` };
  });
  k.setRoot(new Agent({ id: ROOT }));
  k.register(new Agent({ id: "vault", parentId: ROOT, handlerModule: "scrub_probe.ts" }));
  return k;
}

async function ledger(k: Kernel): Promise<Payload[]> {
  const r = (await k.send([{ target: "kernel", payload: { type: "ledger" } }]))[0]!.result as Payload;
  return r["executions"] as Payload[];
}

// ─── S2: a record field is never secret ─────────────────────────────────────

test("a mark on a create_agent / update_agent field is refused", async () => {
  const k = kernel();
  const create: Payload = {
    type: "create_agent",
    parent_id: ROOT,
    handler_module: "scrub_probe.ts",
    id: "leaky",
    env: { TOKEN: KEY },
  };
  const cases: [Payload, unknown[], string][] = [
    [create, ["payload.env.TOKEN"], `path 'payload.env.TOKEN' names a field create_agent writes into the agent record: ${WHY}`],
    [create, [["payload", "env"]], `path ['payload', 'env'] names a field create_agent writes into the agent record: ${WHY}`],
    [create, ["payload.id"], `path 'payload.id' names a field create_agent writes into the agent record: ${WHY}`],
    [
      { type: "update_agent", id: "vault", note: KEY },
      ["payload.note"],
      `path 'payload.note' names a field update_agent writes into the agent record: ${WHY}`,
    ],
  ];
  for (const [payload, secret, text] of cases) {
    const out = (await k.send([{ target: "kernel", payload, secret: secret as never }]))[0]!;
    assert.equal(out.status, "failed", JSON.stringify(secret));
    assert.equal(out.pid, null);
    assert.deepEqual(out.result, { error: `send: secret ${text}`, reason: "bad_arg", arg: "secret" });
  }
});

test("payload.type is still judged first, and a result mark is unchanged", () => {
  const [, r] = check({ secret: ["payload.type"] }, { type: "update_agent", id: "x" });
  assert.equal((r as Payload)["error"], "send: secret cannot hide payload.type — the verb is how every row is read");
  const [s, none] = check({ secret: ["result"] }, { type: "create_agent", parent_id: ROOT, handler_module: "m", id: "plain" });
  assert.equal(none, null);
  assert.ok(s !== null);
});

// ─── S7: keys and numbers ───────────────────────────────────────────────────

test("a short secret never mangles a payload key", () => {
  // the verb's TEXT is scrubbed as before; the `type` KEY survives
  const p: Payload = { type: "set_key", key: "y" };
  const [s] = check({ secret: ["payload.key"] }, p);
  assert.deepEqual(shownPayload(p, s!), { type: "set_ke<secret>", key: PLACEHOLDER });
});

test("scrub replaces a key, a number and a bool whose text is a secret", () => {
  const got = scrub(
    { a: "xxabcxx", k: ["abc"], abc: "ab", xabcx: 1, n: 123, m: 1234, f: 1.5, t: true, z: null },
    ["ab", "abc", "123", "1.5", "True"],
  );
  assert.deepEqual(got, {
    a: "xx<secret>xx",
    k: ["<secret>"],
    "<secret>": "<secret>",
    // a key that only CONTAINS a secret is structure, kept
    xabcx: 1,
    n: "<secret>",
    m: 1234,
    f: "<secret>",
    // a bool's text is `True`/`False` (S7); null holds none
    t: "<secret>",
    z: null,
  });
});

test("a reply keyed by the secret is recorded scrubbed", async () => {
  const k = kernel();
  const out = (
    await k.send([{ target: "vault", payload: { type: "open_session", token: KEY }, secret: ["payload.token"], id: "sess-1" }])
  )[0]!;
  assert.deepEqual(out.result, { sessions: { [KEY]: { open: true } }, count: 1 });
  const rows = await ledger(k);
  const row = rows.find((r) => r["id"] === out.pid) as Payload;
  assert.deepEqual((row["outcome"] as Payload)["result"], { sessions: { [PLACEHOLDER]: { open: true } }, count: 1 });
  assert.ok(!JSON.stringify(rows).includes(KEY));
});

test("a secret number echoed as a number is recorded scrubbed", async () => {
  const k = kernel();
  const out = (await k.send([{ target: "vault", payload: { type: "echo_pin", pin: PIN }, secret: ["payload.pin"], id: "pin-1" }]))[0]!;
  assert.deepEqual(out.result, { pin: PIN, tries: 3 });
  const rows = await ledger(k);
  const row = rows.find((r) => r["id"] === out.pid) as Payload;
  assert.deepEqual((row["outcome"] as Payload)["result"], { pin: PLACEHOLDER, tries: 3 });
  assert.ok(!JSON.stringify(rows).includes(String(PIN)));
});
