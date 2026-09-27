// Deterministic, no-LLM tests for the context-budget primitives, the overflow
// strategies, and the seam + verbs (the ONE notice, the `context` events,
// `recall`, the derived `last_reaction`, `context_clear`). Mirrors python
// ai_core/tests/test_context.py (its cli-renderer case has no ts counterpart:
// this kernel has no `cli` agent).

import { test } from "node:test";
import assert from "node:assert/strict";
import type { Payload } from "../src/kernel/json.ts";
import {
  DEFAULT_CONTEXT_WINDOW,
  budget,
  estimateOne,
  estimateTokens,
  resolveContextWindow,
} from "../src/bundles/ai/context.ts";
import type { ProjectionCtx, Summarizer, Turn } from "../src/bundles/ai/context.ts";
import { DEFAULT_STRATEGY, getStrategy, strategyNames } from "../src/bundles/ai/strategies/index.ts";
import { STUB_SUMMARY } from "../src/bundles/ai/strategies/base.ts";
import { stripped } from "../src/bundles/ai/strategies/strip.ts";
import { assemble, contextNotice, projectContext } from "../src/bundles/ai/seam.ts";
import type { Provider } from "../src/bundles/ai/seam.ts";
import {
  REFLECT_NUDGE,
  REPLY_TAG,
  dedupeStaleReads,
  distilled,
  dropAnsweredReflects,
} from "../src/bundles/ai/replies.ts";
import { aiMem, loadHistory, loadView, saveHistory } from "../src/bundles/ai/memory.ts";
import { contextClear, deriveReaction, grep, recall } from "../src/bundles/ai/verbs.ts";
import { SEND_TO_HOWTO } from "../src/bundles/ai/howtos.ts";
import { World, body, callText, turn } from "./ai_world.ts";

test.beforeEach(() => stripped.clear());

const fakeSummary: Summarizer = async () => "SUMMARY";

function ctx(bud: number, recentN = 4, summarize: Summarizer | null = fakeSummary, w?: World): ProjectionCtx {
  return { budget: bud, recentN, summarize, selfId: "ai", kernel: w?.k ?? null, clientId: null };
}

const fakeProvider: Provider = {
  async *chat() {
    yield "FAKESUMMARY";
  },
};

function msgs(n: number): Turn[] {
  return [turn("system", "sys"), ...body(n)];
}

/** Every `context` event the agent pushed to its caller. */
function contextEvents(w: World): Payload[] {
  const seen: Payload[] = [];
  w.k.onInbox("ai", (e) => {
    if (e["type"] === "context") seen.push(e);
  });
  return seen;
}

async function project(w: World, rec: Payload, messages: Turn[], clientId = "cli") {
  return projectContext({ rec, provider: fakeProvider, selfId: "ai", kernel: w.k, clientId, messages, storeLen: messages.length - 1 });
}

// ── primitives ──────────────────────────────────────────────────────

test("the estimate scales and is monotonic", () => {
  assert.ok(estimateOne(turn("user", "x".repeat(400))) >= 100);
  assert.ok(estimateTokens(body(4)) > estimateTokens(body(2)));
});

test("window precedence: context_window, then num_ctx, then the default", () => {
  assert.equal(resolveContextWindow({ context_window: 8000, num_ctx: 2000 }), 8000);
  assert.equal(resolveContextWindow({ num_ctx: 2000 }), 2000);
  assert.equal(resolveContextWindow({}), DEFAULT_CONTEXT_WINDOW);
});

test("the budget is floored", () => {
  assert.equal(budget({ context_window: 100, output_reserve: 1000 }), 256);
});

test("the registry: compact, truncate, strip, fold — an unknown name is null", () => {
  assert.deepEqual(new Set(strategyNames()), new Set(["compact", "truncate", "strip", "fold"]));
  assert.equal(DEFAULT_STRATEGY, "compact");
  assert.equal(getStrategy("memgpt"), null);
  assert.equal(getStrategy("nope"), null);
});

// ── strategies return a Projection, never a fabricated turn ─────────

test("compact returns a projection with a summary and pins the goal", async () => {
  const b = body(10);
  const proj = await getStrategy("compact")!(b, [turn("system", "S")], {}, ctx(1_000_000));
  assert.equal(proj.summary, "SUMMARY");
  assert.deepEqual(proj.body, [b[0], ...b.slice(-4)]);
});

