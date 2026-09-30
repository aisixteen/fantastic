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

test("a STRUCTURAL wildcard never covers a privileged verb — a name or a named id's `*` does", () => {
  const ctx = {
    holderId: "alice",
    holderParent: "kernel_state",
    holderChildren: new Set<string>(),
  };
  for (const verb of PRIVILEGED_VERBS) {
    assert.equal(
      authorizeCall({ $parent: "*" }, verb, "kernel_state", ctx),
      false,
      `wildcard covered privileged verb '${verb}' — a broad $parent:"*" would silently become "mutate the graph"`,
    );
    assert.equal(authorizeCall({ $parent: [verb] }, verb, "kernel_state", ctx), true);
    assert.equal(authorizeCall({ bob: "*" }, verb, "bob", ctx), true, "a named id's `*` is the operator's own statement");
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

test("joining an execution is gated on the verb THAT execution ran", async () => {
  // Measured in python 2026-08-28: the gate was checked against the JOINER's own
  // verb, then the JOIN handed back the ORIGINAL call's outcome — so a caller
  // granted only a harmless verb reused another's id and received its secret
  // verbatim. Joining is equivalent to having made that call, so that is the
  // question the gate must be asked. Reachable from the wire, where a frame id
  // IS the execution id (#38) and two legs can name one id.
  const k = tree();
  k.registerBundle("vault.tools", (_id, payload) =>
    payload["type"] === "steal" ? { secret: "TOP-SECRET" } : { ok: true },
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

  const alice = k.scopedFor("alice", null);
  const [a] = await alice.send([
    { target: "vault", payload: { type: "steal" }, id: "job1" },
  ]);
  assert.equal((a!.result as Payload)["secret"], "TOP-SECRET");

  // bob may call `harmless` — and reuses the id.
  const bob = k.scopedFor("bob", null);
  const [b] = await bob.send([
    { target: "vault", payload: { type: "harmless" }, id: "job1" },
  ]);
  assert.equal(b!.status, "failed", "bob joined an execution it may not run");
  assert.equal((b!.result as Payload)["reason"], "unauthorized");
  assert.notEqual((b!.result as Payload)["secret"], "TOP-SECRET");

  // An AUTHORIZED re-sender still dedups — the fix must not break ensure-semantics.
  const [again] = await alice.send([
    { target: "vault", payload: { type: "steal" }, id: "job1" },
  ]);
  assert.equal((again!.result as Payload)["secret"], "TOP-SECRET");
});

// ── python `_gate` / rust `gate.rs` parity (found by the contract corpus) ────

test("a KERNEL verb at the root is gated on egress only — the root is not what is acted upon", async () => {
  // A GRANDCHILD of the root: the root's baseline `$children` does not name it,
  // so an ingress check would refuse it. For `ps`/`cancel` the operand rides in
  // the payload, so python and rust check egress alone (lifecycle.yaml's warden).
  const k = tree();
  k.registerBundle("leaf.tools", () => ({ ok: true }));
  k.register(new Agent({ id: "warden", parentId: "alice", handlerModule: "leaf.tools", meta: { can_call: { kernel_state: ["ps", "cancel"] } } }));
  const warden = k.scopedFor("warden", null);
  const [ps] = await warden.send([{ target: "kernel", payload: { type: "ps" } }]);
  assert.equal(ps!.status, "completed", JSON.stringify(ps!.result));
  // egress still decides: a kernel verb the grant does not name is refused
  const [led] = await warden.send([{ target: "kernel", payload: { type: "ledger" } }]);
  assert.equal((led!.result as Payload)["side"], "egress");
  // and a NON-kernel verb at the root still needs the root to accept the caller
  k.get("warden")!.meta["can_call"] = { kernel_state: "*" };
  const [poke] = await warden.send([{ target: "kernel", payload: { type: "poke" } }]);
  assert.equal((poke!.result as Payload)["side"], "ingress");
});

test("`$loader` names the persistence root — this kernel's root", () => {
  const ctx = { holderId: "a", holderParent: "p", holderChildren: new Set<string>(), loaderId: "kernel_state" };
  assert.equal(authorizeCall({ $loader: ["persist_blob"] }, "persist_blob", "kernel_state", ctx), true);
  assert.equal(authorizeCall({ $loader: ["persist_blob"] }, "persist_blob", "p", ctx), false);
  assert.equal(authorizeCall({ $loader: "*" }, "x", "kernel_state", { ...ctx, loaderId: null }), false);
});

test("a malformed grant is DENY, never a character walk", () => {
  const ctx = { holderId: "a", holderParent: "p", holderChildren: new Set<string>() };
  assert.equal(authorizeCall("*" as unknown as Payload, "poke", "0", ctx), false);
  assert.equal(authorizeCall(["*"] as unknown as Payload, "poke", "0", ctx), false);
});

test("a file_bridge's exposure is privileged: a wildcard never carries allow / disallow", () => {
  const ctx = { holderId: "a", holderParent: "p", holderChildren: new Set<string>() };
  for (const verb of ["allow", "disallow"]) {
    assert.ok(PRIVILEGED_VERBS.has(verb));
    assert.equal(authorizeCall({ $parent: "*" }, verb, "p", ctx), false);
    assert.equal(authorizeCall({ $parent: [verb] }, verb, "p", ctx), true);
  }
});

test("a sender that is NOT an agent of this kernel holds no grant — refused at egress", async () => {
  // python `_gate`: an unknown sender's record reads as `{id}` (empty can_call),
  // so EVERY send is refused at egress — even where the target's callable_by
  // names it, even a kernel verb, even reflect. Measured on the python kernel:
  // ghost → kernel_state reflect / kernel ps / `open` reflect all `side: egress`.
  const k = tree();
  k.get("bob")!.meta["callable_by"] = { ghost: "*" };
  const ghost = k.scopedFor("ghost", null);
  for (const [target, verb] of [["kernel_state", "reflect"], ["kernel", "ps"], ["bob", "reflect"], ["bob", "poke"]] as const) {
    const [out] = await ghost.send([{ target, payload: { type: verb } }]);
    assert.equal(out!.status, "failed", `${target} ${verb}`);
    assert.equal((out!.result as Payload)["reason"], "unauthorized");
    assert.equal((out!.result as Payload)["side"], "egress", `${target} ${verb}`);
  }
  // an agent that LEAVES the tree loses its reach with its record
  const alice = k.scopedFor("alice", null);
  assert.equal((await alice.send([{ target: "kernel", payload: { type: "reflect" } }]))[0]!.status, "completed");
  k.remove("alice");
  const [gone] = await alice.send([{ target: "kernel", payload: { type: "reflect" } }]);
  assert.equal((gone!.result as Payload)["side"], "egress", "a removed agent's in-flight view is a stranger now");
  // the operator is still the root of trust
  const [op] = await k.send([{ target: "bob", payload: { type: "poke" } }]);
  assert.equal(op!.status, "completed");
});

test("a watch by a watcher that is not an agent here is refused the same way", () => {
  const k = tree();
  const d = k.scopedFor("ghost", null).watch("bob", "ghost");
  assert.equal(d?.["reason"], "unauthorized");
  assert.equal(d?.["side"], "egress");
});

test("an emit onto ANOTHER agent needs both halves; your own inbox stays ungated (a16)", () => {
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
  k.onInbox("em_target", (p) => seen.push(p));
  k.scopedFor("em_closed", null).emit("em_target", { type: "poke", n: 1 });
  assert.deepEqual(seen, [], "an emit with no egress grant must be dropped");
  k.scopedFor("em_open", null).emit("em_target", { type: "poke", n: 2 });
  assert.deepEqual(seen, [{ type: "poke", n: 2 }]);
  k.scopedFor("em_target", null).emit("em_target", { type: "self", n: 3 });
  assert.equal(seen.length, 2, "an agent's emit on its own inbox is ungated");
});

// ── `$subtree:<id>` ingress + HERITABLE egress (owner, 2026-09-29) ─────────
//
// Agents spawned dynamically under one owner (a web leg's terminals and AIs)
// reach a target with NO per-agent name on either side: the target accepts
// `"$subtree:hub"`, the owner carries a heritable `can_call` entry that covers
// its descendants — never itself.

function subtreeTree(): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  k.registerBundle("probe.tools", () => ({ ok: true }));
  k.register(
    new Agent({
      id: "served",
      parentId: "kernel_state",
      handlerModule: "probe.tools",
      meta: { callable_by: { "$subtree:hub": ["list", "allow", "allowed"], "$subtree:ghost": "*" } },
    }),
  );
  k.register(
    new Agent({
      id: "hub",
      parentId: "kernel_state",
      handlerModule: "probe.tools",
      meta: { can_call: { served: { verbs: ["list", "allow", "allowed"], for: "descendants" } } },
    }),
  );
  k.register(new Agent({ id: "kid", parentId: "hub", handlerModule: "probe.tools" }));
  k.register(
    new Agent({
      id: "outsider",
      parentId: "kernel_state",
      handlerModule: "probe.tools",
      meta: { can_call: { served: ["list"] } },
    }),
  );
  return k;
}

async function sendAs(k: Kernel, from: string, type: string): Promise<Payload> {
  const [out] = await k.scopedFor(from, null).send([{ target: "served", payload: { type } }]);
  return out!.status === "completed" ? { ok: true } : (out!.result as Payload);
}

test("$subtree + heritable: a descendant of the owner reaches the target", async () => {
  const k = subtreeTree();
  assert.deepEqual(await sendAs(k, "kid", "list"), { ok: true });
});

test("heritable: the HOLDER of the entry is not covered by it", async () => {
  const k = subtreeTree();
  const r = await sendAs(k, "hub", "list");
  assert.equal(r["side"], "egress", JSON.stringify(r));
});

test("$subtree: an agent outside the subtree is refused by the target", async () => {
  const k = subtreeTree();
  const r = await sendAs(k, "outsider", "list");
  assert.equal(r["side"], "ingress", JSON.stringify(r));
});

test("a NAMED privileged verb is covered by both forms", async () => {
  const k = subtreeTree();
  assert.deepEqual(await sendAs(k, "kid", "allow"), { ok: true });
  // a verb the heritable entry does not name dies at egress
  assert.equal((await sendAs(k, "kid", "mkdir"))["side"], "egress");
});

test("`*` in a heritable entry never covers a privileged verb", async () => {
  const k = subtreeTree();
  k.updateMeta("hub", { can_call: { served: { verbs: ["*"], for: "descendants" } } });
  assert.deepEqual(await sendAs(k, "kid", "list"), { ok: true });
  assert.equal((await sendAs(k, "kid", "allow"))["side"], "egress");
});

test("$subtree is resolved at CALL time: a grandchild registered later is covered", async () => {
  const k = subtreeTree();
  k.register(new Agent({ id: "grandkid", parentId: "kid", handlerModule: "probe.tools" }));
  assert.deepEqual(await sendAs(k, "grandkid", "list"), { ok: true });
});

test("$subtree never matches a foreign id, an absent root, or an empty id", () => {
  const k = subtreeTree();
  const ctx = {
    holderId: "served",
    holderParent: "kernel_state",
    holderChildren: new Set<string>(),
    enforcePrivileged: false,
    inSubtree: (o: string, x: string) => o === x || (o === "kid" && x === "hub"),
  };
  // a foreign caller that happens to carry the owner's id after the `|`
  assert.equal(authorizeCall({ "$subtree:hub": "*" }, "list", "g1|hub", ctx), false);
  assert.equal(authorizeCall({ "$subtree:": "*" }, "list", "kid", ctx), false);
  // an id that is not in the tree: `$subtree:ghost` on `served` matches nothing
  assert.equal(k.gate("outsider", "list", "served")?.["side"], "ingress");
  // and on EGRESS the key matches nothing (callable_by-only: no resolver there)
  assert.equal(
    authorizeCall({ "$subtree:hub": "*" }, "list", "kid", { holderId: "x", holderParent: null, holderChildren: new Set() }),
    false,
  );
});

test("a NAMED id's `*` covers a privileged verb; a structural token's `*` does not (a12)", () => {
  const ctx = { holderId: "a", holderParent: "p", holderChildren: new Set<string>() };
  assert.equal(authorizeCall({ files: "*" }, "allow", "files", ctx), true);
  assert.equal(authorizeCall({ files: ["*"] }, "disallow", "files", ctx), true);
  assert.equal(authorizeCall({ $parent: "*" }, "allow", "p", ctx), false);
});

test("`<guid>|*` names every agent of ONE other kernel — never a local id, never another kernel", () => {
  const ctx = { holderId: "canvas", holderParent: "root", holderChildren: new Set<string>(), enforcePrivileged: false };
  const grant = { "h0|*": ["around"] };
  assert.equal(authorizeCall(grant, "around", "h0|cell_7", ctx), true);
  assert.equal(authorizeCall(grant, "around", "h1|cell_7", ctx), false);
  assert.equal(authorizeCall(grant, "around", "cell_7", ctx), false);
  assert.equal(authorizeCall(grant, "clear", "h0|cell_7", ctx), false);
});
