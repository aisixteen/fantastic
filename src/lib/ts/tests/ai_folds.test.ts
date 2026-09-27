// Folds are marks: chains derived from `parent`, stretches collapse to an appended
// row with refs, the view hides the members, and every one stays one descent
// away. Mirrors python ai_core/tests/test_folds.py.

import { test } from "node:test";
import assert from "node:assert/strict";
import type { Payload } from "../src/kernel/json.ts";
import * as folds from "../src/bundles/ai/folds.ts";
import { estimateTokens } from "../src/bundles/ai/context.ts";
import type { Summarizer, Turn } from "../src/bundles/ai/context.ts";
import { getStrategy } from "../src/bundles/ai/strategies/index.ts";
import { KEEP_TAIL, nextSpan } from "../src/bundles/ai/strategies/fold.ts";
import { stripped } from "../src/bundles/ai/strategies/strip.ts";
import { dedupeStaleReads, dropAnsweredReflects } from "../src/bundles/ai/replies.ts";
import { grep, history } from "../src/bundles/ai/verbs.ts";
import { World, interleave } from "./ai_world.ts";

const fold = getStrategy("fold")!;

test.beforeEach(() => stripped.clear());

// ── chains and closure ──────────────────────────────────────────────

test("chains are derived from parent links across an interleave", () => {
  const w = new World();
  const rows = interleave(w.chain("a", 2, false), w.chain("b", 3, false), w.chain("c", 1, false, true));
  const got = folds.chains(rows);
  assert.equal(got.length, 3);
  assert.deepEqual(new Set(got.map((ch) => rows[ch[0]!]!["id"])), new Set([w.id("user:a"), w.id("user:b"), w.id("user:c")]));
  for (const ch of got) {
    for (let j = 1; j < ch.length; j++) assert.equal(rows[ch[j]!]!["parent"], rows[ch[j - 1]!]!["id"]);
  }
});

test("a chain ending on an unanswered send is open", () => {
  const w = new World();
  const rows = w.chain("z", 1, false, true);
  assert.ok(folds.isOpen(rows, folds.chains(rows)[0]!));
  const closed = w.chain("y", 1, false);
  assert.ok(!folds.isOpen(closed, folds.chains(closed)[0]!));
});

test("rows without ids are their own roots", () => {
  assert.equal(folds.chains([{ role: "user", content: "old" }, { role: "assistant", content: "older" }]).length, 2);
});

// ── render ──────────────────────────────────────────────────────────

test("a fold hides its refs and sits where its first ref was", () => {
  const w = new World();
  const rows = [...w.chain("a", 2, false), w.row("user", "later", "user:l")];
  const f = w.fold(rows.slice(0, 5).map((r) => r["id"] as number), "done a", "body a");
  const view = folds.render([...rows, f]);
  const ids = view.map((r) => r["id"]);
  assert.equal(ids[0], f["id"], "the fold renders at the first ref's position");
  assert.ok(!view.some((r) => (f["refs"] as number[]).includes(r["id"] as number)));
  assert.equal(ids[ids.length - 1], w.id("user:l"));
  assert.ok(String(view[0]!["content"]).startsWith("[fold ") && String(view[0]!["content"]).includes('"done a"'));
});

test("a fold of folds hides the inner fold", () => {
  const w = new World();
  const rows = w.chain("a", 1, false);
  const inner = w.fold(rows.map((r) => r["id"] as number), "inner", "b");
  const outer = w.fold([inner["id"] as number], "outer", "b");
  assert.deepEqual(folds.render([...rows, inner, outer]).map((r) => r["id"]), [outer["id"]]);
});

test("expandIds descends through folds", () => {
  const w = new World();
  const rows = w.chain("a", 1, false);
  const inner = w.fold(rows.map((r) => r["id"] as number), "i", "b");
  const outer = w.fold([inner["id"] as number, "other:1"], "o", "b");
  const got = folds.expandIds([...rows, inner, outer], [outer["id"] as number]);
  assert.deepEqual(new Set(got), new Set([...rows.map((r) => r["id"]), w.id("other:1")]));
});