test("truncate returns a projection and never calls the summarizer", async () => {
  let called = 0;
  const spy: Summarizer = async () => {
    called++;
    return "S";
  };
  const b = body(10);
  const proj = await getStrategy("truncate")!(b, [], {}, ctx(1_000_000, 4, spy));
  assert.equal(proj.omittedMarker, true);
  assert.equal(proj.summary, undefined);
  assert.deepEqual(proj.body[0], b[0]);
  assert.deepEqual(proj.body.slice(-4), b.slice(-4));
  assert.equal(called, 0);
});

test("tool pairing never orphans a role:tool", async () => {
  const b: Turn[] = [
    turn("user", "u0"),
    { role: "assistant", content: "a", tool_calls: [{ id: "1" }] },
    { role: "tool", tool_call_id: "1", content: "r" },
    turn("assistant", "done"),
    turn("user", "u-live"),
  ];
  const proj = await getStrategy("compact")!(b, [], {}, ctx(1_000_000, 3));
  assert.notEqual(proj.body[0]?.["role"], "tool");
});

test("a summarizer failure degrades to the stub", async () => {
  const boom: Summarizer = async () => {
    throw new Error("nope");
  };
  const proj = await getStrategy("compact")!(body(10), [], {}, ctx(1_000_000, 4, boom));
  assert.equal(proj.summary, STUB_SUMMARY);
});

test("a tight budget keeps the live turn", async () => {
  const b = body(10);
  const proj = await getStrategy("truncate")!(b, [], {}, ctx(80));
  assert.deepEqual(proj.body[proj.body.length - 1], b[b.length - 1]);
});

// ── the ONE context-notice ──────────────────────────────────────────

test("the notice for compact carries the summary and the affordances", () => {
  const n = contextNotice("compact", "MY SUMMARY", false, 7);
  assert.equal(n["role"], "user");
  const c = String(n["content"]);
  assert.ok(c.includes("[context-notice]") && c.includes("MY SUMMARY"));
  assert.ok(c.includes("recall") && c.includes("memory agent"));
  assert.ok(c.includes("7 earlier turn"));
});

test("the notice for truncate says a span was omitted", () => {
  const c = String(contextNotice("truncate", null, true, 3)["content"]);
  assert.ok(c.includes("omitted in place") && c.includes("recall"));
  assert.ok(!c.includes("Summary of the dropped span"));
});

// ── the projection seam ─────────────────────────────────────────────

test("the seam short-circuits under the trigger: no notice, no event", async () => {
  const w = new World();
  const seen = contextEvents(w);
  const m = [turn("system", "S"), turn("user", "hi")];
  const out = await project(w, {}, m);
  assert.deepEqual(out, m);
  assert.deepEqual(aiMem(w.k).projection.get("ai"), { fired: false, trigger: 0.8, calibration: 1 });
  assert.deepEqual(seen, []);
});

test("the seam fires: fits, prepends the notice, pushes context:compacted", async () => {
  const w = new World();
  const seen = contextEvents(w);
  const m = msgs(20);
  const out = await project(w, { context_window: 500, output_reserve: 0, context_strategy: "compact" }, m);
  assert.ok(Array.isArray(out));
  assert.ok(estimateTokens(out) <= 500);
  assert.equal(out[0]!["role"], "system");
  assert.ok(String(out[1]!["content"]).includes("[context-notice]"));
  const proj = aiMem(w.k).projection.get("ai")!;
  assert.equal(proj["fired"], true);
  assert.equal(proj["strategy"], "compact");
  assert.equal(proj["summarized"], true);
  assert.ok((proj["dropped_messages"] as number) > 0);
  assert.ok(!("fired_at_index" in proj), "the public record carries no scan bookkeeping");
  assert.ok(aiMem(w.k).compactionMark.has("ai"));
  assert.equal(seen.length, 1);
  assert.equal(seen[0]!["phase"], "compacted");
  assert.equal((seen[0]!["detail"] as Payload)["strategy"], "compact");
  assert.equal(seen[0]!["client_id"], "cli");
});

test("the notice is never in the history projected from", async () => {
  const w = new World();
  const m = msgs(20);
  await project(w, { context_window: 500, output_reserve: 0 }, m, "web1");
  assert.ok(!JSON.stringify(m).includes("[context-notice]"));
});

test("too_small: a failfast that pushes an event and errors", async () => {
  const w = new World();
  const seen = contextEvents(w);
  const out = await project(w, { context_window: 1000, output_reserve: 0 }, [turn("system", "S".repeat(8000)), turn("user", "hi")]);
  assert.ok(!Array.isArray(out) && out.error.includes("context_insufficient"));
  assert.equal(aiMem(w.k).projection.get("ai")!["too_small"], true);
  assert.equal(seen.length, 1);
  assert.equal(seen[0]!["phase"], "too_small");
  assert.ok("hint" in (seen[0]!["detail"] as Payload));
});

