// THE CALL GATE — two-sided, explicit grants only (+ #39's read side).
//
// The baseline is `{$self: "*"}` on both fields and nothing else. An operator
// send is the origin root of trust and passes ungated; every agent-originated
// send needs BOTH the sender's `can_call` and the target's `callable_by`, and
// `"*"` covers every verb.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Payload } from "../src/kernel/json.ts";
import { DEFAULT_GRANT, FROM_ABOVE, authorizeCall } from "../src/kernel/authz.ts";
import { AS_VERB, as, sendAs } from "./kit.ts";

function tree(): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  k.registerBundle("probe.tools", () => ({ ok: true }));
  // Two agents under the root: the baseline gives them no edge to each other,
  // nor to the root.
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

test("identity comes only from dispatch — the kernel has no API to send as anyone", () => {
  const k = tree();
  for (const gone of ["scopedFor", "foreignView", "operatorView", "kernelView", "taskView", "isForeign", "qualifiedCaller", "base"]) {
    assert.equal(gone in k, false, gone);
  }
});

test("deny-all: one sibling cannot reach another without a grant", async () => {
  const k = tree();
  const [out] = await sendAs(k, "alice", [{ target: "bob", payload: { type: "poke" } }]);
  assert.equal(out!.status, "failed");
  assert.equal((out!.result as Payload)["reason"], "unauthorized");
  assert.equal((out!.result as Payload)["side"], "egress");
  assert.equal((out!.result as Payload)["hint"], FROM_ABOVE);
  assert.equal(
    FROM_ABOVE,
    "grants come from above — an ancestor (update_agent on an agent in its subtree) or the operator writes can_call and callable_by; an agent never raises its own",
  );
});

test("BOTH ends must grant: egress alone is not enough", async () => {
  const k = tree();
  k.get("alice")!.meta["can_call"] = { bob: "*" };
  const [out] = await sendAs(k, "alice", [{ target: "bob", payload: { type: "poke" } }]);
  assert.equal(out!.status, "failed");
  assert.equal(
    (out!.result as Payload)["side"],
    "ingress",
    "egress was granted, so the refusal must come from the callee",
  );

  // Now the callee accepts, and the edge opens.
  k.get("bob")!.meta["callable_by"] = { alice: "*" };
  const [ok] = await sendAs(k, "alice", [{ target: "bob", payload: { type: "poke" } }]);
  assert.equal(ok!.status, "completed");
});

test("nothing is exempt — even `reflect` needs the edge", async () => {
  const k = tree();
  const [out] = await sendAs(k, "alice", [{ target: "bob", payload: { type: "reflect" } }]);
  assert.equal((out!.result as Payload)["reason"], "unauthorized");
});

test("the baseline is {$self: \"*\"} only — not even your creator is reachable without a grant", async () => {
  const k = tree();
  assert.deepEqual(DEFAULT_GRANT, { $self: "*" });
  assert.deepEqual(k.get("alice")!.meta["can_call"], { $self: "*" });
  assert.deepEqual(k.get("alice")!.meta["callable_by"], { $self: "*" });
  const alice = { send: (items: Parameters<Kernel["send"]>[0]) => sendAs(k, "alice", items) };
  const [up] = await alice.send([{ target: "kernel_state", payload: { type: "reflect" } }]);
  assert.equal((up!.result as Payload)["side"], "egress", JSON.stringify(up!.result));
  // $parent on alice's side, $subtree on the root's: the edge opens.
  k.get("alice")!.meta["can_call"] = { $self: "*", $parent: ["reflect"] };
  const [half] = await alice.send([{ target: "kernel_state", payload: { type: "reflect" } }]);
  assert.equal((half!.result as Payload)["side"], "ingress", JSON.stringify(half!.result));
  k.get("kernel_state")!.meta["callable_by"] = { $self: "*", $subtree: ["reflect"] };
  const [ok] = await alice.send([{ target: "kernel_state", payload: { type: "reflect" } }]);
  assert.equal(ok!.status, "completed", JSON.stringify(ok!.result));
  // $self covers every verb on yourself
  const [self] = await alice.send([{ target: "alice", payload: { type: "poke" } }]);
  assert.equal(self!.status, "completed", JSON.stringify(self!.result));
});

test("`*` covers EVERY verb — create_agent and the kernel verbs included; a list covers what it names", () => {
  const ctx = { holderId: "alice", holderParent: "kernel_state", otherAncestors: new Set(["alice", "kernel_state"]) };
  for (const verb of ["create_agent", "update_agent", "delete_agent", "state_subscribe", "ps", "cancel", "ledger", "stop", "watch", "poke"]) {
    assert.equal(authorizeCall({ $parent: "*" }, verb, "kernel_state", ctx), true, verb);
    assert.equal(authorizeCall({ kernel_state: "*" }, verb, "kernel_state", ctx), true, verb);
    assert.equal(authorizeCall({ $subtree: "*" }, verb, "kid", ctx), true, verb);
    assert.equal(authorizeCall({ $self: "*" }, verb, "alice", ctx), true, verb);
    assert.equal(authorizeCall({ $parent: [verb] }, verb, "kernel_state", ctx), true, verb);
    assert.equal(authorizeCall({ $parent: ["other"] }, verb, "kernel_state", ctx), false, verb);
  }
});

