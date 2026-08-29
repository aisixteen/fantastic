// The raw tool-call envelope, pinned.
//
// Every case here is a shape a real model actually emitted. The parser exists to
// recover instructions from models that drift, so "we accept the ideal shape"
// would be the wrong test — what matters is that a call the model MEANT to make
// survives, and that one it botched is REPORTED rather than silently dropped.

import { test } from "node:test";
import assert from "node:assert/strict";

import { extractToolCalls, malformedCalls, parseOne } from "../src/bundles/ai/tool_parse.ts";

const wrap = (s: string) => `<tool_call>${s}</tool_call>`;

test("the taught shape parses", () => {
  const [c] = extractToolCalls(
    wrap('{"name":"send","arguments":{"target_id":"store","payload":{"type":"read"}}}'),
  );
  assert.equal(c.name, "send");
  assert.equal((c.arguments as any).target_id, "store");
  assert.ok(c.id.startsWith("call_"));
});

test("prose around the envelope is ignored", () => {
  const calls = extractToolCalls(
    `I will read the store now.\n${wrap('{"name":"send","arguments":{"target_id":"s","payload":{}}}')}\nDone.`,
  );
  assert.equal(calls.length, 1);
});

test("`tool` is accepted as an alias for `name`", () => {
  const c = parseOne('{"tool":"send","arguments":{"target_id":"s","payload":{}}}');
  assert.equal(c?.name, "send");
});

test("a FLATTENED object becomes the arguments", () => {
  // No `arguments` key at all — the rest of the object is it.
  const c = parseOne('{"name":"send","target_id":"s","payload":{"type":"read"}}');
  assert.equal((c!.arguments as any).target_id, "s");
});

test("DOUBLE-ENCODED arguments are decoded", () => {
  const c = parseOne('{"name":"send","arguments":"{\\"target_id\\":\\"s\\",\\"payload\\":{}}"}');
  assert.equal((c!.arguments as any).target_id, "s");
});

test("a stray `reply_to` one level too high is pulled into arguments", () => {
  const c = parseOne(
    '{"name":"send","reply_to":"self","arguments":{"target_id":"s","payload":{}}}',
  );
  assert.equal((c!.arguments as any).reply_to, "self");
});

test("`arguments` wins over a conflicting stray", () => {
  const c = parseOne(
    '{"name":"send","reply_to":"outer","arguments":{"reply_to":"inner","target_id":"s"}}',
  );
  assert.equal((c!.arguments as any).reply_to, "inner");
});

test("trailing junk after a COMPLETE object still yields the call", () => {
  // gpt-oss:120b closes `arguments` early, then emits `,"reply_to":"self"}}` —
  // one brace too many. The object that IS there must survive, and the tail is
  // where the model's real intent often sits.
  const c = parseOne(
    '{"name":"send","arguments":{"target_id":"s","payload":{}}},"reply_to":"self"}}',
  );
  assert.equal((c!.arguments as any).target_id, "s");
  assert.equal((c!.arguments as any).reply_to, "self");
});

test("an UNTERMINATED envelope at the end of the completion is kept", () => {
  // nemotron emits a complete, valid call object and simply stops. Dropping it
  // loses an unambiguous instruction.
  const calls = extractToolCalls(
    '<tool_call>{"name":"send","arguments":{"target_id":"s","payload":{}}}',
  );
  assert.equal(calls.length, 1);
});

test("the VERB COMPRESSED INTO THE TOOL NAME is translated to a send", () => {
  // `{"name":"reflect","arguments":{"target_id":"clock"}}` means "reflect the
  // clock". Untranslated it dispatched send("clock", null) and the handler
  // raised on a null payload.
  const c = parseOne('{"name":"reflect","arguments":{"target_id":"clock"}}');
  assert.equal(c!.name, "send");
  assert.equal((c!.arguments as any).target_id, "clock");
  assert.equal(((c!.arguments as any).payload as any).type, "reflect");
});

test("an explicit payload beats the name-compression rule", () => {
  const c = parseOne('{"name":"reflect","arguments":{"target_id":"c","payload":{"type":"x"}}}');
  assert.equal(c!.name, "reflect");
});

test("a MENTION of the tag in prose is not a call, and does not eat the real one", () => {
  // A stray opener in prose pairs with the closing tag of the genuine call
  // further down; consuming through the closer loses the genuine call entirely.
  const text =
    `To call a tool you write <tool_call> and then the JSON.\n` +
    wrap('{"name":"send","arguments":{"target_id":"s","payload":{}}}');
  const calls = extractToolCalls(text);
  assert.equal(calls.length, 1);
  assert.equal((calls[0].arguments as any).target_id, "s");
});

test("two calls in one turn are both returned, in order", () => {
  const calls = extractToolCalls(
    wrap('{"name":"send","arguments":{"target_id":"a","payload":{}}}') +
      wrap('{"name":"send","arguments":{"target_id":"b","payload":{}}}'),
  );
  assert.deepEqual(calls.map((c) => (c.arguments as any).target_id), ["a", "b"]);
});

test("a botched envelope is REPORTED, not silently dropped", () => {
  // THE failure mode measured on gemma-4-12B: a program carried inside a JSON
  // string whose closing quote is never written, so every following brace reads
  // as string content. A model that tried and failed must not look like a model
  // that chose to stop.
  // The closing quote of `code` is never written, so every following brace
  // reads as string content and the object ends unterminated.
  const bad = wrap('{"name":"send","arguments":{"code":"def f():\\n  return 1}}');
  const reports = malformedCalls(bad);
  assert.equal(reports.length, 1);
  assert.match(reports[0].reason, /did not parse|never closed/);
});

test("a malformed envelope yields no call", () => {
  assert.equal(extractToolCalls(wrap("{not json at all")).length, 0);
});

test("ids are unique per call", () => {
  const calls = extractToolCalls(
    wrap('{"name":"send","arguments":{"target_id":"a","payload":{}}}') +
      wrap('{"name":"send","arguments":{"target_id":"b","payload":{}}}'),
  );
  assert.notEqual(calls[0].id, calls[1].id);
});
