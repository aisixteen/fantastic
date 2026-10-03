// JOINING a secret-marked execution (audit 2026-10-03, F1). Twin of python
// tests/kernel/test_secret_join.py.
//
// A join hands back the execution's outcome without running anything, re-gated on
// the ORIGINAL verb. Measured in python before the fix: `eve`, granted the same
// verb on a credential_store but never a reader of the ref, joined `alice`'s
// marked `get` by its int pid (and by its name) and received the real key — the
// store's per-ref `readers` check runs in its handler, which a joiner never
// reaches, and the joiner marked nothing.
//
// The rule: an execution whose item carried `secret` marks hands its REAL outcome
// to its ORIGINAL caller only — the identity that opened it: the sending agent,
// the operator (no origin), or a caller on another kernel by its qualified
// `<guid>|<agent>`. Every other joiner gets the outcome AS SHOWN — the result
// marks and every secret value replaced by `<secret>`, exactly what the ledger
// reads back. A join of an unmarked execution is unchanged.
//
// python's `carried_secret` test has no twin: it is asked only by ai_core (F2),
// and this kernel hosts no AI agent.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Payload } from "../src/kernel/json.ts";
import type { Item, Outcome } from "../src/kernel/outcome.ts";
import { PLACEHOLDER } from "../src/kernel/secret.ts";

const ROOT = "kernel_state";
const KEY = "sk-join-0123456789abcdef";

let release: (() => void) | null = null;
let released: Promise<void> = Promise.resolve();

function kernel(): Kernel {
  const k = new Kernel();
  k.registerBundle("secret_join_probe.ts", async (id, payload) => {
    const verb = payload["type"];
    if (verb === "boot" || verb === "shutdown" || verb === "reflect") {
      return { id, sentence: "join probe", verbs: {} };
    }
    if (verb === "issue") return { key: KEY, note: "issued" };
    if (verb === "slow_issue") {
      await released;
      return { key: KEY, note: "issued" };
    }
    if (verb === "echo") return { echo: payload["text"] ?? null };
    return { error: `unknown verb ${String(verb)}` };
  });
  k.setRoot(new Agent({ id: ROOT }));
  k.register(
    new Agent({
      id: "vault",
      parentId: ROOT,
      handlerModule: "secret_join_probe.ts",
      meta: { callable_by: { alice: "*", eve: "*", "P-g|*": "*" } },
    }),
  );
  for (const id of ["alice", "eve"]) {
    k.register(new Agent({ id, parentId: ROOT, meta: { can_call: { vault: "*" } } }));
  }
  return k;
}

/** One item as the operator (no `as`), as a local agent, or — `foreign` — as a
 *  caller on another kernel (`<guid>|<agent>`, the view a bridge's inbound call
 *  runs under). */
async function send(k: Kernel, item: Item, as?: string, foreign = false): Promise<Outcome> {
  const view = as === undefined ? k : foreign ? k.foreignView(as) : k.scopedFor(as, null);
  return (await view.send([item]))[0]!;
}

function issue(extra: Partial<Item> = {}): Item {
  return { target: "vault", payload: { type: "issue" }, secret: ["result.key"], ...extra };
}

function shown(out: Outcome): boolean {
  const r = out.result as Payload;
  return out.status === "completed" && r["key"] === PLACEHOLDER && r["note"] === "issued" && Object.keys(r).length === 2;
}

function real(out: Outcome): boolean {
  const r = out.result as Payload;
  return out.status === "completed" && r["key"] === KEY && r["note"] === "issued" && Object.keys(r).length === 2;
}

test("a non-originating joiner gets the shown outcome by pid", async () => {
  const k = kernel();
  const own = await send(k, issue(), "alice");
  assert.ok(real(own), JSON.stringify(own));
  const joined = await send(k, { target: "vault", id: own.pid as number, payload: { type: "issue" } }, "eve");
  assert.ok(shown(joined), JSON.stringify(joined));
  assert.ok(!JSON.stringify(joined).includes(KEY), JSON.stringify(joined));
});

