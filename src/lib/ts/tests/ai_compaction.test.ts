// The compaction loop end to end, and the provider contract it relies on.
// Mirrors python ai_core/tests/test_memory_features.py (calibrated trigger,
// repeat-read dedupe, the fold summary's budget / re-summarise / cut / refusal,
// the mechanical index, ids minted at the append), plus: the view is projected
// before EVERY turn of a chain (each delivered reply is its own turn), the window
// is re-read every turn, and the OpenAI provider reports usage + stop reason and
// takes a per-call `maxTokens`.

import { test } from "node:test";
import assert from "node:assert/strict";
import type { Kernel } from "../src/kernel/kernel.ts";
import type { Payload } from "../src/kernel/json.ts";
import { estimateOne } from "../src/bundles/ai/context.ts";
import type { ProjectionCtx, Summarizer, Turn } from "../src/bundles/ai/context.ts";
import { stripped } from "../src/bundles/ai/strategies/strip.ts";
import {
  THINK_ALLOWANCE,
  mechanicalIndex,
  summarizeSpan,
  summaryBudget,
} from "../src/bundles/ai/strategies/fold.ts";
import { projectContext } from "../src/bundles/ai/seam.ts";
import { dedupeRepeatBodies } from "../src/bundles/ai/replies.ts";
import { aiMem, loadHistory, saveHistory } from "../src/bundles/ai/memory.ts";
import { defaultSystemContent } from "../src/bundles/ai/howtos.ts";
import { OpenAIProvider, type Message } from "../src/bundles/ai/ai_backend.ts";
import { DONE, Mind, World, backendWorld, callText, settle, turn } from "./ai_world.ts";

test.beforeEach(() => stripped.clear());

// ── the trigger, calibrated by provider-reported usage ──────────────

test("the endpoint's count calibrates the estimate", async () => {
  const w = new World({ context_window: 12000, output_reserve: 0 });
  const msgs: Turn[] = [turn("system", "s"), turn("user", "x".repeat(4000))];
  const rec = w.k.get("ai")!.meta;
  const args = { rec, provider: null, selfId: "ai", kernel: w.k, clientId: "cli", messages: msgs, storeLen: 1 };
  // under the trigger by our own estimate (~1000 of a 12k budget)
  assert.deepEqual(await projectContext(args), msgs);
  assert.equal(aiMem(w.k).projection.get("ai")!["fired"], false);
  // the endpoint said the same prompt was TEN times bigger — the budget shrinks
  aiMem(w.k).calib.set("ai", 10);
  const out = await projectContext(args);
  assert.ok(Array.isArray(out));
  assert.equal(aiMem(w.k).projection.get("ai")!["fired"], true);
  assert.equal(aiMem(w.k).projection.get("ai")!["calibration"], 10);
});

async function send(k: Kernel, text: string): Promise<Payload> {
  const out = (await k.send([{ target: "ai", payload: { type: "send", text } }]))[0]!.result as Payload;
  await settle(k); // the chain the turn started runs to its `done`
  return out;
}

/** What the system block costs in this world — windows below are sized off it. */
async function systemTokens(k: Kernel): Promise<number> {
  return estimateOne({ role: "system", content: await defaultSystemContent("ai", k) });
}

test("a turn records what the endpoint reported", async () => {
  const mind = new Mind(["ok " + DONE]);
  mind.reported = 777;
  const k = backendWorld(mind, { context_window: 32000, output_reserve: 0 });
  await send(k, "hello");
  const st = (await k.send([{ target: "ai", payload: { type: "context_status" } }]))[0]!.result as Payload;
  assert.equal(st["reported_prompt_tokens"], 777);
  assert.equal(st["trigger"], 0.8);
  assert.ok((st["calibration"] as number) > 0);
  assert.equal(st["strategy"], "compact");
  assert.equal(st["budget"], 32000);
});

// ── the same bytes twice: one copy in view ──────────────────────────

test("a repeated body is a stub citing the first", () => {
  const body = "line\n".repeat(200);
  const hist: Turn[] = [
    { role: "user", id: 1, content: `[reply to 10 · notes read]\n${body}` },
    { role: "assistant", id: 2, content: "again" },
    { role: "user", id: 3, content: `[reply to 11 · notes read]\n${body}` },
    { role: "user", id: 4, content: "[reply to 12 · notes read]\nsmall" },
  ];
  const out = dedupeRepeatBodies(hist);
  assert.deepEqual(out[0], hist[0], "the first copy stays whole");
  assert.match(String(out[2]!["content"]), /identical to the reply in row 1/);
  assert.equal(out[2]!["id"], 3);
  assert.deepEqual(out[3], hist[3]);
});

