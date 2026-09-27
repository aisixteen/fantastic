// The ONE call grammar: `<call name="…">` with bare child tags, args inline,
// `bytes` verbatim, no JSON anywhere. Parse or refuse with the fault named —
// never repair. Mirrors python `ai_core/tests/test_tool_parse.py` case for case.

import { test } from "node:test";
import assert from "node:assert/strict";

import { defang, extractToolCalls, malformedCalls, renderCall } from "../src/bundles/ai/tool_parse.ts";
import type { MalformedCall, ToolCall, Value } from "../src/bundles/ai/tool_parse.ts";

const enc = (s: string): Uint8Array => new TextEncoder().encode(s);

const WRITE = `<call name="send">
<target_id>notes</target_id>
<reply_to>self</reply_to>
<title>write the report</title>
<type>write</type>
<path>report.md</path>
<bytes>PRJ-1BCA4F

# Report
- the phrase "quick brown fox", verbatim
</bytes>
</call>`;

const PATCH = `<call name="send">
<target_id>notes</target_id>
<type>patch</type>
<edits><item><path>hello.py</path><hunks>2</hunks></item></edits>
<bytes><<<<<<< SEARCH
    msg = "Hello, " + name
=======
    return f"Hello, {name}!"
>>>>>>> REPLACE
</bytes>
</call>`;

function one(text: string): ToolCall {
  const calls = extractToolCalls(text);
  assert.equal(calls.length, 1, JSON.stringify(malformedCalls(text)));
  return calls[0] as ToolCall;
}

function payloadOf(text: string): { [k: string]: Value } {
  return one(text).arguments["payload"] as { [k: string]: Value };
}

// ── the taught shapes ────────────────────────────────────────────────────────

test("write: args inline, routing out, bytes verbatim", () => {
  const c = one(WRITE);
  assert.equal(c.name, "send");
  assert.ok(c.id.startsWith("call_"));
  const a = c.arguments;
  assert.equal(a["target_id"], "notes");
  assert.equal(a["reply_to"], "self");
  assert.equal(a["title"], "write the report");
  assert.deepEqual(a["payload"], {
    type: "write",
    path: "report.md",
    bytes: enc('PRJ-1BCA4F\n\n# Report\n- the phrase "quick brown fox", verbatim\n'),
  });
});

test("patch: a nested list and typed hunks", () => {
  const p = payloadOf(PATCH);
  assert.equal(p["type"], "patch");
  assert.deepEqual(p["edits"], [{ path: "hello.py", hunks: 2 }]);
  assert.ok(new TextDecoder().decode(p["bytes"] as Uint8Array).startsWith('<<<<<<< SEARCH\n    msg = '));
});

test("JSON-looking content rides untouched", () => {
  const text =
    '<call name="send"><target_id>notes</target_id><type>write</type>' +
    '<path>c.json</path><bytes>{"name": "demo"}\n{"tags": ["a", "b"]}\n</bytes></call>';
  assert.deepEqual(payloadOf(text)["bytes"], enc('{"name": "demo"}\n{"tags": ["a", "b"]}\n'));
});

test("lists are <item> children, any count", () => {
  const text =
    '<call name="send"><target_id>me</target_id><type>grep</type>' +
    "<query>marker</query><ids><item>12</item><item>15</item></ids></call>";
  assert.deepEqual(payloadOf(text)["ids"], [12, 15]);
  assert.deepEqual(payloadOf(text.replace("<item>15</item>", ""))["ids"], [12]);
  const allowed =
    '<call name="send"><target_id>kernel</target_id><type>update_agent</type>' +
    "<id>worker</id><allowed><item>python3 \\S+</item></allowed></call>";
  assert.deepEqual(payloadOf(allowed)["allowed"], ["python3 \\S+"]);
});

test("an object with one field is not a list", () => {
  const text =
    '<call name="send"><target_id>notes</target_id><type>patch</type>' +
    "<edit><path>a.py</path></edit><bytes>x</bytes></call>";
  assert.deepEqual(payloadOf(text)["edit"], { path: "a.py" });
});

