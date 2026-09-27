// REPLY ROUTING — every call in a turn gets its reply, delivered TOGETHER as one
// message per destination, each block tagged `N of M in batch <b>` when M > 1.
// Mirrors python ai_core tests `test_reply_delivery.py` and
// `test_reply_manifest.py` (the stamp, and a call written inside a thought).
//
// The kernel is a stub, like python's: a prototype-linked view of a real kernel
// whose `send` answers every non-`send` item with `{ok: target}` under the pid
// `<target>:pid`, and records the `type: send` handoffs (the deliveries).

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Payload } from "../src/kernel/json.ts";
import type { Item, Outcome } from "../src/kernel/outcome.ts";
import { routeReply } from "../src/bundles/ai/routing.ts";
import type { RouteArgs } from "../src/bundles/ai/routing.ts";
import { REPLY_TAG } from "../src/bundles/ai/replies.ts";
import { withoutThinking } from "../src/bundles/ai/think.ts";
import { extractToolCalls, renderCall } from "../src/bundles/ai/tool_parse.ts";
import type { Value } from "../src/bundles/ai/tool_parse.ts";

interface Stub {
  k: Kernel;
  sent: [string, Payload][];
  batches: string[][];
  items: Item[];
}

function stub(answer?: (it: Item) => Outcome | null): Stub {
  const real = new Kernel();
  real.setRoot(new Agent({ id: "kernel_state" }));
  real.register(new Agent({ id: "ai", parentId: "kernel_state" }));
  const s: Stub = { k: Object.create(real) as Kernel, sent: [], batches: [], items: [] };
  (s.k as unknown as { send: (items: Item[]) => Promise<Outcome[]> }).send = async (items) => {
    s.items.push(...items);
    if (items.some((i) => i.payload["type"] !== "send")) s.batches.push(items.map((i) => i.target));
    return items.map((it) => {
      if (it.payload["type"] === "send") {
        s.sent.push([it.target, it.payload]);
        return { pid: null, status: "completed", result: null, attempts: 1 };
      }
      const own = answer?.(it) ?? null;
      if (own !== null) return own;
      return { pid: `${it.target}:pid` as never, status: "completed", result: { ok: it.target }, attempts: 1 };
    });
  };
  return s;
}

function route(s: Stub, text: string, over: Partial<RouteArgs> = {}): Promise<void> {
  return routeReply({
    selfId: "ai", kernel: s.k, text, clientId: "cli", sendTo: null, systemPrompt: null,
    sendTimeout: 180, incoming: "", messageId: null, ...over,
  });
}

/** Deliveries are spawned, not awaited — wait for them to land. */
async function delivered(s: Stub, want: number, timeoutMs = 2000): Promise<void> {
  const t0 = Date.now();
  while (s.sent.length < want) {
    if (Date.now() - t0 > timeoutMs) throw new Error(`delivered ${s.sent.length}, wanted ${want}`);
    await new Promise((r) => setTimeout(r, 5));
  }
}

function call(target: string, verb = "reflect", replyTo: string | null = "self", extra: { [k: string]: Value } = {}): string {
  const args: { [k: string]: Value } = { target_id: target, payload: { type: verb }, ...extra };
  if (replyTo !== null) args["reply_to"] = replyTo;
  return renderCall("send", args);
}

// ── one message per destination (test_reply_delivery.py) ─────────────────────

test("a turn of four calls is answered in one message", async () => {
  const s = stub();
  await route(s, call("kernel") + call("readings") + call("totals") + call("runner"));
  await delivered(s, 1);
  assert.deepEqual(s.batches, [["kernel", "readings", "totals", "runner"]], "all four calls dispatch in ONE kernel batch");
  assert.equal(s.sent.length, 1, "one destination -> exactly one delivered message");
  const [target, payload] = s.sent[0]!;
  assert.equal(target, "ai");
  const blocks = String(payload["text"]).split("\n\n");
  assert.equal(blocks.length, 4);
  ["kernel", "readings", "totals", "runner"].forEach((agent, i) => {
    assert.ok(blocks[i]!.startsWith(`[reply to ${agent}:pid · ${agent} reflect · ${i + 1} of 4`), blocks[i]);
  });
});