// ── the fold summary: a budget, one retry, then refuse ──────────────

function rows(n = 6, size = 2000): Turn[] {
  return [
    { role: "user", id: 1, content: "the goal: write report.md" },
    ...Array.from({ length: n }, (_, i) => ({ role: "assistant", id: 10 + i, content: "x".repeat(size) })),
  ];
}

function ctx(summarize: Summarizer): ProjectionCtx {
  return { budget: 4000, recentN: 4, summarize, selfId: "ai", kernel: null, clientId: null };
}

test("an oversized summary is re-summarised once, then refused", async () => {
  const asks: string[] = [];
  const verbose: Summarizer = async (msgs) => {
    asks.push(String(msgs[0]!["content"]));
    return "TITLE\n\n" + "word ".repeat(5000);
  };
  assert.equal(await summarizeSpan(ctx(verbose), rows(), [1, 2, 3, 4, 5]), null, "still over budget — nothing folded");
  assert.equal(asks.length, 2);
  assert.ok(asks[1]!.includes("Rewrite it to fit"));
  assert.ok(asks[0]!.includes("AT MOST ~") && asks[0]!.includes("ERRORS:"), "fixed sections, a target");
});

test("a retry that fits is the summary", async () => {
  let n = 0;
  const shrinks: Summarizer = async () => {
    n++;
    return "TITLE\n\n" + (n === 1 ? "word ".repeat(5000) : "short summary");
  };
  assert.deepEqual(await summarizeSpan(ctx(shrinks), rows(), [1, 2, 3, 4, 5]), ["TITLE", "short summary", false]);
});

test("the mechanical index keeps files, identifiers and the user's words", () => {
  const r: Turn[] = [
    { role: "user", id: 1, content: "please check filler_3.txt for deadbeef99" },
    { role: "assistant", id: 2, content: "reading notes/a.md" },
    { role: "user", id: 3, content: "[reply to 9 · notes read]\nbody" },
  ];
  const idx = mechanicalIndex(r, [0, 1, 2]);
  assert.ok(idx.includes("filler_3.txt") && idx.includes("notes/a.md"));
  assert.ok(idx.includes("deadbeef99"));
  assert.ok(idx.includes("user said: please check filler_3.txt for deadbeef99"));
  assert.ok(!idx.includes("[reply to"), "runtime lines are not the user's words");
});

function cutThenFit(cuts: number): Summarizer & { asked: (number | undefined)[] } {
  const asked: (number | undefined)[] = [];
  const s = (async (_msgs: Turn[], maxTokens?: number) => {
    asked.push(maxTokens);
    s.lastCut = asked.length <= cuts;
    return "TITLE\n\n" + (s.lastCut ? "partial" : "whole summary");
  }) as Summarizer & { asked: (number | undefined)[] };
  s.asked = asked;
  return s;
}

test("a summary cut at its budget is re-summarised once, bounded by budget + thinking", async () => {
  const s = cutThenFit(1);
  const c = ctx(s);
  const out = await summarizeSpan(c, rows(), [1, 2, 3, 4, 5]);
  assert.equal(out![1], "whole summary");
  const bound = summaryBudget(c, rows(), [1, 2, 3, 4, 5]) + THINK_ALLOWANCE;
  assert.deepEqual(s.asked, [bound, bound]);
});

test("a summary cut twice is refused — nothing folded", async () => {
  assert.equal(await summarizeSpan(ctx(cutThenFit(2)), rows(), [1, 2, 3, 4, 5]), null);
});

// ── the turn: ids at the append, a projection per turn, the window per turn ──

test("a turn's rows are minted when appended, so id order is append order", async () => {
  const mind = new Mind(["ok " + DONE]);
  const k = backendWorld(mind, { context_window: 32000, output_reserve: 0 });
  mind.midTurn = async () => {
    await saveHistory("ai", k, [{ role: "user", content: "mid" }]);
  };
  await send(k, "hello");
  const hist = await loadHistory("ai", k);
  const ids = hist.map((r) => r["id"] as number);
  assert.deepEqual(ids, [...ids].sort((a, b) => a - b), "append order diverged from id order");
  assert.deepEqual(hist.slice(0, 2).map((r) => r["content"]), ["mid", "hello"]);
});