// ── the strategy ────────────────────────────────────────────────────

test("fold collapses the oldest stretch first and persists it", async () => {
  const w = new World();
  const mid = interleave(w.chain("a", 3), w.chain("b", 3), w.chain("c", 2, true, true));
  const body = await w.persistBody([w.row("user", "GOAL", "user:goal"), ...mid, w.row("user", "now?", null)]);
  const bud = Math.floor(estimateTokens(body) * 0.45);
  const proj = await fold(body, [], {}, w.ctx(bud));
  assert.ok((proj.folded ?? 0) >= 1);
  assert.equal((await w.folds()).length, proj.folded, "every fold row was persisted");
  const blob = JSON.stringify(proj.body);
  assert.ok(blob.includes("GOAL") && blob.includes("now?"), "goal and live turn never fold");
  const tail = mid.slice(-6).map((r) => r["id"]);
  assert.ok(tail.every((i) => proj.body.some((r) => r["id"] === i)), "the live tail never folds");
  assert.ok(estimateTokens(proj.body) <= bud);
});

test("a fold is monotone across projections", async () => {
  const w = new World();
  const body = await w.persistBody([w.row("user", "GOAL", "user:goal"), ...w.chain("a", 2), ...w.chain("b", 2), w.row("user", "now?", null)]);
  const bud = Math.floor(estimateTokens(body) * 0.5);
  const p1 = await fold(body, [], {}, w.ctx(bud));
  const persisted = (await w.folds());
  const body2 = [...body.slice(0, -1), ...persisted, body[body.length - 1]!];
  const p2 = await fold(body2, [], {}, w.ctx(bud * 10));
  assert.equal(p2.folded, 0);
  const visible = (p: { body: Turn[] }) => new Set(p.body.filter(folds.isFold).map((r) => r["id"]));
  assert.deepEqual(visible(p2), visible(p1), "the SAME collapse renders");
});

test("a failed summarizer still writes a fold and says so", async () => {
  const w = new World();
  const boom: Summarizer = async () => {
    throw new Error("provider down");
  };
  const body = await w.persistBody([w.row("user", "GOAL", "user:goal"), ...w.chain("a", 8), w.row("user", "now?", null)]);
  const proj = await fold(body, [], {}, w.ctx(Math.floor(estimateTokens(body) * 0.6), boom));
  assert.ok((proj.folded ?? 0) >= 1);
  const f = (await w.folds())[0]!;
  assert.match(String(f["content"]), /summary unavailable/);
  assert.match(String(f["title"]), /read a0/, "degraded title is the members' titles");
});

// ── descent through the verbs ───────────────────────────────────────

test("grep and history descend into a fold", async () => {
  const w = new World();
  const rows = w.chain("a", 1, false);
  rows[2] = { ...rows[2]!, content: String(rows[2]!["content"]).replace("ok", "the secret is 9f3a") };
  const f = w.fold(rows.map((r) => r["id"] as number), "done a", "summary of a");
  await w.seed([...rows, f]);
  const r = (await grep("ai", { query: "secret", ids: [f["id"] as number] }, w.k)) as Payload;
  assert.equal(r["total"], 1);
  assert.equal((r["matches"] as Payload[])[0]!["id"], rows[2]!["id"]);
  const h = (await history("ai", { fold: f["id"] as number }, w.k)) as Payload;
  assert.equal((h["fold"] as Payload)["title"], "done a");
  assert.equal((h["fold"] as Payload)["summary"], "summary of a");
  assert.deepEqual((h["rows"] as Payload[]).map((d) => d["id"]), rows.map((x) => x["id"]));
});

// ── the prerequisite: views keep row keys ───────────────────────────

test("the view trimmers keep row keys", () => {
  const turn: Turn = {
    role: "user",
    id: "u:1",
    parent: "a:0",
    content: "[reply to n:1 · notes reflect]\nTREE\n\n[reply to n:2 · notes read]\nx",
  };
  const out = dropAnsweredReflects([turn, { role: "assistant", content: "SEEN notes" }, { ...turn, id: "u:2" }]);
  assert.equal(out[0]!["id"], "u:1");
  assert.equal(out[0]!["parent"], "a:0");
  const out2 = dedupeStaleReads([turn, { role: "assistant", content: "k" }, { ...turn, id: "u:2" }]);
  assert.ok(out2.filter((m) => m["role"] === "user").every((m) => "id" in m));
});