test("two destinations get two messages with their own counts", async () => {
  const s = stub();
  await route(s, call("kernel") + call("totals", "reflect", "scribe") + call("readings"));
  await delivered(s, 2);
  const byDest = new Map(s.sent.map(([t, p]) => [t, String(p["text"])]));
  assert.deepEqual(new Set(byDest.keys()), new Set(["ai", "scribe"]));
  assert.ok(byDest.get("ai")!.includes(" · 1 of 2 in batch "));
  assert.ok(byDest.get("ai")!.includes(" · 2 of 2 in batch "));
  assert.ok(!byDest.get("scribe")!.includes("in batch"), "a lone block carries no suffix");
});

test("fire-and-forget still dispatches but is not counted", async () => {
  const s = stub();
  await route(s, call("kernel") + call("totals", "reflect", "null"));
  await delivered(s, 1);
  assert.deepEqual(s.batches, [["kernel", "totals"]], "the discarded call still ran");
  assert.equal(s.sent.length, 1);
  const body = String(s.sent[0]![1]["text"]);
  assert.ok(!body.includes("totals"));
  assert.ok(!body.includes("in batch"), "one awaited call -> no batch suffix");
});

test("the layer default timeout rides every item; the model's own overrides it", async () => {
  const s = stub();
  await route(s, call("kernel") + call("slowpoke", "crunch", "self", { timeout: 5 }), { sendTimeout: 42 });
  await delivered(s, 1);
  const seen = s.items.filter((i) => i.payload["type"] !== "send").map((i) => [i.target, i.timeout]);
  assert.deepEqual(seen, [["kernel", 42], ["slowpoke", 5]]);
});

test("a failed outcome is rendered, not hidden", async () => {
  const s = stub((it) =>
    it.target === "hung"
      ? { pid: "hung:1" as never, status: "timed_out", result: { error: "no reply after 42s", reason: "timeout" }, attempts: 1 }
      : null,
  );
  await route(s, call("hung") + call("kernel"), { sendTimeout: 42 });
  await delivered(s, 1);
  const blocks = String(s.sent[0]![1]["text"]).split("\n\n");
  assert.equal(blocks.length, 2);
  assert.ok(blocks[0]!.startsWith("[reply to hung:1 · hung reflect · 1 of 2"), blocks[0]);
  assert.ok(blocks[0]!.includes("no reply after 42s"));
  assert.ok(blocks[1]!.startsWith("[reply to kernel:pid · kernel reflect · 2 of 2"), blocks[1]);
});

test("the model's exec_id rides the item as its id", async () => {
  const s = stub();
  await route(s, call("vault", "withdraw", "self", { exec_id: "job-alpha", retries: 2 }));
  assert.equal(s.items[0]!.id, "job-alpha");
  assert.equal(s.items[0]!.retries, 2);
  assert.deepEqual(s.items[0]!.payload, { type: "withdraw" });
});

test("<to> fans out one copy per recipient, carrying the list and msg", async () => {
  const s = stub();
  const c = renderCall("send", {
    to: ["solver_2", "solver_3"],
    re: 398,
    reply_to: "none",
    payload: { type: "send", text: "CANDIDATE 586" },
  });
  await route(s, c, { messageId: 412 });
  const peers = s.items.filter((i) => i.target === "solver_2" || i.target === "solver_3");
  assert.deepEqual(peers.map((i) => i.target), ["solver_2", "solver_3"]);
  for (const i of peers) {
    assert.deepEqual(i.payload["to"], ["solver_2", "solver_3"]);
    assert.equal(i.payload["msg"], 412);
    assert.equal(i.payload["re"], 398);
    assert.equal(i.payload["text"], "CANDIDATE 586");
  }
});

test("a self-call inherits the conversation it was made in", async () => {
  const s = stub();
  await route(s, call("ai", "context_status") + call("kernel"), { clientId: "tab7" });
  const self = s.items.find((i) => i.target === "ai" && i.payload["type"] === "context_status")!;
  assert.equal(self.payload["client_id"], "tab7");
  const other = s.items.find((i) => i.target === "kernel")!;
  assert.ok(!("client_id" in other.payload), "only a self-call carries it");
});

test("a malformed call comes back to SELF, its fault named and its block shown", async () => {
  const s = stub();
  await route(s, '<call name="send"><type>reflect</type></call>');
  await delivered(s, 1);
  const [target, payload] = s.sent[0]!;
  assert.equal(target, "ai");
  const text = String(payload["text"]);
  assert.ok(text.startsWith("[reply to - · - -]\ncall NOT dispatched: a send names its target"), text);
  const echoed = text.split("\n\nyour block:\n")[1]!;
  assert.equal(echoed, 'name="send"><type>reflect</type>', "the block is shown whole, between the delimiters");
});