test("scalars are typed by the one rule", () => {
  const text =
    '<call name="send"><target_id>me</target_id><type>pull</type>' +
    "<offset>0</offset><length>1.5</length><checkpoint>true</checkpoint>" +
    "<why>keep</why><empty></empty></call>";
  assert.deepEqual(payloadOf(text), { type: "pull", offset: 0, length: 1.5, checkpoint: true, why: "keep", empty: "" });
});

test("done and inquiry_answer", () => {
  let c = one('<call name="done"></call>');
  assert.equal(c.name, "done");
  assert.deepEqual(c.arguments, {});
  c = one('<call name="inquiry_answer"><answer>yes, "both"</answer></call>');
  assert.equal(c.name, "inquiry_answer");
  assert.deepEqual(c.arguments, { answer: 'yes, "both"' });
});

// ── verbatim mode ────────────────────────────────────────────────────────────

test("a closing call tag inside bytes does not end the call", () => {
  const text =
    '<call name="send"><target_id>notes</target_id><type>write</type>' +
    "<path>a.md</path><bytes>the text says </call> and goes on\n</bytes>" +
    "<checkpoint>true</checkpoint></call>";
  const p = payloadOf(text);
  assert.deepEqual(p["bytes"], enc("the text says </call> and goes on\n"));
  assert.equal(p["checkpoint"], true);
});

test("bytes is never trimmed and other values are", () => {
  const text =
    '<call name="send"><target_id>notes</target_id><type>write</type>' +
    "<path>  a.md  </path><bytes>  two spaces \n</bytes></call>";
  const p = payloadOf(text);
  assert.equal(p["path"], "a.md");
  assert.deepEqual(p["bytes"], enc("  two spaces \n"));
});

test("an unterminated call at the end is still an attempt", () => {
  assert.deepEqual(payloadOf('<call name="send"><target_id>notes</target_id><type>reflect</type>'), { type: "reflect" });
});

test("several calls in order, and a call inside think", () => {
  const calls = extractToolCalls(`<think>first ${WRITE}</think>then\n${PATCH}\n`);
  assert.deepEqual(
    calls.map((c) => (c.arguments["payload"] as { [k: string]: Value })["type"]),
    ["write", "patch"],
  );
});

test("a mention in prose is stepped over, not reported", () => {
  const text = `I will emit a <call soon. ${PATCH}`;
  assert.equal(extractToolCalls(text).length, 1);
  assert.deepEqual(malformedCalls(text), []);
  assert.deepEqual(extractToolCalls("a <caller> is not an envelope"), []);
});

// ── refusals: fault named, shape shown, nothing repaired ────────────────────

function refused(text: string, ...needles: string[]): MalformedCall {
  assert.deepEqual(extractToolCalls(text), []);
  const bad = malformedCalls(text);
  assert.equal(bad.length, 1, JSON.stringify(bad));
  const err = (bad[0] as MalformedCall).error;
  for (const n of needles) assert.ok(err.includes(n), `${n} :: ${err}`);
  assert.ok(err.startsWith("call NOT dispatched: ") && err.includes("The shape:"));
  assert.ok(!(bad[0] as MalformedCall).your_block.includes("<call"), "the echoed block is defanged");
  return bad[0] as MalformedCall;
}

test("a bytes closer inside a document ends the value early and is refused", () => {
  refused(
    '<call name="send"><target_id>notes</target_id><type>write</type>' +
      "<path>a.md</path><bytes>first </bytes> no\nreal end</bytes></call>",
    "text after the last tag",
    "no",
  );
});

test("bytes never closed", () => {
  refused('<call name="send"><target_id>notes</target_id><type>write</type><bytes>open', "`<bytes>` opened and never closed");
});

test("an element never closed", () => {
  refused('<call name="send"><target_id>notes</target_id><type>write<path>a</path></call>', "`<type>` opened and never closed");
});

test("a duplicate scalar tag", () => {
  refused('<call name="send"><target_id>n</target_id><path>a</path><path>b</path></call>', "duplicate `<path>`", "<path><item>");
});

