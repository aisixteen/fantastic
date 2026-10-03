// JOINING a caller-scoped kernel verb's execution (owner-approved 2026-10-03,
// audit KV-1). Twin of python tests/kernel/test_caller_scoped_join.py.
//
// `ledger` and `ack_deadletter` answer from WHO sends them — the executions that
// caller originated. A join never re-runs the handler, so before the fix a joiner
// was handed the original caller's view: `B`, granted `ledger`, joined `A`'s named
// ledger read (or the operator's unscoped one, by its int pid) and received A's
// (or every caller's) rows. Now a join of such an execution by any caller other
// than the one that sent it is refused; the original caller still dedups, and
// cross-caller joins of every other verb are unchanged.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Item, Outcome } from "../src/kernel/outcome.ts";

const ROOT = "kernel_state";

/** `A` and `B`, children of the root granted its caller-scoped verbs (and `ps`)
 *  by name — a wildcard never covers a kernel verb — and a `desk` both may call. */
function kernel(): Kernel {
  const k = new Kernel();
  k.registerBundle("scoped_join_probe.ts", async (id, payload) => {
    const verb = payload["type"];
    if (verb === "boot" || verb === "shutdown" || verb === "reflect") {
      return { id, sentence: "scoped join probe", verbs: {} };
    }
    if (verb === "work") return { body: "result-of-a" };
    if (verb === "boom") throw new Error("boom");
    return { error: `unknown verb ${String(verb)}` };
  });
  k.setRoot(new Agent({ id: ROOT }));
  k.register(
    new Agent({
      id: "desk",
      parentId: ROOT,
      handlerModule: "scoped_join_probe.ts",
      meta: { callable_by: { A: "*", B: "*" } },
    }),
  );
  for (const id of ["A", "B"]) {
    k.register(
      new Agent({
        id,
        parentId: ROOT,
        meta: { can_call: { desk: "*", [ROOT]: ["ledger", "ack_deadletter", "ps"] } },
      }),
    );
  }
  return k;
}

async function send(k: Kernel, item: Item, as?: string): Promise<Outcome> {
  const view = as === undefined ? k : k.scopedFor(as, null);
  return (await view.send([item]))[0]!;
}

/** A NAMED call: the ledger keeps a named execution's result body. */
const work = (): Item => ({ target: "desk", payload: { type: "work" }, id: "work-a" });
const ledger = (id?: string | number): Item =>
  id === undefined ? { target: "kernel", payload: { type: "ledger" } } : { target: "kernel", payload: { type: "ledger" }, id };

function refused(execId: string, verb: string) {
  return {
    error:
      `send: id ${execId} names another caller's ${verb} execution — ${verb} answers only the ` +
      `caller that sent it, so it cannot be joined; make your own ${verb} call under a new id`,
    reason: "bad_arg",
    arg: "id",
  };
}

test("a foreign join of a named ledger read is refused", async () => {
  const k = kernel();
  await send(k, work(), "A");
  const own = await send(k, ledger("led-A"), "A");
  assert.equal(own.status, "completed", JSON.stringify(own));
  assert.ok(JSON.stringify(own.result).includes("result-of-a"), JSON.stringify(own.result));

  const byName = await send(k, ledger("led-A"), "B");
  assert.equal(byName.status, "failed");
  assert.deepEqual(byName.result, refused("'led-A'", "ledger"));
  const byPid = await send(k, ledger(own.pid as number), "B");
  assert.deepEqual(byPid.result, refused(String(own.pid), "ledger"));

  // the original caller still dedups: the recorded view
  const again = await send(k, ledger("led-A"), "A");
  assert.deepEqual(again.result, own.result);
  assert.equal(again.pid, own.pid);
});

test("an agent joining the operator's ledger read is refused", async () => {
  const k = kernel();
  await send(k, work(), "A");
  const op = await send(k, ledger());
  assert.ok(JSON.stringify(op.result).includes("result-of-a"), "the operator's read is unscoped");
  const out = await send(k, ledger(op.pid as number), "B");
  assert.deepEqual(out.result, refused(String(op.pid), "ledger"));
});

test("the operator joining an agent's ledger read is refused", async () => {
  const k = kernel();
  await send(k, ledger("led-A"), "A");
  const out = await send(k, ledger("led-A"));
  assert.deepEqual(out.result, refused("'led-A'", "ledger"));
});

test("a foreign join of an ack_deadletter is refused", async () => {
  const k = kernel();
  const dead = await send(k, { target: "desk", payload: { type: "boom" } }, "A");
  assert.equal(dead.status, "failed", JSON.stringify(dead));
  const ack: Item = { target: "kernel", payload: { type: "ack_deadletter", id: dead.pid }, id: "ack-A" };
  const own = await send(k, ack, "A");
  assert.equal((own.result as { acked?: boolean }).acked, true, JSON.stringify(own));
  const out = await send(k, ack, "B");
  assert.deepEqual(out.result, refused("'ack-A'", "ack_deadletter"));
});

test("cross-caller joins of other verbs stay open", async () => {
  // `ps` answers the same table to everyone: B joining A's named `ps` is the
  // once-only contract, unchanged.
  const k = kernel();
  const own = await send(k, { target: "kernel", payload: { type: "ps" }, id: "ps-A" }, "A");
  const joined = await send(k, { target: "kernel", payload: { type: "ps" }, id: "ps-A" }, "B");
  assert.equal(own.status, "completed", JSON.stringify(own));
  assert.deepEqual(joined.result, own.result);
  assert.equal(joined.pid, own.pid);
});