test("a send with no payload is refused in its slot, nothing dispatched", async () => {
  const s = stub();
  await route(s, renderCall("send", { target_id: "notes", reply_to: "self", payload: {} }));
  await delivered(s, 1);
  assert.deepEqual(s.batches, []);
  const text = String(s.sent[0]![1]["text"]);
  assert.ok(text.startsWith("[reply to - · notes -]"), text);
  assert.ok(text.includes("carried no payload"));
});

test("send_to hands this pass's TEXT to another agent once, when the model called nothing", async () => {
  const s = stub();
  await route(s, "the answer is 42 " + '<call name="done"></call>', { sendTo: "scribe" });
  await delivered(s, 1);
  assert.deepEqual(s.sent.map(([t]) => t), ["scribe"]);
  assert.ok(String(s.sent[0]![1]["text"]).startsWith("the answer is 42"));
});

// ── the N of M stamp (test_reply_manifest.py) ────────────────────────────────

test("a single call is not stamped", async () => {
  const s = stub();
  await route(s, call("kernel_state"), { clientId: "probe" });
  await delivered(s, 1);
  const text = String(s.sent[0]![1]["text"]);
  assert.ok(!text.includes("in batch"));
  assert.ok(text.startsWith("[reply to kernel_state:pid · kernel_state reflect]"), text);
});

test("parallel calls each say how many are coming, under one token", async () => {
  const s = stub();
  await route(s, call("exec_bridge") + call("file_bridge"));
  await delivered(s, 1);
  assert.equal(s.sent.length, 1, "one destination -> ONE message, blocks inside");
  const blocks = String(s.sent[0]![1]["text"]).split("\n\n");
  assert.match(blocks[0]!, /· 1 of 2 in batch [0-9a-f]{4}\]/);
  assert.match(blocks[1]!, /· 2 of 2 in batch [0-9a-f]{4}\]/);
  const toks = blocks.map((b) => /in batch ([0-9a-f]{4})/.exec(b)![1]);
  assert.equal(toks[0], toks[1], "one turn must be one batch");
});

test("fire-and-forget calls are not counted in M", async () => {
  const s = stub();
  await route(s, call("a") + call("b", "set", "null"));
  await delivered(s, 1);
  assert.ok(!String(s.sent[0]![1]["text"]).includes("in batch"));
});

test("the tag regex still parses a stamped envelope", () => {
  const m = REPLY_TAG.exec("[reply to ai:beef · kernel_state reflect · 1 of 3 in batch a1b2]")!;
  assert.equal(m[1], "kernel_state");
  assert.equal(m[2], "reflect");
  const m2 = REPLY_TAG.exec("[reply to ai:beef · kernel_state reflect]")!;
  assert.equal(m2[1], "kernel_state");
  assert.equal(m2[2], "reflect");
});

// ── a call written inside a thought ──────────────────────────────────────────
// Dispatch reads the FULL text; history reads the STRIPPED text. The two must
// agree, or a call made inside a thought runs and leaves no record.

const C = renderCall("send", { target_id: "totals", payload: { type: "read" } });

function pair(text: string): [number, number] {
  return [extractToolCalls(text).length, extractToolCalls(withoutThinking(text)).length];
}

test("a call inside a thought survives into history", () => {
  assert.deepEqual(pair(`<think>let me check ${C} done</think>`), [1, 1]);
});

test("reasoning around a kept call is still dropped", () => {
  const kept = withoutThinking(`<think>long private reasoning ${C} more</think>`);
  assert.ok(!kept.includes("long private reasoning"));
  assert.ok(!kept.includes("more"));
  assert.ok(kept.includes(C));
});

test("an unterminated thought still keeps its call", () => {
  assert.deepEqual(pair(`<think>cut mid ${C}`), [1, 1]);
});

test("a thought with no call persists as nothing", () => {
  assert.equal(withoutThinking("<think>reasoning, no answer</think>"), "");
});

test("calls inside and outside one turn both persist", () => {
  assert.deepEqual(pair(`<think>a ${C} b</think>${C}`), [2, 2]);
});

test("an unclosed envelope in a thought is reasoning, not an action", () => {
  assert.equal(withoutThinking('<think>maybe <call name="send"><type>x</type> …</think>'), "");
});