test("an unknown $token matches nothing rather than everything", () => {
  const ctx = {
    holderId: "alice",
    holderParent: "kernel_state",
    otherAncestors: new Set(["alice", "kernel_state"]),
  };
  assert.equal(authorizeCall({ $everyone: "*" }, "poke", "bob", ctx), false);
  assert.equal(authorizeCall({ $subtree: "*" }, "poke", "kid", ctx), true);
  assert.equal(authorizeCall({ $children: "*" }, "poke", "kid", ctx), false);
});

test("#39 — watch is GATED: an agent cannot mirror an inbox it may not call", async () => {
  const k = tree();
  // A watcher receives the target's raw inbound payloads verbatim, so an
  // ungated watch reads everything sent to `bob` without ever calling him.
  const denied = await as(k, "alice", (v) => v.watch("bob", "alice"));
  assert.notEqual(denied, null, "watch was ungated — a read side-channel around callable_by");
  assert.equal((denied as Payload)["reason"], "unauthorized");
  // python `_gate`'s text, verbatim; the verb rides in the text, not a field
  assert.equal((denied as Payload)["error"], "alice may not call 'watch' on bob");
  assert.equal((denied as Payload)["side"], "egress");
  assert.equal("verb" in (denied as Payload), false);

  // With the edge open both ways, the watch is allowed.
  k.get("alice")!.meta["can_call"] = { bob: ["watch"] };
  k.get("bob")!.meta["callable_by"] = { alice: ["watch"] };
  assert.equal(await as(k, "alice", (v) => v.watch("bob", "alice")), null);
});

test("an operator watch stays ungated, as with send", () => {
  const k = tree();
  assert.equal(k.watch("bob", "ui"), null);
});

test("an untrusted iframe's reach is its AGENT's, not the operator's", async () => {
  // The sandbox makes the iframe null-origin in the BROWSER. Relaying its sends
  // as the operator would have made that cosmetic: unlimited authority in the
  // kernel. The relay is a call TO the agent whose handler makes the onward
  // send, so the same two-sided gate applies to it as the agent.
  const k = tree();
  const [out] = await sendAs(k, "alice", [{ target: "bob", payload: { type: "poke" } }]);
  assert.equal((out!.result as Payload)["reason"], "unauthorized");

  // Not even its own creator, without an explicit grant on both ends.
  const [up] = await sendAs(k, "alice", [{ target: "kernel_state", payload: { type: "reflect" } }]);
  assert.equal((up!.result as Payload)["reason"], "unauthorized");
});

test("joining an execution is gated on the verb THAT execution ran", async () => {
  // Gating the JOINER's own verb while the JOIN hands back the ORIGINAL call's
  // outcome would let a caller granted only a harmless verb reuse another's id
  // and receive its result verbatim. Joining is equivalent to having made that call, so that is the
  // question the gate must be asked. Reachable from the wire, where a frame id
  // IS the execution id and two legs can name one id.
  const k = tree();
  k.registerBundle("vault.tools", (_id, payload) =>
    payload["type"] === "steal" ? { gold: "ALL OF IT" } : { ok: true },
  );
  k.register(
    new Agent({
      id: "vault",
      parentId: "kernel_state",
      handlerModule: "vault.tools",
      meta: { callable_by: { alice: ["steal"], bob: ["harmless"] } },
    }),
  );
  k.get("alice")!.meta["can_call"] = { vault: ["steal"] };
  k.get("bob")!.meta["can_call"] = { vault: ["harmless"] };

  const [a] = await sendAs(k, "alice", [
    { target: "vault", payload: { type: "steal" }, id: "job1" },
  ]);
  assert.equal((a!.result as Payload)["gold"], "ALL OF IT");

  // bob may call `harmless` — and reuses the id.
  const [b] = await sendAs(k, "bob", [
    { target: "vault", payload: { type: "harmless" }, id: "job1" },
  ]);
  assert.equal(b!.status, "failed", "bob joined an execution it may not run");
  assert.equal((b!.result as Payload)["reason"], "unauthorized");
  assert.notEqual((b!.result as Payload)["gold"], "ALL OF IT");

  // An AUTHORIZED re-sender still dedups — the fix must not break ensure-semantics.
  const [again] = await sendAs(k, "alice", [
    { target: "vault", payload: { type: "steal" }, id: "job1" },
  ]);
  assert.equal((again!.result as Payload)["gold"], "ALL OF IT");
});