test("an unknown strategy errors without an event", async () => {
  const w = new World();
  const seen = contextEvents(w);
  const out = await project(w, { context_window: 500, output_reserve: 0, context_strategy: "bogus" }, msgs(20));
  assert.ok(!Array.isArray(out) && out.error.includes("bogus"));
  assert.deepEqual(seen, []);
});

// ── recall: a read-only page-back over the store ────────────────────

test("recall filters, pages and returns whole turns", async () => {
  const w = new World();
  await w.seed([
    turn("user", "the project codename is HALCYON"),
    turn("assistant", "noted"),
    turn("user", "tell me about bicycles"),
    turn("assistant", "x".repeat(5000)),
    turn("user", "what about codename again"),
  ]);
  const r = (await recall("ai", { query: "codename" }, w.k)) as Payload;
  assert.equal(r["total"], 2);
  const ms = r["messages"] as Payload[];
  assert.ok(ms.every((m) => String(m["content"]).toLowerCase().includes("codename")));
  assert.equal(ms[0]!["index"], 0);
  const r2 = (await recall("ai", { limit: 1 }, w.k)) as Payload;
  assert.equal((r2["messages"] as Payload[]).length, 1);
  assert.equal(r2["truncated"], true);
  const r3 = (await recall("ai", { before: 2 }, w.k)) as Payload;
  assert.deepEqual(new Set((r3["messages"] as Payload[]).map((m) => m["index"])), new Set([0, 1]));
  const big = (await recall("ai", { query: "xxxx" }, w.k)) as Payload;
  assert.equal(String((big["messages"] as Payload[])[0]!["content"]).length, 5000, "recall returns the turn whole");
});

test("an empty recall page says why", async () => {
  const w = new World();
  const r = (await recall("ai", {}, w.k)) as Payload;
  assert.equal(r["total"], 0);
  assert.match(String(r["note"]), /compacted/);
  await w.seed([turn("user", "hello")]);
  const rq = (await recall("ai", { query: "zzz" }, w.k)) as Payload;
  assert.equal(rq["total"], 0);
  assert.match(String(rq["note"]), /matched none/);
});

// ── context_status.last_reaction — the derived ack ──────────────────

test("no reaction before any compaction", async () => {
  const w = new World();
  assert.equal(await deriveReaction("ai", w.k), null);
  aiMem(w.k).projection.set("ai", { fired: false });
  assert.equal(await deriveReaction("ai", w.k), null);
});

test("the reaction detects a recall to itself and a persist", async () => {
  const w = new World();
  await w.seed([
    turn("user", "old turn"),
    turn("user", "what's my codename?"),
    turn("assistant", callText("ai", { type: "recall", query: "codename" })),
    turn("assistant", callText("notes", { type: "write", path: "facts.md", text: "HALCYON" })),
    turn("assistant", "It's HALCYON."),
  ]);
  aiMem(w.k).projection.set("ai", { fired: true });
  aiMem(w.k).compactionMark.set("ai", 1);
  assert.deepEqual(await deriveReaction("ai", w.k), { recalled: true, persisted: true, recall_count: 1 });
});

test("the persist verbs are text_file's: write/patch/append, not the dead KV ones", async () => {
  for (const [verb, want] of [["write", true], ["patch", true], ["append", true], ["set", false], ["replace", false]] as const) {
    const w = new World();
    await w.seed([turn("assistant", callText("notes", { type: verb, path: "facts.md" }))]);
    aiMem(w.k).projection.set("ai", { fired: true });
    aiMem(w.k).compactionMark.set("ai", 0);
    assert.equal((await deriveReaction("ai", w.k))!["persisted"], want, verb);
  }
});

test("the reaction cursor is the STORE index, not the deduped view's", async () => {
  const w = new World();
  await w.seed([
    turn("user", "the ask: " + "x".repeat(1200)),
    w.reply(null, null, "A".repeat(1200), "reflect"),
    w.reply(null, null, "B".repeat(1200), "reflect"),
    // written BEFORE the notice — not a reaction to a compaction not yet fired
    turn("assistant", callText("notes", { type: "write", path: "facts.md", text: "HALCYON" })),
  ]);
  const { messages, storeLen } = await assemble("ai", w.k, "and now?", "sys");
  assert.equal(storeLen, 4);
  assert.equal(messages.length, 1 + (4 - 1) + 1, "system + deduped view + live");
  const out = await projectContext({
    rec: { context_window: 500, output_reserve: 0, context_strategy: "compact" },
    provider: fakeProvider, selfId: "ai", kernel: w.k, clientId: "cli", messages, storeLen,
  });
  assert.ok(Array.isArray(out));
  assert.equal(aiMem(w.k).compactionMark.get("ai"), 4);
  assert.deepEqual(await deriveReaction("ai", w.k), { recalled: false, persisted: false, recall_count: 0 });
});

