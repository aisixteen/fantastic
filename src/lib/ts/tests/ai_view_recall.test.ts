// The view array is the agent's state; `context_recall` rebuilds it; every
// self-read verb answers under the inline cap. Mirrors python
// ai_core/tests/test_view_recall.py.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Payload } from "../src/kernel/json.ts";
import { estimateOne, inlineCap } from "../src/bundles/ai/context.ts";
import type { Turn } from "../src/bundles/ai/context.ts";
import { stripped } from "../src/bundles/ai/strategies/strip.ts";
import { assemble, projectContext } from "../src/bundles/ai/seam.ts";
import type { Provider } from "../src/bundles/ai/seam.ts";
import { resOf } from "../src/bundles/ai/replies.ts";
import { aiMem, loadHistory, loadView, saveView, storeReply } from "../src/bundles/ai/memory.ts";
import { contextRecall, fitReply, grep, history } from "../src/bundles/ai/verbs.ts";
import { makeAIBackend, type AIRecord } from "../src/bundles/ai/ai_backend.ts";
import { deliver } from "../src/bundles/ai/routing.ts";
import { registerRamStore } from "../src/bundles/memory/ram_store.ts";
import { BIG, World } from "./ai_world.ts";

test.beforeEach(() => stripped.clear());

// ── the cap invariant ────────────────────────────────────────────────

test("fitReply never exceeds the cap and says so", () => {
  const items = Array.from({ length: 200 }, (_, i) => ({ id: i, text: "y".repeat(300) }));
  const [kept, cut] = fitReply(items, 500);
  assert.ok(cut && kept.length > 0 && kept.length < 200);
  assert.ok(estimateOne({ role: "user", content: JSON.stringify(kept) }) <= 500);
  const [kept2, cut2] = fitReply(items.slice(0, 2), 10_000);
  assert.deepEqual(kept2, items.slice(0, 2));
  assert.equal(cut2, false);
});

test("grep answers under the inline cap for a broad query", async () => {
  const w = new World();
  const rows = Array.from({ length: 40 }, (_, i) =>
    w.row("user", Array.from({ length: 80 }, (_, j) => `marker ${i}.${j}`).join("\n"), `u:${i}`));
  const pids = Array.from({ length: 10 }, () => w.k.mintId());
  pids.forEach((pid, i) =>
    storeReply("ai", w.k, pid, Array.from({ length: 500 }, (_, j) => `marker line ${i}.${j}`).join("\n")));
  await w.seed(rows);
  const r = (await grep("ai", { query: "marker" }, w.k)) as Payload;
  const cap = inlineCap(w.k.get("ai")!.meta);
  assert.ok(estimateOne({ role: "user", content: JSON.stringify(r) }) <= cap + 64);
  assert.equal(r["truncated"], true);
  assert.ok((r["next"] as Payload)["id"] !== undefined);
});

test("history is digest resolution and paged", async () => {
  const w = new World();
  await w.seed(Array.from({ length: 30 }, (_, i) => w.row("user", BIG, `u:${i}`)));
  const r = (await history("ai", {}, w.k)) as Payload;
  const rows = r["rows"] as Payload[];
  assert.equal(r["total"], 30);
  assert.ok(rows.length > 0 && rows.every((x) => "head" in x && "bytes" in x));
  assert.ok(rows.every((x) => String(x["head"]).length <= 120), "no bodies in a digest");
  if (r["next"] !== null) {
    const r2 = (await history("ai", { after: r["next"] }, w.k)) as Payload;
    assert.notEqual((r2["rows"] as Payload[])[0]!["id"], rows[0]!["id"]);
  }
});

// ── the view array ───────────────────────────────────────────────────

test("the view round-trips and assembly builds from it", async () => {
  const w = new World();
  const a = w.chain("a", 3);
  await w.seed(a);
  const ids = [
    { id: a[0]!["id"] as number, res: "full" },
    { id: a[2]!["id"] as number, res: "stub" },
    { id: a[4]!["id"] as number, res: "full" },
  ];
  await saveView("ai", w.k, ids, { budget: 1000, why: "test" });
  const v = await loadView("ai", w.k);
  assert.deepEqual(v!["ids"], ids);
  assert.equal(v!["why"], "test");
  const { messages, storeLen } = await assemble("ai", w.k, "live", "sys");
  assert.equal(storeLen, a.length);
  const bodyRows = messages.slice(1, -1);
  // the array, in order, then every row appended after its newest member
  assert.deepEqual(bodyRows.map((m) => m["id"]), [...ids.map((e) => e.id), ...a.slice(5).map((r) => r["id"])]);
  assert.ok(String(bodyRows[1]!["content"]).includes("(stripped:") && !String(bodyRows[1]!["content"]).includes(BIG));
  assert.equal(aiMem(w.k).viewSource.get("ai"), "array");
});

test("a stale array falls back to the store and says so", async () => {
  const w = new World();
  const a = w.chain("a", 1, false);
  await w.seed(a);
  await saveView("ai", w.k, [{ id: 99_999_999, res: "full" }], { budget: 1, why: "x" });
  const { messages } = await assemble("ai", w.k, "live", "sys");
  assert.equal(messages.length, 1 + a.length + 1);
  assert.ok(String(aiMem(w.k).viewSource.get("ai")).startsWith("store"));
});