// ── edge cases ──────────────────────────────────────────────────────

test("an open superlong chain folds its oldest stretch and keeps the tail", async () => {
  const w = new World();
  const ch = w.chain("a", 12, true, true); // 25 rows, still awaiting a reply
  const body = await w.persistBody([w.row("user", "GOAL", "user:goal"), ...ch, w.row("user", "now?", null)]);
  const span = nextSpan(body, folds.render(body));
  assert.ok(span.length >= 2 && !span.includes(0));
  const tailIds = new Set(ch.slice(-5).map((r) => r["id"]));
  assert.ok(!span.some((i) => tailIds.has(body[i]!["id"])), "the live tail never folds");
  assert.notEqual(body[span[span.length - 1]!]!["role"], "assistant", "a span never ends between a call and its answer");
  const proj = await fold(body, [], {}, w.ctx(Math.floor(estimateTokens(body) * 0.4)));
  assert.ok((proj.folded ?? 0) >= 1);
  assert.ok(JSON.stringify(proj.body).includes("one more"), "the open call at the tail is still in view");
});

test("folds are ordinary members, so nesting needs no second stage", async () => {
  const w = new World();
  const chs = "abcdefghijklmnop".split("").map((t) => w.chain(t, 1, false));
  const rows = [w.row("user", "GOAL", "user:goal"), ...interleave(...chs)];
  for (const c of chs) rows.push(w.fold(c.map((r) => r["id"] as number), `done ${String(c[0]!["id"])}`, "S".repeat(1500)));
  rows.push(w.row("user", "now?", null));
  await w.persistBody(rows);
  const proj = await fold(rows, [], {}, w.ctx(1200));
  const top = (await w.folds()).filter((r) => (r["level"] as number) >= 2);
  assert.ok(top.length > 0, "a fold whose members were folds was written by the SAME path");
  assert.ok(top.every((r) => r["body"] && r["title"]), "every level carries a summary");
  assert.ok(proj.body.length > 0);
});

test("fold of folds terminates when even folds do not fit", async () => {
  const w = new World();
  const rows: Turn[] = [w.row("user", "GOAL", "user:goal")];
  for (const t of "ab") {
    const c = w.chain(t, 1, false);
    rows.push(...c, w.fold(c.map((r) => r["id"] as number), `done ${t}`, "S".repeat(5000)));
  }
  rows.push(w.row("user", "now?", null));
  await w.persistBody(rows);
  const proj = await fold(rows, [], {}, w.ctx(300)); // absurdly small
  assert.ok(proj !== null); // it returned — no infinite loop
});

test("a title cannot close the tag or quote itself", () => {
  const w = new World();
  const f = w.fold(["x:1", "x:2"], 'weird "title] here', "b");
  const head = String(f["content"]).split("\n")[0]!;
  assert.equal(head.split("]").length - 1, 1);
  assert.ok(head.endsWith(`"weird 'title) here"`));
});

test("the summarizer input is capped", () => {
  const rows = Array.from({ length: 6 }, (_, i) => ({ role: "user", content: "z".repeat(50_000), id: i + 1 }));
  assert.ok(folds.chainText(rows, [0, 1, 2, 3, 4, 5]).length <= folds.SUMMARY_INPUT_CAP);
});

test("a fold written twice over the same stretch is not written twice", async () => {
  const w = new World();
  const body = await w.persistBody([w.row("user", "GOAL", "user:goal"), ...w.chain("a", 8), w.row("user", "now?", null)]);
  const bud = Math.floor(estimateTokens(body) * 0.5);
  const p1 = await fold(body, [], {}, w.ctx(bud));
  const body2 = [...body.slice(0, -1), ...(await w.folds()), body[body.length - 1]!];
  const p2 = await fold(body2, [], {}, w.ctx(bud));
  assert.equal((await w.folds()).length, (p1.folded ?? 0) + (p2.folded ?? 0), "no duplicate fold over an already-folded stretch");
});