// ── python `_gate` / rust `gate.rs` parity (found by the contract corpus) ────

test("a KERNEL verb at the root takes the same two-sided check — no ingress skip", async () => {
  const k = tree();
  k.registerBundle("leaf.tools", () => ({ ok: true }));
  k.register(new Agent({ id: "warden", parentId: "alice", handlerModule: "leaf.tools", meta: { can_call: { kernel_state: ["ps", "cancel"] } } }));
  const warden = { send: (items: Parameters<Kernel["send"]>[0]) => sendAs(k, "warden", items) };
  const [ps] = await warden.send([{ target: "kernel", payload: { type: "ps" } }]);
  assert.equal((ps!.result as Payload)["side"], "ingress", JSON.stringify(ps!.result));
  assert.equal((ps!.result as Payload)["error"], "kernel_state does not accept 'ps' from warden");
  k.get("kernel_state")!.meta["callable_by"] = { $self: "*", warden: ["ps"] };
  const [ok] = await warden.send([{ target: "kernel", payload: { type: "ps" } }]);
  assert.equal(ok!.status, "completed", JSON.stringify(ok!.result));
  // egress still decides: a kernel verb the sender's grant does not name is refused
  const [led] = await warden.send([{ target: "kernel", payload: { type: "ledger" } }]);
  assert.equal((led!.result as Payload)["side"], "egress");
});


test("a malformed grant is DENY, never a character walk", () => {
  const ctx = { holderId: "a", holderParent: "p", otherAncestors: new Set<string>() };
  assert.equal(authorizeCall("*" as unknown as Payload, "poke", "0", ctx), false);
  assert.equal(authorizeCall(["*"] as unknown as Payload, "poke", "0", ctx), false);
});

test("an agent that LEAVES the tree loses its reach with its record — sends and watches refused at egress", async () => {
  // python `_gate`: a sender with no record reads as `{id}` (empty can_call), so
  // EVERY send is refused at egress — even where the target's callable_by names
  // it, even a kernel verb, even reflect. A handler still running after its
  // agent was removed is such a sender.
  const k = tree();
  k.get("alice")!.meta["can_call"] = { $self: "*", kernel_state: ["reflect"], bob: "*" };
  k.get("kernel_state")!.meta["callable_by"] = { $self: "*", alice: ["reflect"] };
  k.get("bob")!.meta["callable_by"] = { alice: "*" };
  await as(k, "alice", async (v) => {
    assert.equal((await v.send([{ target: "kernel", payload: { type: "reflect" } }]))[0]!.status, "completed");
    k.remove("alice");
    for (const [target, verb] of [["kernel_state", "reflect"], ["kernel", "ps"], ["bob", "reflect"], ["bob", "poke"]] as const) {
      const [out] = await v.send([{ target, payload: { type: verb } }]);
      assert.equal(out!.status, "failed", `${target} ${verb}`);
      assert.equal((out!.result as Payload)["reason"], "unauthorized");
      assert.equal((out!.result as Payload)["side"], "egress", `${target} ${verb}`);
    }
    const d = v.watch("bob", "alice");
    assert.equal(d?.["reason"], "unauthorized");
    assert.equal(d?.["side"], "egress");
  });
  // the operator is still the root of trust
  const [op] = await k.send([{ target: "bob", payload: { type: "poke" } }]);
  assert.equal(op!.status, "completed");
});

test("an emit onto ANOTHER agent needs both halves; your own inbox stays ungated (a16)", async () => {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  k.register(new Agent({ id: "em_open", parentId: "kernel_state", meta: { can_call: { em_target: ["emit"] } } }));
  k.register(new Agent({ id: "em_closed", parentId: "kernel_state", meta: { can_call: {} } }));
  k.register(
    new Agent({
      id: "em_target",
      parentId: "kernel_state",
      meta: { callable_by: { em_open: ["emit"], em_closed: ["emit"] } },
    }),
  );
  const seen: Payload[] = [];
  k.onInbox("em_target", (p) => {
    if (p["type"] !== AS_VERB) seen.push(p);
  });
  await as(k, "em_closed", (v) => v.emit("em_target", { type: "poke", n: 1 }));
  assert.deepEqual(seen, [], "an emit with no egress grant must be dropped");
  await as(k, "em_open", (v) => v.emit("em_target", { type: "poke", n: 2 }));
  assert.deepEqual(seen, [{ type: "poke", n: 2 }]);
  await as(k, "em_target", (v) => v.emit("em_target", { type: "self", n: 3 }));
  assert.equal(seen.length, 2, "an agent's emit on its own inbox is ungated");
});