test("a non-originating joiner gets the shown outcome by name", async () => {
  const k = kernel();
  const own = await send(k, issue({ id: "keyread" }), "alice");
  assert.ok(real(own), JSON.stringify(own));
  const joined = await send(k, { target: "vault", id: "keyread", payload: { type: "issue" } }, "eve");
  assert.ok(shown(joined), JSON.stringify(joined));
  assert.equal(joined.pid, own.pid, JSON.stringify(joined));
});

test("the original caller joining again gets the real outcome", async () => {
  const k = kernel();
  const own = await send(k, issue({ id: "mine" }), "alice");
  const again = await send(k, { target: "vault", id: "mine", payload: { type: "issue" } }, "alice");
  assert.ok(real(again), JSON.stringify(again));
  assert.equal(again.pid, own.pid);
});

test("the operator is an identity like any other", async () => {
  // The operator joining an AGENT's secret execution is not its caller (shown);
  // an agent joining the OPERATOR's is not either (shown); the operator joining
  // its own gets the real outcome.
  const k = kernel();
  const alices = await send(k, issue({ id: "a1" }), "alice");
  const opJoin = await send(k, { target: "vault", id: alices.pid as number, payload: { type: "issue" } });
  assert.ok(shown(opJoin), JSON.stringify(opJoin));
  const ops = await send(k, issue({ id: "o1" }));
  assert.ok(real(ops), JSON.stringify(ops));
  const eveJoin = await send(k, { target: "vault", id: "o1", payload: { type: "issue" } }, "eve");
  assert.ok(shown(eveJoin), JSON.stringify(eveJoin));
  const opAgain = await send(k, { target: "vault", id: "o1", payload: { type: "x" } });
  assert.ok(real(opAgain), JSON.stringify(opAgain));
});

test("two foreign callers of one peer kernel are two identities", async () => {
  // python relays them through ONE bridge leg; here each arrives under its own
  // foreign view. Either way each is its qualified `<guid>|<agent>`.
  const k = kernel();
  const first = await send(k, issue({ id: "peer-read" }), "P-g|a", true);
  assert.ok(real(first), JSON.stringify(first));
  const other = await send(k, { target: "vault", id: "peer-read", payload: { type: "issue" } }, "P-g|b", true);
  assert.ok(shown(other), JSON.stringify(other));
  const same = await send(k, { target: "vault", id: "peer-read", payload: { type: "issue" } }, "P-g|a", true);
  assert.ok(real(same), JSON.stringify(same));
});

test("a joiner waiting on an execution in flight gets it shown", async () => {
  released = new Promise((res) => {
    release = res;
  });
  const k = kernel();
  const owner = send(
    k,
    { target: "vault", id: "slow", payload: { type: "slow_issue" }, secret: ["result.key"] },
    "alice",
  );
  for (let i = 0; i < 50 && k.ledger.resolveName("slow") === undefined; i++) await Promise.resolve();
  assert.notEqual(k.ledger.resolveName("slow"), undefined, "the owner's execution never opened");
  const joiner = send(k, { target: "vault", id: "slow", payload: { type: "x" } }, "eve");
  await Promise.resolve();
  release!();
  const [own, joined] = await Promise.all([owner, joiner]);
  assert.ok(real(own), JSON.stringify(own));
  assert.ok(shown(joined), JSON.stringify(joined));
});

test("a join of an unmarked execution is unchanged", async () => {
  const k = kernel();
  const own = await send(k, { target: "vault", id: "plain", payload: { type: "echo", text: "hi" } }, "alice");
  const joined = await send(k, { target: "vault", id: "plain", payload: { type: "echo" } }, "eve");
  assert.deepEqual(own.result, { echo: "hi" });
  assert.deepEqual(joined.result, own.result, JSON.stringify(joined));
});
