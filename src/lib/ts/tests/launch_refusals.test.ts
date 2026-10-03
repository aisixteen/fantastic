// EVERY LAUNCH REFUSAL, WHOLE — the outcome a refused batch item settles with,
// pinned field for field: `{pid: null, status: "failed", result}` (no `attempts`:
// nothing ran) and the refusal's `error` text, `reason` and `arg` — and NOTHING
// else (an empty `hint` is dropped, never emitted as ""). Also the ORDER in which
// an item's own fields are checked: an item with several faults reports the
// first one in python's order.
//
// Every expectation below was measured on python's `Agent._launch`
// (kernel/_agent.py) with the same items on 2026-10-01 and is byte-identical.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import type { Outcome } from "../src/kernel/outcome.ts";
import { strictLoads } from "../src/kernel/pyjson.ts";

function kernel(): Kernel {
  const k = new Kernel();
  k.registerBundle("lr.ts", () => ({ ok: true }));
  k.setRoot(new Agent({ id: "kernel_state" }));
  k.register(new Agent({ id: "a", parentId: "kernel_state", handlerModule: "lr.ts" }));
  k.register(new Agent({ id: "b", parentId: "kernel_state", handlerModule: "lr.ts" }));
  return k;
}

async function one(k: Kernel, item: unknown): Promise<Outcome> {
  return (await k.send([item as never]))[0]!;
}

const failed = (result: Payload): Outcome => ({ pid: null, status: "failed", result });
const badArg = (arg: string, error: string): Outcome => failed({ error, reason: "bad_arg", arg });
const missingArg = (arg: string): Outcome => failed({ error: `send: ${arg} required`, reason: "missing_arg", arg });

test("every item-shape refusal: the whole outcome, python's text, reason and arg", async () => {
  const k = kernel();
  const go = { type: "go" };
  const cases: [unknown, Outcome][] = [
    [7, badArg("item", "send: item must be a dict")],
    [[], badArg("item", "send: item must be a dict")],
    [null, badArg("item", "send: item must be a dict")],
    ["x", badArg("item", "send: item must be a dict")],
    [{ payload: go }, missingArg("target")],
    [{ target: "", payload: go }, missingArg("target")],
    [{ target: 5, payload: go }, missingArg("target")],
    [{ target: "a" }, missingArg("payload")],
    [{ target: "a", payload: [] }, missingArg("payload")],
    [{ target: "a", payload: "go" }, missingArg("payload")],
    [{ target: "a", payload: go, timeout: -1 }, badArg("timeout", "send: timeout must be a positive number of seconds, got -1")],
    [{ target: "a", payload: go, timeout: 0 }, badArg("timeout", "send: timeout must be a positive number of seconds, got 0")],
    [{ target: "a", payload: go, timeout: true }, badArg("timeout", "send: timeout must be a positive number of seconds, got True")],
    [{ target: "a", payload: go, timeout: "1" }, badArg("timeout", "send: timeout must be a positive number of seconds, got '1'")],
    [{ target: "a", payload: go, timeout: [1] }, badArg("timeout", "send: timeout must be a positive number of seconds, got [1]")],
    [{ target: "a", payload: {} }, missingArg("payload.type")],
    [{ target: "a", payload: { x: 1 } }, missingArg("payload.type")],
    [{ target: "a", payload: { type: "" } }, badArg("payload.type", "send: payload.type must be a non-empty string, got ''")],
    // a non-string is named by its python TYPE, never echoed (W-11)
    [{ target: "a", payload: { type: null } }, badArg("payload.type", "send: payload.type must be a non-empty string, got NoneType")],
    [{ target: "a", payload: { type: ["go"] } }, badArg("payload.type", "send: payload.type must be a non-empty string, got list")],
    [{ target: "a", payload: { type: 5 } }, badArg("payload.type", "send: payload.type must be a non-empty string, got int")],
    [{ target: "a", payload: { type: 1.5 } }, badArg("payload.type", "send: payload.type must be a non-empty string, got float")],
    [{ target: "a", payload: strictLoads('{"type": 5.0}') }, badArg("payload.type", "send: payload.type must be a non-empty string, got float")],
    [{ target: "a", payload: { type: true } }, badArg("payload.type", "send: payload.type must be a non-empty string, got bool")],
    [{ target: "a", payload: { type: { v: 1 } } }, badArg("payload.type", "send: payload.type must be a non-empty string, got dict")],
    [{ target: "a", payload: { type: new Uint8Array(4096) } }, badArg("payload.type", "send: payload.type must be a non-empty string, got bytes")],
    [{ target: "a", payload: go, id: 1.5 }, badArg("id", "send: id must be a name (non-empty string) or a message id (int), got 1.5")],
    [{ target: "a", payload: go, id: true }, badArg("id", "send: id must be a name (non-empty string) or a message id (int), got True")],
    [{ target: "a", payload: go, id: [] }, badArg("id", "send: id must be a name (non-empty string) or a message id (int), got []")],
    [{ target: "a", payload: go, id: { n: 1 } }, badArg("id", "send: id must be a name (non-empty string) or a message id (int), got {'n': 1}")],
    [
      { target: "a", payload: go, id: 99999 },
      badArg("id", "send: id 99999 names no known execution — ids are minted by the kernel and can be joined, never created"),
    ],
    [{ target: "a", payload: go, id: "" }, badArg("id", "send: id must be a non-empty name")],
    [{ target: "a", payload: go, retries: -1 }, badArg("retries", "send: retries must be a non-negative int, got -1")],
    [{ target: "a", payload: go, retries: 1.5 }, badArg("retries", "send: retries must be a non-negative int, got 1.5")],
    [{ target: "a", payload: go, retries: "2" }, badArg("retries", "send: retries must be a non-negative int, got '2'")],
    [{ target: "a", payload: go, retries: true }, badArg("retries", "send: retries must be a non-negative int, got True")],
  ];
  for (const [item, want] of cases) {
    const out = await one(k, item);
    assert.deepEqual(out, want, JSON.stringify(item));
    // the key order of the refusal itself: error, reason, then the arg
    assert.deepEqual(Object.keys(out.result as Payload), ["error", "reason", "arg"], JSON.stringify(item));
  }
  assert.equal(k.tasks.size(), 0, "no refused item reached the task table");
  assert.equal(k.ledger.size(), 0, "no refused item opened an execution");
  assert.equal(k.store.rows.size, 0, "no refused item wrote a call row");
});