test("resOf classifies rows", () => {
  const w = new World();
  assert.equal(resOf(w.fold(["x:1", "x:2"], "t", "b")), "fold");
  assert.equal(resOf({ role: "user", content: "[reply to n:1 · n r]\n(stripped: 10 bytes — recall by pid)" }), "stub");
  assert.equal(resOf({ role: "user", content: "[reply to n:1 · n r]\nbody" }), "full");
});

// ── context_recall ───────────────────────────────────────────────────

test("the mechanical fit is under budget and persists a view and a recall row", async () => {
  const w = new World();
  await w.seed([w.row("user", "GOAL", "user:goal"), ...w.chain("a", 20)]);
  const r = (await contextRecall("ai", { dry: true }, w.k, null, "s".repeat(4000))) as Payload;
  assert.equal(r["degraded"], true);
  assert.ok((r["kept"] as number) >= 1);
  const v = await loadView("ai", w.k);
  assert.equal((v!["ids"] as Payload[])[0]!["id"], w.id("user:goal"), "the goal turn is always in");
  const rec = (await loadHistory("ai", w.k)).filter((x) => x["kind"] === "recall");
  assert.equal(rec.length, 1);
  assert.ok(String(rec[0]!["content"]).includes("[recall "));
  const r2 = (await contextRecall("ai", { dry: true }, w.k, null, "s".repeat(4000))) as Payload;
  assert.ok((r2["kept"] as number) >= (r["kept"] as number) - 1);
});

test("recall asks the model and expands only what fits", async () => {
  const w = new World();
  const a = w.chain("a", 12);
  await w.seed([w.row("user", "GOAL", "user:goal"), ...a]);
  const replyIds = new Set(a.filter((r) => String(r["content"]).startsWith("[reply to ")).map((r) => r["id"]));
  const p: Provider = {
    async *chat(messages) {
      const headIds = messages[0]!.content.split("\n")
        .filter((ln) => ln.includes(" · ") && /^\d+$/.test(ln.split(" · ")[0]!))
        .map((ln) => Number(ln.split(" · ")[0]));
      const want = [...headIds.filter((i) => replyIds.has(i)).slice(-2), 999_999_999];
      yield JSON.stringify({ expand: want, why: "the last two replies matter" });
    },
  };
  const r = (await contextRecall("ai", {}, w.k, p, "s".repeat(4000))) as Payload;
  assert.equal(r["degraded"], false);
  assert.equal(r["why"], "the last two replies matter");
  const v = await loadView("ai", w.k);
  const full = (v!["ids"] as Payload[]).filter((e) => e["res"] === "full" && replyIds.has(e["id"] as number));
  assert.ok(full.length >= 1 && full.length <= 2, "expansions are the model's, capped by budget; a ghost id is ignored");
});

test("recall clears RAM scratch but never the store", async () => {
  const w = new World();
  await w.seed([w.row("user", "GOAL", "user:goal"), ...w.chain("a", 8)]);
  aiMem(w.k).projection.set("ai", { fired: true });
  aiMem(w.k).compactionMark.set("ai", 3);
  stripped.set("ai", new Set([1]));
  const before = (await loadHistory("ai", w.k)).length;
  await contextRecall("ai", { dry: true }, w.k, null, "s");
  assert.ok(!aiMem(w.k).projection.has("ai") && !aiMem(w.k).compactionMark.has("ai") && !stripped.has("ai"));
  assert.equal((await loadHistory("ai", w.k)).length, before + 1, "one recall row appended, nothing removed");
});

function backendKernel(meta: Payload = {}, provider?: Provider): { k: Kernel; calls: () => number } {
  const k = new Kernel();
  registerRamStore(k);
  k.registerBundle(
    "ai.ts",
    makeAIBackend((id) => k.get(id)!.meta as AIRecord, provider !== undefined ? { makeProvider: () => provider } : {}),
  );
  k.setRoot(new Agent({ id: "kernel_state", meta: { guid: "g1" } }));
  k.register(new Agent({ id: "mem", parentId: "kernel_state", handlerModule: "ram_store.ts", meta: { operator: true, admins: ["kernel"], callable_by: { ai: "*" } } }));
  k.register(new Agent({ id: "ai", parentId: "kernel_state", handlerModule: "ai.ts", meta: { memory_store_id: "mem", can_call: { mem: "*" }, ...meta } }));
  const calls = (): number => [...k.store.rows.values()].filter((r) => r["kind"] === "call" && r["agent"] === "mem").length;
  return { k, calls };
}

test("boot is lazy and marks the view for rebuilding", async () => {
  const { k, calls } = backendKernel();
  const before = calls();
  const out = (await k.send([{ target: "ai", payload: { type: "boot" } }]))[0]!;
  assert.equal(out.result, null);
  assert.equal(calls(), before, "boot read nothing");
  assert.ok(aiMem(k).recallDue.has("ai"), "the first turn rebuilds the view");
});

