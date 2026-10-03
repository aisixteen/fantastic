// JOINING a caller-scoped kernel verb's execution. Twin of python
// tests/kernel/test_caller_scoped_join.py.
//
// A join re-gates the ORIGINAL verb only (owner, 2026-10-03): a caller the gate
// admits for `ledger` / `ack_deadletter` joins another caller's execution of it
// like any other verb and is handed the recorded outcome; a caller the gate
// refuses for that verb is refused.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Item, Outcome } from "../src/kernel/outcome.ts";
import { sendAs } from "./kit.ts";

const ROOT = "kernel_state";

/** `A` and `B`, children of the root granted its caller-scoped verbs (and `ps`)
 *  on both ends; `C` reaches only the `desk`, which all three may call. */
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
  k.setRoot(
    new Agent({ id: ROOT, meta: { callable_by: { A: ["ledger", "ack_deadletter", "ps"], B: ["ledger", "ack_deadletter", "ps"] } } }),
  );
  k.register(
    new Agent({
      id: "desk",
      parentId: ROOT,
      handlerModule: "scoped_join_probe.ts",
      meta: { callable_by: { A: "*", B: "*", C: "*" } },
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
  k.register(new Agent({ id: "C", parentId: ROOT, meta: { can_call: { desk: "*" } } }));
  return k;
}

async function send(k: Kernel, item: Item, who?: string): Promise<Outcome> {
  return (await (who === undefined ? k.send([item]) : sendAs(k, who, [item])))[0]!;
}

/** A NAMED call: the ledger keeps a named execution's result body. */
const work = (): Item => ({ target: "desk", payload: { type: "work" }, id: "work-a" });
const ledger = (id?: string | number): Item =>
  id === undefined ? { target: "kernel", payload: { type: "ledger" } } : { target: "kernel", payload: { type: "ledger" }, id };

test("a join of another caller's ledger read re-gates the verb and hands back the recorded outcome", async () => {
  const k = kernel();
  await send(k, work(), "A");
  const own = await send(k, ledger("led-A"), "A");
  assert.equal(own.status, "completed", JSON.stringify(own));
  assert.ok(JSON.stringify(own.result).includes("result-of-a"), JSON.stringify(own.result));
  const byName = await send(k, ledger("led-A"), "B");
  assert.deepEqual(byName.result, own.result);
  assert.equal(byName.pid, own.pid);
  // C reaches the desk but holds no `ledger` grant: the join is refused on the verb
  const byC = await send(k, ledger("led-A"), "C");
  assert.equal(byC.status, "failed");
  assert.equal((byC.result as { side?: string }).side, "egress", JSON.stringify(byC.result));
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