test("text between tags", () => {
  refused('<call name="send"><target_id>n</target_id> stray words <type>x</type></call>', "text outside a tag");
});

test("a send without a target", () => {
  refused('<call name="send"><type>reflect</type></call>', "a send names its target");
});

test("a head with an extra attribute", () => {
  refused('<call name="send" id="7"><target_id>n</target_id></call>', 'the call head must be exactly `name="<tool>"`');
});

test("defang breaks every delimiter", () => {
  const d = defang('<call name="x"><bytes>a</bytes></call>');
  assert.ok(!d.includes("<call") && !d.includes("</call>") && !d.includes("<bytes>"));
  assert.ok(d.includes("‹call"));
});

test("the broken block is shown whole", () => {
  const big = "x".repeat(40_000);
  const bad = malformedCalls(`<call name="send"><target_id>n</target_id><type>write</type><bytes>${big}`);
  assert.equal(bad.length, 1);
  assert.ok((bad[0] as MalformedCall).your_block.includes(big));
});

test("renderCall round-trips the parser shape", () => {
  const args: { [k: string]: Value } = {
    target_id: "notes",
    reply_to: "self",
    title: "t",
    payload: {
      type: "patch",
      edits: [
        { path: "a.py", hunks: 2 },
        { path: "b.py", hunks: 1 },
      ],
      ids: [3],
      dry_run: true,
      bytes: enc('x = "1"\n</call>\n'),
    },
  };
  assert.deepEqual(one(renderCall("send", args)).arguments, args);
  assert.deepEqual(one(renderCall("done")).arguments, {});
  assert.deepEqual(one(renderCall("inquiry_answer", { answer: "ok" })).arguments, { answer: "ok" });
});

test("grant maps keyed by ids and relative tokens are tags", () => {
  const text =
    '<call name="send"><target_id>kernel</target_id><type>update_agent</type>' +
    "<id>vault</id><callable_by><$self>*</$self><$parent>*</$parent>" +
    "<ai-rev.2>*</ai-rev.2><notes><item>read</item><item>write</item></notes>" +
    "</callable_by></call>";
  const p = payloadOf(text);
  assert.deepEqual(p["callable_by"], { $self: "*", $parent: "*", "ai-rev.2": "*", notes: ["read", "write"] });
  assert.deepEqual(one(renderCall("send", { target_id: "k", payload: p })).arguments["payload"], p);
});

test("a send may name several recipients with <to>", () => {
  const text =
    '<call name="send"><to><item>solver_2</item><item>solver_3</item></to>' +
    "<re>412</re><type>send</type><text>CANDIDATE 586</text></call>";
  const a = one(text).arguments;
  assert.deepEqual(a["to"], ["solver_2", "solver_3"]);
  assert.equal(a["re"], 412);
  assert.ok(!("target_id" in a));
  assert.deepEqual(a["payload"], { type: "send", text: "CANDIDATE 586" });
  assert.deepEqual(one(renderCall("send", a)).arguments, a);
});

test("a send with neither target nor <to> is refused naming both", () => {
  refused('<call name="send"><type>reflect</type></call>', "<target_id>", "<to><item>");
});

test("await is a control call", () => {
  const c = one('<call name="await"></call>');
  assert.equal(c.name, "await");
  assert.deepEqual(c.arguments, {});
});

test("<item> beside other tags is a field", () => {
  const c = one(
    '<call name="send"><target_id>user</target_id><type>inquiry</type>' +
      "<action><payload><type>buy</type><item>rocket</item></payload></action></call>",
  );
  const action = (c.arguments["payload"] as { [k: string]: Value })["action"] as { [k: string]: Value };
  assert.deepEqual(action["payload"], { type: "buy", item: "rocket" });
});

test("call ids are unique per call", () => {
  const calls = extractToolCalls(WRITE + PATCH);
  assert.equal(calls.length, 2);
  assert.notEqual(calls[0]?.id, calls[1]?.id);
});
