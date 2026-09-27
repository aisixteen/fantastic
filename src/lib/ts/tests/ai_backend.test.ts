// The BROWSER-RESIDENT AI agent: its turn, not its wire.
//
// The provider is faked here on purpose. What is under test is the thing that
// makes a streaming text endpoint into an AGENT: that the calls it writes as text
// actually reach other agents through the kernel, that their replies come back as
// its next turn, and that its chain ends for the right reasons. The wire itself
// is pinned separately in `tool_parse.test.ts`; the chain contract (done / nudge
// / await) in `ai_turn.test.ts`.

import { test } from "node:test";
import assert from "node:assert/strict";
import type { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import { OpenAIProvider } from "../src/bundles/ai/ai_backend.ts";
import { withoutThinking } from "../src/bundles/ai/think.ts";
import { ASYNC_HOWTO, TITLES_HOWTO, defaultSystemContent } from "../src/bundles/ai/howtos.ts";
import { renderCall } from "../src/bundles/ai/tool_parse.ts";
import type { Value } from "../src/bundles/ai/tool_parse.ts";
import { DONE, Mind, backendWorld, settle } from "./ai_world.ts";

/** backendWorld + a `store` agent the ai may call. */
function world(mind: Mind, handler: (p: Payload) => Json): Kernel {
  const k = backendWorld(mind);
  k.registerBundle("store.tools", (_id, payload) => handler(payload));
  k.register(new Agent({ id: "store", parentId: "kernel_state", handlerModule: "store.tools", meta: { callable_by: { ai: "*" } } }));
  // update_agent REPLACES a map: the baseline is restated with the new edge
  k.updateMeta("ai", { can_call: { $self: "*", $parent: "*", $children: "*", mem: "*", tool: "*", store: "*" } });
  return k;
}

const storeAnswers = (p: Payload): Json => {
  if (p["type"] === "read") return { key: p["key"] ?? null, value: 42 };
  if (p["type"] === "reflect") return { id: "store", sentence: "a store" };
  return { error: "unknown" };
};

const call = (target: string, payload: { [k: string]: Value }, extra: { [k: string]: Value } = {}) =>
  renderCall("send", { target_id: target, payload, ...extra });

async function run(k: Kernel, text: string, extra: Payload = {}): Promise<Payload> {
  const out = (await k.send([{ target: "ai", payload: { type: "send", text, ...extra } }]))[0]!.result as Payload;
  await settle(k);
  return out;
}

test("a call written as TEXT reaches another agent, and its reply is the next turn", async () => {
  const mind = new Mind([call("store", { type: "read", key: "answer" }), "The answer is 42. " + DONE]);
  const k = world(mind, storeAnswers);
  await run(k, "what is the answer?");
  assert.equal(mind.calls, 2);
  // The REPLY lands back in context as the next turn — that is what makes a chain
  // rather than a one-shot completion — in the tag notation, never JSON.
  const reply = mind.incoming[1]!;
  assert.ok(reply.startsWith("[reply to "), reply);
  assert.ok(reply.includes("store read"));
  assert.ok(reply.includes("<value>42</value>"), reply);
});

test("the chain ends when the model says done", async () => {
  const mind = new Mind(["Nothing to do. " + DONE]);
  const k = world(mind, storeAnswers);
  await run(k, "hi");
  assert.equal(mind.calls, 1);
});

test("two calls in one turn both dispatch", async () => {
  const seen: Payload[] = [];
  const mind = new Mind([call("store", { type: "read", key: "a" }) + call("store", { type: "list" })]);
  const k = world(mind, (p) => {
    seen.push(p);
    return storeAnswers(p);
  });
  await run(k, "go");
  assert.deepEqual(seen.map((p) => p["type"]).sort(), ["list", "read"]);
  assert.equal(mind.incoming[1]!.split("\n\n[reply to ").length, 2, "both replies arrive in ONE turn");
});

test("a REFUSAL is fed back as information, not thrown", async () => {
  // The gate is two-sided; an agent that cannot reach a peer must learn that from
  // its own transcript rather than have the chain crash under it.
  const mind = new Mind([call("nobody", { type: "read" }), "ok " + DONE]);
  const k = world(mind, storeAnswers);
  await run(k, "go");
  assert.equal(mind.calls, 2, "the refusal never reached the model");
  assert.ok(mind.incoming[1]!.startsWith("[reply to "));
  assert.ok(mind.incoming[1]!.includes("<error>"), mind.incoming[1]);
});

test("a MALFORMED envelope is named back to the model, and the chain continues", async () => {
  // A model that tried and produced nothing looks identical to one that chose to
  // stop — unless it is told what went wrong.
  const seen: Payload[] = [];
  const mind = new Mind([
    '<call name="send"><type>read</type></call>', // no target
    call("store", { type: "read", key: "a" }),
    "recovered " + DONE,
  ]);
  const k = world(mind, (p) => {
    seen.push(p);
    return storeAnswers(p);
  });
  await run(k, "go");
  assert.ok(mind.incoming[1]!.includes("call NOT dispatched: a send names its target"), mind.incoming[1]);
  assert.equal(seen.length, 1, "it got to make the corrected call");
  assert.equal(mind.calls, 3);
});

test("a NAMED execution is forwarded, so a repeat does not re-run the side effect", async () => {
  // Ensure-semantics is the whole point of naming a call: unknown ⇒ dispatch,
  // settled ⇒ the RECORDED outcome, handler never runs again.
  let dispensed = 0;
  const named = call("store", { type: "dispense" }, { exec_id: "coin-alpha" });
  const mind = new Mind([named, named, DONE]); // the SAME call, twice
  const k = world(mind, () => ({ coin: ++dispensed }));
  await run(k, "dispense one coin");
  assert.equal(dispensed, 1, "the named repeat dispensed a second time");
  assert.ok(mind.incoming[2]!.includes("<coin>1</coin>"), "the repeat got the RECORDED outcome");
});

test("the model's exec_id rides through UNTOUCHED — the kernel validates it", async () => {
  // Ids are the kernel's ints; a name is a string. Routing forwards whatever the
  // model wrote and the KERNEL owns the semantics: an int it never minted comes
  // back as a refusal the model reads, and nothing runs.
  let ran = 0;
  const mind = new Mind([call("store", { type: "go" }, { exec_id: 424242 }), DONE]);
  const k = world(mind, () => ({ ok: ++ran }));
  await run(k, "go");
  assert.equal(ran, 0, "an invented int id must not create an execution");
  assert.match(mind.incoming[1]!, /never created/);
});

test("the recursion guard refuses a cycle and an over-deep chain before the lock", async () => {
  const k = world(new Mind(), storeAnswers);
  const cyc = await run(k, "x", { _call_stack: ["peer", "ai"] });
  assert.match(String(cyc["error"]), /cycle detected/);
  assert.deepEqual(cyc["cycle"], ["peer", "ai", "ai"]);
  const deep = await run(k, "x", { _call_stack: ["a", "b", "c", "d", "e", "f", "g", "h"] });
  assert.match(String(deep["error"]), /max call depth 8/);
});

test("the system block teaches the ONE tool grammar and the how-tos", async () => {
  const k = world(new Mind(), storeAnswers);
  const p = await defaultSystemContent("ai", k);
  // No schema is sent to the provider, so what the model knows is this text.
  assert.ok(p.includes('<call name="send">'));
  assert.ok(p.includes("<target_id>"));
  assert.ok(p.includes("## Reaching other agents"));
  assert.ok(p.includes("## Context window & compaction"));
  assert.ok(p.includes("## Your memory") && p.includes("MEMORY STORE `mem`"));
  assert.ok(p.startsWith("Fantastic kernel."), "the kernel's own reflect opens it");
  assert.ok(p.includes("You are `ai`."));
  assert.ok(!p.includes("<tool_call>") && !p.includes('"arguments"'), "no JSON grammar on the model path");
  // the titles / async blocks are opt-in, on the record
  assert.ok(!p.includes(TITLES_HOWTO) && !p.includes(ASYNC_HOWTO));
  k.updateMeta("ai", { context_titles: true, context_async: true });
  const q = await defaultSystemContent("ai", k);
  assert.ok(q.includes(TITLES_HOWTO) && q.includes(ASYNC_HOWTO));
});

test("a caller's system_prompt REPLACES the substrate prompt", async () => {
  const mind = new Mind([DONE]);
  const k = world(mind, storeAnswers);
  await run(k, "hi", { system_prompt: "You are a terse bot." });
  assert.equal(mind.seen[0]![0]!.content, "You are a terse bot.");
});

test("inquiry: the answer is what the model emits with inquiry_answer", async () => {
  const mind = new Mind(['<call name="inquiry_answer"><answer>yes, ship it</answer></call>']);
  const k = world(mind, storeAnswers);
  const out = (await k.send([{ target: "ai", payload: { type: "inquiry", text: "ship?", options: ["yes", "no"], request_id: "r1" } }]))[0]!;
  assert.deepEqual(out.result, { answer: "yes, ship it", request_id: "r1" });
  assert.ok(mind.incoming[0]!.endsWith("ship?\n\nOptions: yes, no"));
  await settle(k);
});

test("inquiry: no answer, or the placeholder, is refused — never harvested from prose", async () => {
  const k1 = world(new Mind(["probably yes " + DONE]), storeAnswers);
  const none = (await k1.send([{ target: "ai", payload: { type: "inquiry", text: "ship?" } }]))[0]!;
  assert.equal(none.status, "errored");
  assert.equal((none.result as Payload)["reason"], "unhandled");
  await settle(k1);
  const k2 = world(new Mind(['<call name="inquiry_answer"><answer>...</answer></call>']), storeAnswers);
  const ph = (await k2.send([{ target: "ai", payload: { type: "inquiry", text: "ship?" } }]))[0]!;
  assert.equal((ph.result as Payload)["reason"], "bad_arg");
  await settle(k2);
});

test("thinking is stripped before a turn is recorded", () => {
  assert.equal(withoutThinking("<think>deliberating</think>answer"), "answer");
});

test("the provider REFUSES to guess an endpoint", async () => {
  // Nothing here invents a host to send a key to.
  const p = new OpenAIProvider({});
  await assert.rejects(async () => {
    for await (const _ of p.chat([])) { /* consume */ }
  }, /endpoint/);
});
