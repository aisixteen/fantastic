// THE CALL GATE — two-sided, from above (+ #39's read side).
//
// Deny-all is the default posture. An operator send is the origin root of trust
// and passes ungated; every agent-originated send needs BOTH the sender's
// `can_call` and the target's `callable_by`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Payload } from "../src/kernel/json.ts";
import { PRIVILEGED_VERBS, authorizeCall } from "../src/kernel/authz.ts";

function tree(): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  k.registerBundle("probe.tools", () => ({ ok: true }));
  // Two SIBLINGS under the root: neither is the other's parent or child, so
  // the structural baseline gives them no edge to each other.
  for (const id of ["alice", "bob"]) {
    k.register(new Agent({ id, parentId: "kernel_state", handlerModule: "probe.tools" }));
  }
  return k;
}

test("an operator send is ungated — it is the origin root of trust", async () => {
  const k = tree();
  const [out] = await k.send([{ target: "bob", payload: { type: "poke" } }]);
  assert.equal(out!.status, "completed");
});

test("deny-all: one sibling cannot reach another without a grant", async () => {
  const k = tree();
  const alice = k.scopedFor("alice", null);
  const [out] = await alice.send([{ target: "bob", payload: { type: "poke" } }]);
  assert.equal(out!.status, "failed");
  assert.equal((out!.result as Payload)["reason"], "unauthorized");
  assert.equal((out!.result as Payload)["side"], "egress");
  assert.match(String((out!.result as Payload)["hint"]), /granted from ABOVE/);
});

test("BOTH ends must grant: egress alone is not enough", async () => {
  const k = tree();
  k.get("alice")!.meta["can_call"] = { bob: "*" };
  const alice = k.scopedFor("alice", null);
  const [out] = await alice.send([{ target: "bob", payload: { type: "poke" } }]);
  assert.equal(out!.status, "failed");
  assert.equal(
    (out!.result as Payload)["side"],
    "ingress",
    "egress was granted, so the refusal must come from the callee",
  );

  // Now the callee accepts, and the edge opens.
  k.get("bob")!.meta["callable_by"] = { alice: "*" };
  const [ok] = await alice.send([{ target: "bob", payload: { type: "poke" } }]);
  assert.equal(ok!.status, "completed");
});

test("nothing is exempt — even `reflect` needs the edge", async () => {
  const k = tree();
  const alice = k.scopedFor("alice", null);
  const [out] = await alice.send([{ target: "bob", payload: { type: "reflect" } }]);
  assert.equal((out!.result as Payload)["reason"], "unauthorized");
});

test("the structural baseline covers your own creator and your own subtree", async () => {
  const k = tree();
  // alice → root is $parent on her side, $children on the root's side.
  const alice = k.scopedFor("alice", null);
  const [up] = await alice.send([{ target: "kernel_state", payload: { type: "reflect" } }]);
  assert.equal(up!.status, "completed", JSON.stringify(up!.result));
});

test("a wildcard NEVER covers a privileged verb — only an explicit name does", () => {
  const ctx = {
    holderId: "alice",
    holderParent: "kernel_state",
    holderChildren: new Set<string>(),
  };
  for (const verb of PRIVILEGED_VERBS) {
    assert.equal(
      authorizeCall({ bob: "*" }, verb, "bob", ctx),
      false,
      `wildcard covered privileged verb '${verb}' — a broad $parent:"*" would silently become "mutate the graph"`,
    );
    assert.equal(authorizeCall({ bob: [verb] }, verb, "bob", ctx), true);
  }
  // Acting on your OWN subtree is baseline, not escalation.
  assert.equal(
    authorizeCall({ $self: "*" }, "update_agent", "alice", ctx),
    true,
    "an agent could not update itself",
  );
});

test("an unknown $token matches nothing rather than everything", () => {
  const ctx = {
    holderId: "alice",
    holderParent: "kernel_state",
    holderChildren: new Set(["kid"]),
  };
  assert.equal(authorizeCall({ $everyone: "*" }, "poke", "bob", ctx), false);
  assert.equal(authorizeCall({ $children: "*" }, "poke", "kid", ctx), true);
});

test("#39 — watch is GATED: an agent cannot mirror an inbox it may not call", () => {
  const k = tree();
  // A watcher receives the target's raw inbound payloads verbatim, so an
  // ungated watch reads everything sent to `bob` without ever calling him.
  const alice = k.scopedFor("alice", null);
  const denied = alice.watch("bob", "alice");
  assert.notEqual(denied, null, "watch was ungated — a read side-channel around callable_by");
  assert.equal((denied as Payload)["reason"], "unauthorized");
  assert.equal((denied as Payload)["verb"], "watch");

  // With the edge open both ways, the watch is allowed.
  k.get("alice")!.meta["can_call"] = { bob: ["watch"] };
  k.get("bob")!.meta["callable_by"] = { alice: ["watch"] };
  assert.equal(alice.watch("bob", "alice"), null);
});

test("an operator watch stays ungated, as with send", () => {
  const k = tree();
  assert.equal(k.watch("bob", "ui"), null);
});

test("an untrusted iframe's reach is its AGENT's, not the operator's", async () => {
  // The sandbox makes the iframe null-origin in the BROWSER. Relaying its sends
  // as the operator would have made that cosmetic: unlimited authority in the
  // kernel. The relay sends as the agent, so the same two-sided gate applies.
  const k = tree();
  const panel = k.scopedFor("alice", null); // what html_view relays through
  const [out] = await panel.send([{ target: "bob", payload: { type: "poke" } }]);
  assert.equal((out!.result as Payload)["reason"], "unauthorized");

  // Its own creator stays reachable via the structural baseline, so the normal
  // panel→host-peer flow needs no ceremony.
  const [up] = await panel.send([{ target: "kernel_state", payload: { type: "reflect" } }]);
  assert.equal(up!.status, "completed");
});
