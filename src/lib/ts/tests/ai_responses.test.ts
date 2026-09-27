// ResponsesProvider — the wire it sends, the two channels it reads, NO native tool
// route, and a visible cut. python `responses_backend/tests/test_responses_provider.py`
// is the oracle; `fetch` is injected, so nothing here touches a network.

import { test } from "node:test";
import assert from "node:assert/strict";
import { ResponsesProvider, toResponses } from "../src/bundles/ai/responses.ts";
import type { AIRecord } from "../src/bundles/ai/ai_backend.ts";

const EP = "http://proxy.test/v1/responses";

type Sent = { url: string; headers: Record<string, string>; body: any };

function sse(...events: object[]): string {
  return events.map((e: any) => `event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`).join("");
}

function done(status = "completed", reason: string | null = null, input = 42): object {
  const response: any = { status, usage: { input_tokens: input } };
  if (reason) response.incomplete_details = { reason };
  return { type: status === "completed" ? "response.completed" : "response.incomplete", response };
}

const text = (...chunks: string[]) => [
  { type: "response.created", response: { status: "in_progress" } },
  { type: "response.output_item.added", item: { type: "message" } },
  ...chunks.map((delta) => ({ type: "response.output_text.delta", delta })),
];

/** A fetch that answers `replies` in order (the last one repeats) and records every request. */
function fake(sent: Sent[], ...replies: [number, string, string][]) {
  return async (url: string, init: { headers: Record<string, string>; body: string }) => {
    sent.push({ url, headers: init.headers, body: JSON.parse(init.body) });
    const [status, body, ctype] = replies[Math.min(sent.length - 1, replies.length - 1)]!;
    return new Response(body, { status, headers: { "content-type": ctype } });
  };
}

const ok = (...events: object[]): [number, string, string] => [200, sse(...events), "text/event-stream"];

const MSGS = [
  { role: "system", content: "be terse" },
  { role: "user", content: "hi" },
  { role: "assistant", content: "hello" },
  { role: "user", content: "again" },
];

function provider(sent: Sent[], replies: [number, string, string][], rec: AIRecord = {}): ResponsesProvider {
  return new ResponsesProvider({ endpoint: EP, model: "qwen-test", api_key: "sk-test", ...rec }, fake(sent, ...replies));
}

async function drain(p: ResponsesProvider, maxTokens?: number): Promise<string> {
  let out = "";
  for await (const c of p.chat(MSGS, maxTokens)) out += c;
  return out;
}

test("the request is stateless typed items, no tools, python's key order", async () => {
  const sent: Sent[] = [];
  const p = provider(sent, [ok(...text("ok"), done())], { max_tokens: 900, sampling: { temperature: 0.2, top_p: null } });
  assert.equal(await drain(p), "ok");
  const b = sent[0]!.body;
  assert.deepEqual(Object.keys(b), ["model", "input", "max_output_tokens", "stream", "store", "instructions", "temperature"]);
  assert.equal(b.max_output_tokens, 900);
  assert.equal(b.store, false);
  assert.equal(b.instructions, "be terse");
  assert.deepEqual(b.input[1], { type: "message", role: "assistant", content: [{ type: "output_text", text: "hello" }] });
  assert.equal(b.tools, undefined);
  assert.equal(sent[0]!.url, EP, "the endpoint is used verbatim");
  assert.equal(sent[0]!.headers["authorization"], "Bearer sk-test");
  await drain(p, 64);
  assert.equal(sent[1]!.body.max_output_tokens, 64, "a side call's own bound wins");
});

test("messages translate like python: last system wins, (no content), other roles dropped", () => {
  const r = toResponses([
    { role: "system", content: "first" },
    { role: "system", content: "last wins" },
    { role: "assistant", content: "" },
    { role: "tool", content: "dropped" },
  ]);
  assert.equal(r.instructions, "last wins");
  assert.deepEqual(r.items, [{ type: "message", role: "assistant", content: [{ type: "output_text", text: "(no content)" }] }]);
});

test("thinking is wrapped, not dropped — raw or summary; an all-thought turn still closes", async () => {
  const sent: Sent[] = [];
  const think = (d: string, kind = "reasoning_text") => ({ type: `response.${kind}.delta`, delta: d });
  assert.equal(
    await drain(provider(sent, [ok(think("let me "), think("think"), { type: "response.output_text.delta", delta: "answer" }, done())])),
    "<think>let me think</think>answer",
  );
  assert.equal(
    await drain(provider(sent, [ok(think("sum", "reasoning_summary_text"), { type: "response.output_text.delta", delta: "out" }, done())])),
    "<think>sum</think>out",
  );
  assert.equal(await drain(provider(sent, [ok(think("hm"), done())])), "<think>hm</think>");
});

test("reported usage calibrates; a completed turn reports `completed`", async () => {
  const p = provider([], [ok(...text("ok"), done("completed", null, 1234))]);
  await drain(p);
  assert.equal(p.lastPromptTokens, 1234);
  assert.equal(p.lastStopReason, "completed");
});

test("a cut main turn throws; a bounded side call's cut is an answer", async () => {
  const cut = ok(...text("half a "), done("incomplete", "max_output_tokens"));
  const p = provider([], [cut]);
  await assert.rejects(drain(p), /without completing.*max_output_tokens/);
  assert.equal(p.lastStopReason, "max_tokens");
  const q = provider([], [cut]);
  assert.equal(await drain(q, 16), "half a ");
  assert.equal(q.lastStopReason, "max_tokens");
});

test("no terminal event is a cut connection; response.failed and a native function_call are loud", async () => {
  await assert.rejects(drain(provider([], [ok({ type: "response.output_text.delta", delta: "x" })])), /connection cut mid-stream/);
  await assert.rejects(
    drain(provider([], [ok({ type: "response.failed", response: { error: { message: "upstream died" } } })])),
    /upstream died/,
  );
  await assert.rejects(
    drain(provider([], [ok({ type: "response.output_item.added", item: { type: "function_call", name: "send" } }, done())])),
    /native function_call.*<call name=/,
  );
});

test("nothing is guessed: endpoint and model are required", async () => {
  await assert.rejects(drain(new ResponsesProvider({ model: "m" }, fake([]))), /endpoint/);
  await assert.rejects(drain(new ResponsesProvider({ endpoint: EP }, fake([]))), /model/);
});

test("a deliberate answer is never retried", async () => {
  const sent: Sent[] = [];
  await assert.rejects(drain(provider(sent, [[404, '{"error": "not found"}', "application/json"]])), /responses 404/);
  assert.equal(sent.length, 1);
});

test("a CDN error page is retried, then the stream is read", { timeout: 10_000 }, async () => {
  const sent: Sent[] = [];
  const p = provider(sent, [[521, "<!DOCTYPE html>", "text/html"], ok(...text("recovered"), done())]);
  assert.equal(await drain(p), "recovered");
  assert.equal(sent.length, 2);
});