// ── the GOAL survives every strategy, and the seam ──────────────────

test("every strategy keeps the goal when the body overflows", async () => {
  const goal = "GOAL-SENTINEL: seal the secrets bridge and open the assets bridge.";
  for (const name of strategyNames()) {
    stripped.clear();
    const w = new World();
    const b = [turn("user", goal), ...body(40, 400)];
    const proj = await getStrategy(name)!(b, [], {}, ctx(1200, 4, fakeSummary, w));
    const carried = proj.body.some((m) => String(m["content"] ?? "").includes(goal));
    const summarized = typeof proj.summary === "string" && proj.summary.includes(goal);
    assert.ok(carried || summarized, `strategy ${name} dropped the task-framing turn`);
  }
});

for (const strategy of ["compact", "fold", "strip", "truncate"]) {
  for (const window of [300, 400, 500, 700, 900, 1200]) {
    test(`the seam keeps the goal the strategies pinned (${strategy}@${window})`, async () => {
      stripped.clear();
      const w = new World();
      const goal = "GOAL-SENTINEL: build the widget and report when done";
      const m = [turn("system", "sys"), turn("user", goal), ...body(40)];
      const out = await project(w, { context_window: window, output_reserve: 0, context_strategy: strategy }, m);
      assert.ok(Array.isArray(out), JSON.stringify(out));
      assert.ok(estimateTokens(out) <= window, "the projection must still fit");
      assert.ok(out.some((x) => String(x["content"] ?? "").includes(goal)), `${strategy}@${window}: the goal was dropped`);
    });
  }
}

// ── reflect bulk: distilled by the reader, then dropped ─────────────

test("answered reflects are dropped, the in-flight one survives", () => {
  const w = new World();
  const b = [
    w.reply(null, null, "TREE".repeat(2000), "reflect", "notes"),
    turn("assistant", "SEEN notes — key/value store; verbs read/set/keys."),
    w.reply(null, null, "TREE".repeat(2000), "reflect", "assets"),
  ];
  const out = dropAnsweredReflects(b);
  assert.ok(!String(out[0]!["content"]).includes("TREE") && String(out[0]!["content"]).includes("notes"));
  assert.deepEqual(out[1], b[1]);
  assert.deepEqual(out[2], b[2]);
});

test("stale whole-state reads dedupe; parameterised reads never do", () => {
  const w = new World();
  const b = [
    w.reply(null, null, "OLD TREE", "reflect", "kernel_state"),
    turn("assistant", "noted"),
    w.reply(null, null, "reading_1 = 41", "read", "readings"),
    w.reply(null, null, "reading_2 = 42", "read", "readings"),
    w.reply(null, null, "NEW TREE", "reflect", "kernel_state"),
  ];
  const contents = dedupeStaleReads(b).map((m) => String(m["content"]));
  assert.ok(!contents.some((c) => c.includes("OLD TREE")));
  assert.ok(contents.some((c) => c.includes("NEW TREE")));
  assert.ok(contents.some((c) => c.includes("reading_1 = 41")));
  assert.ok(contents.some((c) => c.includes("reading_2 = 42")));
});

test("a reflect the model did not distil is kept", () => {
  const w = new World();
  const b = [
    w.reply(null, null, "TREE".repeat(500), "reflect", "notes"),
    turn("assistant", callText("x", { type: "reflect" })), // no SEEN
    w.reply(null, null, "FACTS".repeat(500), "reflect", "assets"),
    turn("assistant", "SEEN assets — public web assets; verbs read/list."),
    turn("user", "next"),
  ];
  const out = dropAnsweredReflects(b);
  assert.ok(String(out[0]!["content"]).includes("TREE"), "undistilled reflect must be KEPT");
  assert.ok(!String(out[2]!["content"]).includes("FACTS"), "distilled reflect is dropped");
  assert.ok(String(out[2]!["content"]).includes("assets"), "the stub still names the agent");
});

test("the distillation instruction rides on the reflect reply itself", () => {
  assert.ok(REFLECT_NUDGE.includes("SEEN"));
  assert.ok(REFLECT_NUDGE.trimEnd().endsWith("]"));
  assert.ok(distilled("SEEN notes — durable key/value store; verbs: read/set/keys"));
  assert.ok(!SEND_TO_HOWTO.includes("SEEN"), "the format lives at point of use only");
});

