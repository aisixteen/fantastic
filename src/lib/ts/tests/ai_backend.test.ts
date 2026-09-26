// The BROWSER-RESIDENT AI agent: its loop, not its wire.
//
// The provider is faked here on purpose. What is under test is the thing that
// makes a streaming text endpoint into an AGENT: that the calls it writes as
// text actually reach other agents through the kernel, that their replies come
// back into its context, and that it stops for the right reasons. The wire
// itself is pinned separately in `tool_parse.test.ts`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import {
  OpenAIProvider,
  runLoop,
  systemPrompt,
  withoutThinking,
  type Provider,
} from "../src/bundles/ai/ai_backend.ts";

/** A provider that replays scripted turns — one per pass. */
function scripted(turns: string[]): Provider {
  let i = 0;
  return {
    async *chat() {
      const t = turns[Math.min(i++, turns.length - 1)];
      // Streamed in pieces, because a real one arrives that way and the loop
      // must not depend on a whole turn landing at once.
      for (const piece of t.match(/.{1,7}/gs) ?? []) yield piece;
    },
  };
}

function tree(): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  k.registerBundle("store.tools", (_id, payload) => {
    const p = payload as any;
    if (p.type === "read") return { key: p.key, value: 42 };
    if (p.type === "reflect") return { id: "store", sentence: "a store" };
    return { error: "unknown" };
  });
  k.register(new Agent({ id: "store", parentId: "kernel_state", handlerModule: "store.tools" }));
  return k;
}

const call = (target: string, payload: object) =>
  `<tool_call>{"name":"send","arguments":{"target_id":"${target}","payload":${JSON.stringify(payload)}}}</tool_call>`;

test("a call written as TEXT actually reaches another agent", async () => {
  const k = tree();
  const hist = [{ role: "system", content: systemPrompt("ai") }];
  const out = await runLoop(
    "ai",
    k,
    scripted([call("store", { type: "read", key: "answer" }), "The answer is 42."]),
    hist,
  );
  assert.equal(out.calls, 1);
  // The REPLY must land back in context — that is what makes it a loop rather
  // than a one-shot completion.
  // Match the REPLY turns, not the system block — which mentions the tag while
  // teaching it, and counted as one until this filter got specific.
  const responses = hist.filter(
    (m) => m.role === "user" && m.content.startsWith("<tool_response>"),
  );
  assert.equal(responses.length, 1);
  assert.match(responses[0].content, /"value":42/);
  assert.equal(out.text, "The answer is 42.");
});

test("the loop ends when the model stops calling", async () => {
  const out = await runLoop("ai", tree(), scripted(["Nothing to do."]), []);
  assert.equal(out.passes, 1);
  assert.equal(out.calls, 0);
});

test("two calls in one turn both dispatch", async () => {
  const k = tree();
  const hist: any[] = [];
  const out = await runLoop(
    "ai",
    k,
    scripted([
      call("store", { type: "read", key: "a" }) + call("store", { type: "reflect" }),
      "done",
    ]),
    hist,
  );
  assert.equal(out.calls, 2);
});

test("a REFUSAL is fed back as information, not thrown", async () => {
  // The gate is two-sided; an agent that cannot reach a peer must learn that
  // from its own transcript rather than have the loop crash under it.
  const k = tree();
  const hist: any[] = [];
  const out = await runLoop("ai", k, scripted([call("nobody", { type: "read" }), "ok"]), hist);
  assert.equal(out.calls, 1);
  const resp = hist.find((m) => m.content.includes("<tool_response>"));
  assert.ok(resp, "the refusal never reached the model");
});

test("a MALFORMED envelope is named back to the model, and the loop continues", async () => {
  // A model that tried three times and produced nothing looks identical to one
  // that chose to stop — unless it is told what went wrong.
  const k = tree();
  const hist: any[] = [];
  const out = await runLoop(
    "ai",
    k,
    scripted([
      '<tool_call>{"name":"send","arguments":{"code":"def f():\\n  return 1}}</tool_call>',
      call("store", { type: "read", key: "a" }),
      "recovered",
    ]),
    hist,
  );
  const notice = hist.find((m) => m.content.includes("could not be parsed"));
  assert.ok(notice, "the model was never told its envelope was malformed");
  assert.equal(out.calls, 1, "it never got to make the corrected call");
});

test("a NAMED execution is forwarded, so a repeat does not re-run the side effect", async () => {
  // Ensure-semantics is the whole point of naming a call: unknown ⇒ dispatch,
  // settled ⇒ the RECORDED outcome, handler never runs again. The loop dropped
  // the model's `id` until a double-fire probe caught it — the model named the
  // execution, the kernel never heard the name, and the coin was dispensed twice.
  let dispensed = 0;
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  k.registerBundle("disp.tools", () => ({ coin: ++dispensed }));
  k.register(
    new Agent({ id: "dispenser", parentId: "kernel_state", handlerModule: "disp.tools" }),
  );

  const named =
    '<tool_call>{"name":"send","arguments":{"target_id":"dispenser","id":"coin-alpha","payload":{"type":"dispense"}}}</tool_call>';
  // The SAME call, twice — exactly what "work as if your reply was lost" means.
  await runLoop("ai", k, scripted([named, named, "done"]), []);
  assert.equal(dispensed, 1, "the named repeat dispensed a second time");
});

test("the model's id rides through UNTOUCHED — the kernel validates it", async () => {
  // Ids are the kernel's ints; a name is a string. The loop forwards whatever
  // the model wrote and the KERNEL owns the semantics: an int it never minted
  // comes back as a refusal the model reads, and nothing runs.
  let ran = 0;
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  k.registerBundle("p.tools", () => ({ ok: ++ran }));
  k.register(new Agent({ id: "p", parentId: "kernel_state", handlerModule: "p.tools" }));
  const hist: any[] = [];
  const out = await runLoop(
    "ai",
    k,
    scripted([
      '<tool_call>{"name":"send","arguments":{"target_id":"p","id":424242,"payload":{"type":"go"}}}</tool_call>',
      "done",
    ]),
    hist,
  );
  assert.equal(out.calls, 1);
  assert.equal(ran, 0, "an invented int id must not create an execution");
  const resp = hist.find((m) => m.content.startsWith("<tool_response>"));
  assert.match(resp.content, /never created/);
});

test("maxPasses bounds a model that calls forever", async () => {
  const out = await runLoop(
    "ai",
    tree(),
    scripted([call("store", { type: "read", key: "a" })]), // never stops calling
    [],
    { maxPasses: 3 },
  );
  assert.equal(out.passes, 3);
});

test("the system prompt teaches the ONE tool and the exact envelope", () => {
  const p = systemPrompt("ai");
  // No schema is sent to the provider, so what the model knows is this text.
  assert.match(p, /<tool_call>/);
  assert.match(p, /target_id/);
  assert.match(p, /reflect/);
});

test("thinking is stripped before a turn is shown", () => {
  assert.equal(withoutThinking("<think>deliberating</think>answer"), "answer");
});

test("the provider REFUSES to guess an endpoint", async () => {
  // Nothing here invents a host to send a key to.
  const p = new OpenAIProvider({});
  await assert.rejects(async () => {
    for await (const _ of p.chat([])) { /* consume */ }
  }, /endpoint/);
});