test("the view is projected before EVERY turn of a chain", async () => {
  // distinct bodies — identical ones would be deduped to one copy in view
  const calls = Array.from({ length: 6 }, (_, i) => callText("tool", { type: "go", size: 1200 + i })).join(" ");
  const mind = new Mind([calls, "read them all " + DONE]);
  const k = backendWorld(mind, { context_window: 100_000, output_reserve: 0, context_strategy: "truncate" });
  // the first turn fits; the turn the six replies arrive in (~1950 tokens) is past
  // the trigger, yet leaves room for the live message itself
  k.updateMeta("ai", { context_window: (await systemTokens(k)) + 2500 });
  const events: Payload[] = [];
  k.onInbox("ai", (e) => {
    if (e["type"] === "context") events.push(e);
  });
  const out = await send(k, "call the tool six times");
  assert.equal(out["error"], undefined, JSON.stringify(out));
  assert.equal(mind.seen.length, 2);
  // (the system block teaches the notice by name; the notice itself is a user turn)
  const noticed = (pass: Message[]) => pass.some((m) => m.role === "user" && m.content.startsWith("[context-notice]"));
  assert.ok(!noticed(mind.seen[0]!), "turn 1 fit");
  assert.ok(noticed(mind.seen[1]!), "turn 2 was projected");
  assert.ok(events.some((e) => e["phase"] === "compacted"));
});

test("a window switched smaller is fitted on the very next turn (downshift)", async () => {
  const mind = new Mind();
  const k = backendWorld(mind, { context_window: 131072, output_reserve: 0, context_strategy: "fold" });
  for (let i = 0; i < 4; i++) {
    mind.turns.push(callText("tool", { type: "go", size: 2000 + i }), `noted ${i} ${DONE}`);
    await send(k, `task ${i}`);
  }
  assert.equal(aiMem(k).projection.get("ai")!["fired"], false, "a large window never compacted");
  k.updateMeta("ai", { context_window: (await systemTokens(k)) + 1200 });
  mind.turns.push("ok " + DONE);
  const out = await send(k, "and now?");
  assert.equal(out["error"], undefined, JSON.stringify(out));
  assert.equal(aiMem(k).projection.get("ai")!["fired"], true, "the smaller window was fitted on the next turn");
});

// ── the provider: max_tokens in, usage + stop reason out ────────────

function sse(lines: object[]): string {
  return lines.map((l) => `data: ${JSON.stringify(l)}\n\n`).join("") + "data: [DONE]\n\n";
}

test("the OpenAI provider sends max_tokens and reports usage and a cut", async () => {
  const realFetch = globalThis.fetch;
  const bodies: Payload[] = [];
  globalThis.fetch = (async (_url: unknown, init: { body: string }) => {
    bodies.push(JSON.parse(init.body) as Payload);
    return new Response(sse([
      { choices: [{ delta: { reasoning_content: "hmm" } }] },
      { choices: [{ delta: { content: "TITLE" }, finish_reason: null }] },
      { choices: [{ delta: {}, finish_reason: "length" }] },
      { choices: [], usage: { prompt_tokens: 321 } },
    ]));
  }) as typeof fetch;
  try {
    const p = new OpenAIProvider({ endpoint: "http://llm.invalid/v1" });
    let text = "";
    for await (const piece of p.chat([{ role: "user", content: "hi" }], 77)) text += piece;
    assert.equal(text, "<think>hmm</think>TITLE");
    assert.equal(p.lastStopReason, "max_tokens");
    assert.equal(p.lastPromptTokens, 321);
    assert.equal(bodies[0]!["max_tokens"], 77);
    assert.deepEqual(bodies[0]!["stream_options"], { include_usage: true });
    // no bound asked ⇒ none sent
    for await (const _ of p.chat([{ role: "user", content: "hi" }])) { /* drain */ }
    assert.equal("max_tokens" in bodies[1]!, false);
  } finally {
    globalThis.fetch = realFetch;
  }
});

test("a stream that ends mid-thought still closes the thought span", async () => {
  const realFetch = globalThis.fetch;
  globalThis.fetch = (async () => new Response(sse([{ choices: [{ delta: { reasoning_content: "still thinking" } }] }]))) as typeof fetch;
  try {
    let text = "";
    for await (const piece of new OpenAIProvider({ endpoint: "http://llm.invalid/v1" }).chat([])) text += piece;
    assert.equal(text, "<think>still thinking</think>");
  } finally {
    globalThis.fetch = realFetch;
  }
});