test("a combined turn trims per block, never whole", () => {
  const combined =
    "[reply to ks:a1 · kernel_state reflect · 1 of 2 in batch ab12]\n" +
    "TREE".repeat(500) +
    "\n\n[reply to tot:b1 · totals read · 2 of 2 in batch ab12]\n" +
    "reading_1 = 41";
  const b = [
    turn("user", combined),
    turn("assistant", "SEEN kernel_state — the root; verbs create/list."),
    turn("user", "[reply to ks:a2 · kernel_state reflect]\nNEW TREE"),
  ];
  const out = dropAnsweredReflects(b);
  assert.ok(!String(out[0]!["content"]).includes("TREE".repeat(5)), "distilled reflect block drops");
  assert.ok(String(out[0]!["content"]).includes("reading_1 = 41"), "sibling block must SURVIVE");
  const out2 = dedupeStaleReads(b);
  const first = String(out2[0]!["content"]);
  assert.ok(!first.includes("TREE".repeat(5)), "older whole-state block is superseded");
  assert.ok(first.includes("reading_1 = 41"), "parameterised sibling must survive");
  assert.ok(out2.some((m) => String(m["content"]).includes("NEW TREE")));
});

test("a turn whose every block is superseded drops entirely", () => {
  const out = dedupeStaleReads([
    turn(
      "user",
      "[reply to ks:a1 · kernel_state reflect · 1 of 2 in batch cd34]\nOLD" +
        "\n\n[reply to k:a2 · kernel ps · 2 of 2 in batch cd34]\nOLD ROWS",
    ),
    turn("assistant", "noted"),
    turn("user", "[reply to ks:a3 · kernel_state reflect]\nNEW"),
    turn("user", "[reply to k:a4 · kernel ps]\nNEW ROWS"),
  ]);
  assert.ok(!out.some((m) => String(m["content"]).includes("OLD")));
  assert.equal(out.length, 3, "the emptied turn leaves no husk behind");
});

test("the verb group stops at the tag's own bracket", () => {
  const verb = (s: string): string | undefined => REPLY_TAG.exec(s)?.[2];
  assert.equal(verb('[reply to notes:aa · notes reflect]\nverbs=["read", "write"]  root=.'), "reflect");
  assert.equal(verb('[reply to notes:aa · notes reflect · 2 of 3 in batch ab12]\nv=["x"]'), "reflect");
  assert.equal(verb("[reply to a:1 · notes read]\nplain body"), "read");
  assert.equal(REPLY_TAG.exec("[reply to a:1 notes reflect]"), null);
});

test("a stale reflect whose body has brackets still dedupes", () => {
  const w = new World();
  const bodyText = 'verbs=["read", "write", "patch"]  root=.';
  const out = dedupeStaleReads([
    w.reply(null, null, bodyText, "reflect"),
    turn("assistant", "SEEN notes"),
    w.reply(null, null, bodyText, "reflect"),
  ]);
  assert.equal(out.filter((m) => String(m["content"]).includes(bodyText)).length, 1);
});

// ── context_clear: a new thread, nothing deleted ────────────────────

test("context_clear starts a new thread without deleting", async () => {
  const w = new World();
  await saveHistory("ai", w.k, [
    { role: "user", content: "the codename is HALCYON" },
    { role: "assistant", content: "noted" },
  ]);
  assert.equal((await loadHistory("ai", w.k)).length, 2);
  const out = (await contextClear("ai", { why: "fresh start" }, w.k)) as Payload;
  assert.equal(out["rows"], 2);
  const hist = await loadHistory("ai", w.k);
  assert.equal(hist.length, 1);
  assert.equal(hist[0]!["kind"], "clear");
  assert.ok(String(hist[0]!["content"]).startsWith(`[cleared ${String(out["cleared"])} · 2 rows] fresh start`));
  assert.equal((hist[0]!["refs"] as number[]).length, 2, "the clear-fold's members are the earlier rows");
  assert.ok(!JSON.stringify(hist).includes("HALCYON"));
  assert.equal((await loadHistory("ai", w.k, { behind: true })).length, 3, "behind the boundary, everything is still there");
  assert.equal(((await recall("ai", { query: "codename" }, w.k)) as Payload)["total"], 1);
  assert.equal(((await grep("ai", { query: "halcyon" }, w.k)) as Payload)["total"], 1);
  assert.deepEqual((await loadView("ai", w.k))!["ids"], [], "the persisted view is emptied");
  await saveHistory("ai", w.k, [{ role: "user", content: "hello again" }]);
  const after = await loadHistory("ai", w.k);
  assert.equal(after[after.length - 1]!["content"], "hello again");
});