test("an item with several faults reports the first in python's check order", async () => {
  const k = kernel();
  const cases: [unknown, Outcome][] = [
    // target, then payload, then timeout, then payload.type, then id, then
    // retries — and every one of them before the target is resolved
    [{ target: "", payload: null, timeout: -1 }, missingArg("target")],
    [{ target: "a", payload: {}, timeout: -1 }, badArg("timeout", "send: timeout must be a positive number of seconds, got -1")],
    [{ target: "a", payload: { type: 5 }, id: 1.5 }, badArg("payload.type", "send: payload.type must be a non-empty string, got int")],
    [{ target: "a", payload: { type: "go" }, id: 1.5, retries: -1 }, badArg("id", "send: id must be a name (non-empty string) or a message id (int), got 1.5")],
    [{ target: "a", payload: { type: "go" }, retries: -1 }, badArg("retries", "send: retries must be a non-negative int, got -1")],
    [{ target: "nobody", payload: { type: "go" }, retries: -1 }, badArg("retries", "send: retries must be a non-negative int, got -1")],
  ];
  for (const [item, want] of cases) assert.deepEqual(await one(k, item), want, JSON.stringify(item));
});

test("an id bound to another target: python's text, and the first execution untouched", async () => {
  const k = kernel();
  const first = await one(k, { target: "a", payload: { type: "go" }, id: "job" });
  assert.equal(first.status, "completed");
  const out = await one(k, { target: "b", payload: { type: "go" }, id: "job" });
  assert.deepEqual(
    out,
    badArg(
      "id",
      "send: id 'job' already names an execution on 'a', not 'b' — an id is bound to its first execution; a retry aimed elsewhere needs a new id",
    ),
  );
  assert.equal(k.ledger.resolveName("job"), first.pid);
});

test("the draining refusal is {error, reason} — no arg, no hint — and costs no id", async () => {
  const k = kernel();
  const before = k.store.nextId;
  k.store.draining = true;
  const out = await one(k, { target: "a", payload: { type: "go" } });
  assert.deepEqual(out, failed({
    error: "send: the kernel is draining for shutdown — no new work is accepted; in-flight work may finish",
    reason: "draining",
  }));
  assert.deepEqual(Object.keys(out.result as Payload), ["error", "reason"]);
  assert.equal(k.store.nextId, before);
});

test("an unknown target with no bridge: the unknown_agent outcome, whole", async () => {
  const k = kernel();
  assert.deepEqual(await one(k, { target: "nobody", payload: { type: "go" } }), failed({
    error: "no agent 'nobody'",
    reason: "unknown_agent",
    agent_count: 3,
    agents: ["a", "b", "kernel_state"] as Json,
  }));
});


test("send of anything but a LIST throws python's TypeError — before any item is looked at", async () => {
  const k = kernel();
  for (const bad of ["x", { target: "a", payload: { type: "go" } }, null, 5]) {
    await assert.rejects(k.send(bad as never), (e: unknown) => {
      assert.ok(e instanceof TypeError, String(e));
      assert.equal((e as Error).message, "send takes a LIST of {target, payload, timeout?} items");
      return true;
    });
  }
  assert.equal(k.store.nextId, 1, "nothing was minted");
});
