// `strip` — the ledger at mixed resolution. Reply BODIES are stubbed oldest-first
// in one batch (hysteresis), the model's own turns are never touched, a stub
// names the call it answered, the set is monotone so the rendered prefix is
// byte-stable between strip events, and the cut is only the second stage.
// Mirrors python ai_core/tests/test_strip.py (the title-cleaning case has no ts
// counterpart: the ts call grammar carries no titles).

import { test } from "node:test";
import assert from "node:assert/strict";
import type { Payload } from "../src/kernel/json.ts";
import { estimateTokens } from "../src/bundles/ai/context.ts";
import type { ProjectionCtx, Turn } from "../src/bundles/ai/context.ts";
import { getStrategy } from "../src/bundles/ai/strategies/index.ts";
import { LOW, stripped } from "../src/bundles/ai/strategies/strip.ts";
import { wrap } from "../src/bundles/ai/replies.ts";
import { recall } from "../src/bundles/ai/verbs.ts";
import { World, callText, turn } from "./ai_world.ts";

const strip = getStrategy("strip")!;
const BIG = "x".repeat(6000);

test.beforeEach(() => stripped.clear());

function ctx(budget: number, selfId = "ai"): ProjectionCtx {
  return { budget, recentN: 4, summarize: null, selfId, kernel: null, clientId: null };
}

function reply(pid: number, body = BIG, verb = "read"): Turn {
  return { role: "user", content: wrap(body), reply: { pid, target: "notes", verb } };
}

function run(n = 8, live = "write the report"): Turn[] {
  const b: Turn[] = [turn("user", "GOAL: survey then report")];
  for (let i = 0; i < n; i++) {
    b.push(turn("assistant", `Reading filler_${i}. ${callText("notes", { type: "read" })}`));
    b.push(reply(1000 + i));
    b.push(turn("assistant", `filler_${i} was padding.`));
  }
  b.push(turn("user", live));
  return b;
}

test("under budget nothing is stripped", async () => {
  const body = run(2);
  const proj = await strip(body, [], {}, ctx(100_000));
  assert.equal(proj.stripped, 0);
  assert.deepEqual(proj.body.map((m) => m["content"]), body.map((m) => m["content"]));
});

test("over budget strips the oldest bodies in one batch down to LOW", async () => {
  const body = run(8);
  const bud = Math.floor(estimateTokens(body) * 0.7);
  const proj = await strip(body, [], {}, ctx(bud));
  assert.ok((proj.stripped ?? 0) > 0);
  // HYSTERESIS: went well below the line, not just under it.
  assert.ok(estimateTokens(proj.body) <= bud * LOW + 200);
  // OLDEST FIRST, CONTIGUOUS: stripped replies are a prefix of the reply order.
  const stubbed = [...Array(8).keys()].filter((i) => String(proj.body[2 + 3 * i]!["content"]).includes("(stripped:"));
  assert.deepEqual(stubbed, [...Array(stubbed.length).keys()]);
  assert.equal(proj.omittedMarker, false, "no cut was needed");
});

test("a stub names the call it answered and how big the body was", async () => {
  const body = run(8);
  const proj = await strip(body, [], {}, ctx(Math.floor(estimateTokens(body) * 0.5)));
  const stub = String(proj.body[2]!["content"]);
  assert.ok(stub.startsWith("<tool_response>[reply to 1000 · notes read]"), stub);
  assert.match(stub, /\(stripped: 6000 bytes/);
  assert.ok(!stub.includes(BIG));
  assert.deepEqual(proj.body[2]!["reply"], body[2]!["reply"], "the row keeps its reply metadata");
});

test("the model's own turns are never touched", async () => {
  const body = run(8);
  const proj = await strip(body, [], {}, ctx(Math.floor(estimateTokens(body) * 0.5)));
  const pick = (b: Turn[]) => b.filter((m) => m["role"] === "assistant").map((m) => m["content"]);
  assert.deepEqual(pick(proj.body), pick(body));
});

test("the goal and the live turn are never stripped", async () => {
  const body = run(8);
  body[0] = reply(1, BIG);
  body[body.length - 1] = reply(2, BIG);
  const proj = await strip(body, [], {}, ctx(Math.floor(estimateTokens(body) * 0.4)));
  assert.ok(String(proj.body[0]!["content"]).includes(BIG));
  assert.ok(String(proj.body[proj.body.length - 1]!["content"]).includes(BIG));
});

test("stripping is monotone and the render is byte-stable between events", async () => {
  const body = run(8);
  const bud = Math.floor(estimateTokens(body) * 0.6);
  const a = await strip(body, [], {}, ctx(bud));
  const b = await strip(body, [], {}, ctx(bud)); // same inputs, no new event
  assert.equal(JSON.stringify(a.body), JSON.stringify(b.body));
  const c = await strip(body, [], {}, ctx(bud * 4)); // the budget RELAXES; nothing comes back
  assert.equal(JSON.stringify(c.body), JSON.stringify(a.body));
});

test("a second event only extends the stripped run", async () => {
  const body = run(8);
  const a = await strip(body, [], {}, ctx(Math.floor(estimateTokens(body) * 0.8)));
  const n1 = a.stripped ?? 0;
  const b = await strip(body, [], {}, ctx(Math.floor(estimateTokens(body) * 0.4)));
  assert.ok((b.stripped ?? 0) > n1);
  assert.deepEqual(
    b.body.slice(0, 2 + 3 * n1).map((m) => m["content"]),
    a.body.slice(0, 2 + 3 * n1).map((m) => m["content"]),
    "the prefix extended, no holes",
  );
});

test("when every body is stripped and prose still overflows, the cut is stage two", async () => {
  const body: Turn[] = [turn("user", "GOAL")];
  for (let i = 0; i < 40; i++) {
    body.push(turn("assistant", `long deliberation ${i} ` + "y".repeat(800)));
    body.push(reply(2000 + i, "z".repeat(50)));
  }
  body.push(turn("user", "now?"));
  const proj = await strip(body, [], {}, ctx(3000));
  assert.equal(proj.omittedMarker, true, "prose alone overflowed; the cut must have fired");
  assert.equal(proj.body[0]!["content"], "GOAL");
  assert.equal(proj.body[proj.body.length - 1]!["content"], "now?");
  assert.ok(estimateTokens(proj.body) <= 3000);
});

test("per-agent state does not leak across agents", async () => {
  const body = run(8);
  await strip(body, [], {}, ctx(Math.floor(estimateTokens(body) * 0.5), "a"));
  const fresh = await strip(body, [], {}, ctx(100_000, "b"));
  assert.equal(fresh.stripped, 0);
});

test("a recall page is trimmed to fit the window and says so", async () => {
  const w = new World({ context_window: 12000, output_reserve: 0 });
  await w.seed(Array.from({ length: 20 }, (_, i) => turn("user", `t${i} ` + "w".repeat(8000))));
  const r = (await recall("ai", { limit: 20 }, w.k)) as Payload;
  const msgs = r["messages"] as Payload[];
  assert.ok(msgs.length > 0 && msgs.length < 20);
  assert.ok(estimateTokens(msgs) <= 6000);
  assert.equal(r["truncated"], true);
  assert.ok(String(r["note"]).includes("before=") && String(r["note"]).includes(String(msgs[0]!["index"])));
  assert.equal(msgs[msgs.length - 1]!["index"], 19, "the NEWEST of what was asked for survives");
});