test("overlapping folds never show a member twice", () => {
  const w = new World();
  const a = w.chain("a", 2, false);
  const f1 = w.fold(a.slice(0, 3).map((r) => r["id"] as number), "part", "b");
  const f2 = w.fold(a.map((r) => r["id"] as number), "whole", "b");
  const ids = folds.render([...a, f1, f2]).map((r) => r["id"]);
  assert.equal(ids.length, new Set(ids).size);
  assert.ok(!ids.some((i) => a.some((r) => r["id"] === i)));
});

test("a fold whose member was pruned still renders and expands", () => {
  const w = new World();
  const a = w.chain("a", 1, false);
  const f = w.fold([...a.map((r) => r["id"] as number), "gone:0000"], "done", "b");
  assert.deepEqual(folds.render([...a, f]).map((r) => r["id"]), [f["id"]]);
  assert.ok(folds.expandIds([...a, f], [f["id"] as number]).includes(w.id("gone:0000")));
});

test("a late reply whose parent is folded stays visible", () => {
  const w = new World();
  const a = w.chain("a", 1, false);
  const f = w.fold(a.map((r) => r["id"] as number), "done", "b");
  const late = w.reply("ai:late", "ai:a0", "late reply");
  assert.deepEqual(folds.render([...a, f, late]).map((r) => r["id"]), [f["id"], w.id("ai:late")]);
});

test("folding does not build a linked list of summaries", async () => {
  const w = new World();
  const body = await w.persistBody([w.row("user", "GOAL", "user:goal"), ...w.chain("a", 30), w.row("user", "now?", null)]);
  await fold(body, [], {}, w.ctx(Math.floor(estimateTokens(body) * 0.35)));
  const levels = (await w.folds()).map((r) => r["level"] as number);
  assert.ok(levels.length >= 3, `${levels}`);
  assert.ok(Math.max(...levels) <= 3, `${levels}`);
});

test("a fold inherits its first member's parent", () => {
  const w = new World();
  const a = w.chain("a", 1, false);
  const f = w.fold([a[1]!["id"] as number, a[2]!["id"] as number], "t", "b", false, { parent: a[1]!["parent"]! });
  assert.equal(f["parent"], w.id("user:a"));
  assert.deepEqual(folds.chains([...a, f])[0], [0, 1, 2, 3], "the fold sits in the chain it collapsed");
});

test("three adjacent folds at the head fold as a group", () => {
  const w = new World();
  const goal = w.row("user", "GOAL", "user:goal");
  const heads = [0, 1, 2].map((i) => w.fold([], `fold ${i}`, "body"));
  const tail = Array.from({ length: KEEP_TAIL + 1 }, (_, i) => w.row("user", `t${i}`, `user:t${i}`));
  const rows = [goal, ...heads, ...tail];
  assert.deepEqual(nextSpan(rows, rows), [1, 2, 3]);
});

test("the head is coalesced even when the view already fits", async () => {
  const w = new World();
  const goal = w.row("user", "GOAL", "user:goal");
  const src = Array.from({ length: 6 }, (_, i) => w.row("user", `s${i}`, `user:s${i}`));
  const heads = [0, 1, 2].map((i) =>
    w.fold([src[2 * i]!["id"] as number, src[2 * i + 1]!["id"] as number], `f${i}`, "b", false, { parent: src[2 * i]!["parent"]! }),
  );
  const tail = Array.from({ length: 7 }, (_, i) => w.row("user", `t${i}`, `user:t${i}`));
  const body = await w.persistBody([goal, ...src, ...heads, ...tail]);
  await fold(body, [], {}, w.ctx(estimateTokens(body) * 100)); // comfortably under budget
  const levels = (await w.folds()).map((r) => r["level"] as number);
  assert.ok(levels.length > 0 && Math.max(...levels) >= 2, `the head was left uncoalesced: ${levels}`);
});