test("boot without a memory store fails loud", async () => {
  const { k } = backendKernel();
  delete k.get("ai")!.meta["memory_store_id"];
  const out = (await k.send([{ target: "ai", payload: { type: "boot" } }]))[0]!.result as Payload;
  assert.equal(out["reason"], "missing_arg");
  assert.equal(out["arg"], "memory_store_id");
});

test("a reply in flight across a reboot is recorded, not acted on", async () => {
  // Measured 2026-09-06 (python): the rebooted agent was woken by answers to calls
  // its previous incarnation made, and finished THAT work instead of the new one.
  let passes = 0;
  const provider: Provider = {
    async *chat(messages) {
      // a TURN opens with the system block; `context_recall`'s side call does not
      if (messages[0]?.role === "system") passes += 1;
      yield '<call name="done"></call>';
    },
  };
  const { k } = backendKernel({ context_window: 12000, output_reserve: 0 }, provider);
  await k.send([{ target: "ai", payload: { type: "boot" } }]); // incarnation 1
  const born = aiMem(k).incarnation.get("ai")!;
  await k.send([{ target: "ai", payload: { type: "boot" } }]); // reboot → incarnation 2
  await deliver(k, "ai", "[reply to n:1 · n r]\nlate", "c", null, born);
  assert.equal(passes, 0, "a late reply runs no turn");
  let rows = await loadHistory("ai", k);
  assert.equal(rows[rows.length - 1]!["kind"], "late");
  assert.ok(String(rows[rows.length - 1]!["content"]).includes("late"));
  await deliver(k, "ai", "[reply to n:2 · n r]\nfresh", "c", null, aiMem(k).incarnation.get("ai")!);
  assert.equal(passes, 1, "a reply from THIS incarnation runs a turn");
  rows = await loadHistory("ai", k);
  assert.ok(rows.some((r) => String(r["content"]).includes("fresh")));
});

test("a fold batch makes recall due on the next turn, and every projection writes the view", async () => {
  const w = new World();
  const b = [
    { role: "system", content: "sys" },
    w.row("user", "GOAL", "user:goal"),
    ...w.chain("a", 12),
    w.row("user", "now?", null),
  ] as Turn[];
  await w.persistBody(b.slice(1));
  const out = await projectContext({
    rec: { context_window: 2500, output_reserve: 0, context_strategy: "fold" },
    provider: null, selfId: "ai", kernel: w.k, clientId: "cli", messages: b, storeLen: b.length - 1,
  });
  assert.ok(Array.isArray(out));
  assert.ok(aiMem(w.k).recallDue.has("ai"));
  assert.ok((await loadView("ai", w.k)) !== null, "every projection writes the view");
});

test("rows appended after the array ride behind it", async () => {
  const w = new World();
  const a = w.chain("a", 4, false);
  await w.seed(a);
  await saveView("ai", w.k, [{ id: a[0]!["id"] as number, res: "full" }, { id: a[2]!["id"] as number, res: "full" }], { budget: 1000, why: "t" });
  await w.seed([
    w.row("assistant", "I just read filler_9", "ai:new1", a[a.length - 1]!["id"] as number),
    w.reply("ai:new2", "ai:new1", "filler_9 body"),
  ]);
  const { messages } = await assemble("ai", w.k, "live", "sys");
  const ids = messages.slice(1, -1).map((m) => m["id"]);
  assert.deepEqual(ids.slice(0, 2), [a[0]!["id"], a[2]!["id"]], "the array first");
  assert.deepEqual(ids.slice(-2), [w.id("ai:new1"), w.id("ai:new2")], "then everything appended since");
  assert.equal(aiMem(w.k).viewSource.get("ai"), "array");
});

test("the recall row names the newest root", async () => {
  const w = new World();
  await w.seed([w.row("user", "TASK 0: first", "user:t0"), ...w.chain("a", 2), w.row("user", "TASK 1: latest work", "user:t1")]);
  await contextRecall("ai", { dry: true }, w.k, null, "sys");
  const rec = (await loadHistory("ai", w.k)).filter((x) => x["kind"] === "recall").at(-1)!;
  assert.ok(String(rec["content"]).includes(`live task: ${w.id("user:t1")} "TASK 1: latest work"`), String(rec["content"]));
});

test("the live root is found even when a fold collapsed it", async () => {
  const w = new World();
  const b = w.chain("b", 2);
  await w.seed([w.row("user", "TASK 0: first", "user:t0"), ...w.chain("a", 2), w.row("user", "TASK 1: latest work", "user:t1"), ...b]);
  await w.seed([w.fold(["user:t1", ...b.map((r) => r["id"] as number)], "task 1 folded", "body")]);
  await contextRecall("ai", { dry: true }, w.k, null, "sys");
  const rec = (await loadHistory("ai", w.k)).filter((x) => x["kind"] === "recall").at(-1)!;
  // chain b's own root ("task b") is the newest root, and the fold collapsed it too
  assert.ok(String(rec["content"]).includes(`live task: ${w.id("user:b")}`), String(rec["content"]));
});