test("`<guid>|*` in can_call names every agent of ONE peer kernel — never a local id, never another kernel", () => {
  const ctx = { holderId: "canvas", holderParent: "root", otherAncestors: new Set<string>() };
  const grant = { "h0|*": ["around"] };
  assert.equal(authorizeCall(grant, "around", "h0|cell_7", ctx), true);
  assert.equal(authorizeCall(grant, "around", "h1|cell_7", ctx), false);
  assert.equal(authorizeCall(grant, "around", "cell_7", ctx), false);
  assert.equal(authorizeCall(grant, "clear", "h0|cell_7", ctx), false);
});

test("a caller is always a LOCAL agent: a `<guid>|…` key in callable_by never admits anyone", async () => {
  const k = tree();
  k.get("alice")!.meta["can_call"] = { bob: "*" };
  k.get("bob")!.meta["callable_by"] = { "h0|alice": "*", "h0|*": "*" };
  const [out] = await sendAs(k, "alice", [{ target: "bob", payload: { type: "poke" } }]);
  assert.equal((out!.result as Payload)["side"], "ingress", JSON.stringify(out!.result));
});

// python `_gate`'s denials, verbatim: the verb rides in the TEXT (repr-quoted),
// the keys are {error, reason, hint, side}.
test("denial texts: egress and ingress", async () => {
  const k = tree();
  const [eg] = await sendAs(k, "alice", [{ target: "bob", payload: { type: "poke" } }]);
  const e = eg!.result as Payload;
  assert.deepEqual(Object.keys(e), ["error", "reason", "hint", "side"]);
  assert.equal(e["error"], "alice may not call 'poke' on bob");

  k.get("alice")!.meta["can_call"] = { bob: ["poke"] };
  const [ing] = await sendAs(k, "alice", [{ target: "bob", payload: { type: "poke" } }]);
  const i = ing!.result as Payload;
  assert.deepEqual(Object.keys(i), ["error", "reason", "hint", "side"]);
  assert.equal(i["error"], "bob does not accept 'poke' from alice");
  assert.equal(i["side"], "ingress");
  assert.equal(i["hint"], FROM_ABOVE);
});

test("watching the ROOT is an ordinary two-sided edge — no named-grant rule", async () => {
  const k = tree();
  const alice = { watch: (src: string, w: string) => as(k, "alice", (v) => v.watch(src, w)) };
  const denied = await alice.watch("kernel_state", "alice");
  assert.equal(denied?.["side"], "egress");
  assert.equal(denied?.["error"], "alice may not call 'watch' on kernel_state");
  k.get("alice")!.meta["can_call"] = { $self: "*", $parent: "*" };
  assert.equal((await alice.watch("kernel_state", "alice"))?.["side"], "ingress");
  k.get("kernel_state")!.meta["callable_by"] = { $self: "*", $subtree: "*" };
  assert.equal(await alice.watch("kernel_state", "alice"), null);
});


test("a reflect that passes the gate returns the tree whole — roster and detail:\"full\"", async () => {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  k.registerBundle("probe.tools", () => ({ ok: true }));
  k.register(new Agent({ id: "rgf_parent", parentId: "kernel_state", handlerModule: "probe.tools", meta: { description: "the parent", callable_by: { rgf_reader: ["reflect"] } } }));
  k.register(new Agent({
    id: "rgf_sibling",
    parentId: "rgf_parent",
    handlerModule: "probe.tools",
    meta: { description: "the sibling", can_call: { rgf_peer: "*" } },
  }));
  k.register(new Agent({ id: "rgf_kid", parentId: "rgf_sibling", handlerModule: "probe.tools", meta: { description: "deeper" } }));
  k.register(new Agent({ id: "rgf_reader", parentId: "rgf_parent", handlerModule: "probe.tools", meta: { can_call: { $parent: ["reflect"] } } }));
  const reader = { send: (items: Parameters<Kernel["send"]>[0]) => sendAs(k, "rgf_reader", items) };
  const [direct] = await reader.send([{ target: "rgf_sibling", payload: { type: "reflect" } }]);
  assert.equal((direct!.result as Payload)["reason"], "unauthorized");
  const [r] = await reader.send([{ target: "rgf_parent", payload: { type: "reflect", detail: "full" } }]);
  const full = (r!.result as Payload)["tree"] as Payload;
  const sib = (full["children"] as Payload[]).find((c) => c["id"] === "rgf_sibling")!;
  assert.equal(sib["description"], "the sibling");
  assert.equal((sib["can_call"] as Payload)["rgf_peer"], "*");
  assert.equal(((sib["children"] as Payload[])[0] as Payload)["description"], "deeper");
  const [d] = await reader.send([{ target: "rgf_parent", payload: { type: "reflect" } }]);
  const roster = (d!.result as Payload)["tree"] as Payload;
  const rs = (roster["children"] as Payload[]).find((c) => c["id"] === "rgf_sibling")!;
  assert.equal(rs["description"], "the sibling");
});
