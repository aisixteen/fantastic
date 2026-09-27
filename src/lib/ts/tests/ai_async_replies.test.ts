// FULL ASYNC: a reply is a row, not a turn. A settled call's body is inlined only
// when it is small; otherwise it is held in RAM under its pid for this run and
// the model is handed the digest — so a huge reply is a row to grep, never a
// turn that has to fit. Mirrors python ai_core/tests/test_async_replies.py.

import { test } from "node:test";
import assert from "node:assert/strict";
import type { Payload } from "../src/kernel/json.ts";
import { budget, inlineCap } from "../src/bundles/ai/context.ts";
import { aiMem, storeReply } from "../src/bundles/ai/memory.ts";
import { grep } from "../src/bundles/ai/verbs.ts";
import { makeFold } from "../src/bundles/ai/folds.ts";
import { World } from "./ai_world.ts";

async function g(w: World, payload: Payload): Promise<Payload> {
  return (await grep("ai", payload, w.k)) as Payload;
}

test("the inline cap is an eighth of the budget", () => {
  for (const window of [8000, 12000, 32000, 131072]) {
    const rec = { context_window: window, output_reserve: 0 };
    assert.equal(inlineCap(rec), Math.max(256, Math.floor(budget(rec) / 8)));
  }
});

test("a body is held in RAM for this run only — nothing of it reaches the store", async () => {
  const w = new World();
  const body = Array.from({ length: 800 }, (_, i) => `line ${i}\n`).join("");
  const pid = w.k.mintId();
  const before = (await w.stored()).length;
  assert.ok(storeReply("ai", w.k, pid, body));
  assert.equal(aiMem(w.k).bodies.get("ai")!.get(pid), body);
  assert.equal((await w.stored()).length, before, "nothing of the body reached the store");
  aiMem(w.k).bodies.delete("ai"); // the process restarts
  assert.equal((await g(w, { query: "line 799" }))["total"], 0);
});

test("grep finds a line inside a stored body without pulling it", async () => {
  const w = new World();
  const pid = w.k.mintId();
  const body = [
    ...Array(3).fill("chunk 5: " + "the quick brown fox ".repeat(40)),
    ">> audit marker: a1b2c3d4",
    ...Array(3).fill("the quick brown fox ".repeat(40)),
  ].join("\n");
  storeReply("ai", w.k, pid, body);
  await w.seed([
    w.row("user", "survey the corpus", "user:0001"),
    w.row("assistant", "Reading filler_5.", "ai:0002", "user:0001"),
    w.row("user", `[reply to ${pid} · notes read · "survey filler_5"]\n(settled, 9000 bytes — NOT inlined. Pull a page: …)`, "ai:0003"),
  ]);
  const r = await g(w, { query: "AUDIT MARKER" });
  assert.equal(r["total"], 1);
  assert.equal(r["truncated"], false);
  const m = (r["matches"] as Payload[])[0]!;
  assert.equal(m["id"], pid);
  assert.equal(m["line"], 4);
  assert.ok(String(m["text"]).includes("a1b2c3d4"));
});

test("grep scopes to ids and caps matches", async () => {
  const w = new World();
  const p1 = w.k.mintId();
  const p2 = w.k.mintId();
  storeReply("ai", w.k, p1, Array.from({ length: 50 }, (_, i) => `hit ${i}`).join("\n"));
  storeReply("ai", w.k, p2, "hit elsewhere");
  await w.seed([w.row("user", `[reply to ${p1} · n r]\nx\n\n[reply to ${p2} · n r]\ny`, "u:0")]);
  const r = await g(w, { query: "hit", ids: [p1], limit: 5 });
  assert.equal(r["total"], 50);
  assert.equal((r["matches"] as Payload[]).length, 5);
  assert.equal(r["truncated"], true);
  assert.ok((r["matches"] as Payload[]).every((m) => m["id"] === p1));
});

test("grep refuses without a query", async () => {
  const r = await g(new World(), {});
  assert.equal(r["reason"], "bad_arg");
  assert.equal(r["arg"], "query");
});

test("grep puts the stored body hit first and collapses repeated lines", async () => {
  // Measured 2026-09-06 (python): the task line repeated per delivery matched 28
  // times and pushed the one body hit past the cap.
  const w = new World();
  const pid = w.k.mintId();
  await w.seed([
    ...Array.from({ length: 6 }, () => w.row("user", "find the audit marker in the files", null)),
    w.row("user", `[reply to ${pid} · notes read]\n(settled, 9 bytes — NOT inlined)`, null),
  ]);
  storeReply("ai", w.k, pid, "chunk 1\n>> audit marker: deadbeef\nchunk 1");
  const r = await g(w, { query: "audit marker" });
  const first = (r["matches"] as Payload[])[0]!;
  assert.equal(first["id"], pid);
  assert.ok(String(first["text"]).includes("deadbeef"));
  assert.equal(r["total"], 2, JSON.stringify(r));
});

test("grep scoped to a fold searches its members' bodies", async () => {
  const w = new World();
  const callId = w.k.mintId();
  const pid = w.k.mintId();
  const replyId = w.k.mintId();
  await w.seed([
    w.row("assistant", "read filler_1", callId),
    w.row("user", `[reply to ${pid} · notes read · "read filler_1" · ↑${callId}]\n(settled, 9 bytes — NOT inlined)`, replyId, callId),
  ]);
  storeReply("ai", w.k, pid, "chunk 1\n>> audit marker: cafebabe\nchunk 1");
  const fold = makeFold("ai", [callId, replyId], "read filler_1", "plain filler", false, { id: w.k.mintId() });
  await w.seed([fold]);
  const r = await g(w, { query: "audit marker", ids: [fold["id"] as number] });
  assert.equal(r["total"], 1);
  assert.equal((r["matches"] as Payload[])[0]!["id"], pid);
  assert.ok(String((r["matches"] as Payload[])[0]!["text"]).includes("cafebabe"));
  const r2 = await g(w, { query: "audit marker", ids: [pid] });
  assert.equal(r2["total"], 1, "an id that is itself a pid still reaches its body");
});

test("grep width is the model's to raise", async () => {
  const w = new World();
  const pid = w.k.mintId();
  await w.seed([w.row("user", `[reply to ${pid} · notes read]\n(settled, 9 bytes — NOT inlined)`, null)]);
  storeReply("ai", w.k, pid, "rule: " + "x".repeat(300) + " VALUE=cafebabe");
  let r = await g(w, { query: "rule:" });
  const text = String((r["matches"] as Payload[])[0]!["text"]);
  assert.equal(text.length, 200);
  assert.ok(!text.includes("cafebabe"));
  r = await g(w, { query: "rule:", width: 400 });
  assert.ok(String((r["matches"] as Payload[])[0]!["text"]).includes("cafebabe"));
});

test("grep falls back per knob — a bad limit leaves width intact", async () => {
  const w = new World();
  const pid = w.k.mintId();
  await w.seed([w.row("user", `[reply to ${pid} · notes read]\n(settled — NOT inlined)`, null)]);
  storeReply("ai", w.k, pid, "rule: " + "x".repeat(300) + " VALUE=cafebabe");
  const r = await g(w, { query: "rule:", limit: "nope" });
  assert.ok((r["matches"] as Payload[]).length > 0);
  assert.equal(String((r["matches"] as Payload[])[0]!["text"]).length, 200);
});
