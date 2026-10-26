import { test } from "node:test";
import assert from "node:assert/strict";
import { historyTail, collapseReports, keptThoughts } from "../src/bundles/ai_view/ai_view.ts";

const R = "[malformed call — nothing ran] bad verb";
const isRep = (m: Record<string, unknown>): boolean => typeof m["head"] === "string" && (m["head"] as string).includes("[malformed call — nothing ran]");

test("historyTail keeps the newest rows", async () => {
  // 3 pages of 200 rows: ids 1-200, 201-400, 401-600; next 200, 400, then none
  const pages: Record<string, unknown>[] = [
    { rows: Array.from({ length: 200 }, (_, i) => ({ id: i + 1, bytes: 10 })), next: 200 },
    { rows: Array.from({ length: 200 }, (_, i) => ({ id: i + 201, bytes: 10 })), next: 400 },
    { rows: Array.from({ length: 200 }, (_, i) => ({ id: i + 401, bytes: 10 })), next: null },
  ];
  let k = 0;
  const call = async (): Promise<Record<string, unknown> | null> => pages[k++] ?? null;
  const t = await historyTail(call, 300);
  assert.equal(t.rows.length, 300);
  assert.equal(t.rows[0]!["id"], 301);
  assert.equal(t.rows[299]!["id"], 600);
  assert.equal(t.lastId, 600);
  assert.equal(t.bytes, 6000);
});

test("historyTail stops on a null answer", async () => {
  const t = await historyTail(async () => null);
  assert.deepEqual(t, { rows: [], bytes: 0, lastId: 0 });
});

test("collapseReports folds a run of identical reports", () => {
  const rows = [
    { id: 1, head: "a" },
    { id: 2, head: R },
    { id: 3, head: R },
    { id: 4, head: R },
    { id: 5, head: "b" },
    { id: 6, head: R },
  ];
  const out = collapseReports(rows, isRep);
  assert.equal(out.length, 4);
  assert.equal(out[0]!["head"], "a");
  assert.equal(out[1]!["head"], R);
  assert.equal(out[1]!["repeat"], 3);
  assert.equal(out[2]!["head"], "b");
  assert.equal(out[3]!["head"], R);
  assert.equal("repeat" in out[3]!, false);
});

test("collapseReports keeps different heads apart", () => {
  const rows = [
    { id: 1, head: R },
    { id: 2, head: "[malformed call — nothing ran] other" },
  ];
  const out = collapseReports(rows, isRep);
  assert.equal(out.length, 2);
  assert.equal("repeat" in out[0]!, false);
  assert.equal("repeat" in out[1]!, false);
});

test("keptThoughts pages the rows verb from the head, whose ids are not consecutive", async () => {
  // the loaded head, as `rows` holds it: `after` must be the id of a row IN it, else empty
  const head = [
    { id: 101, role: "user" },
    { id: 160, role: "assistant", thinking: "a deep thought", thinking_cut: 3 },
    { id: 161, role: "user" },
    { id: 230, role: "assistant", thinking: "another" },
  ];
  const calls: Record<string, unknown>[] = [];
  const call = async (p: Record<string, unknown>): Promise<Record<string, unknown> | null> => {
    calls.push(p);
    let rows = head;
    if (p["after"] !== undefined) {
      const at = head.findIndex((r) => r.id === p["after"]);
      rows = at < 0 ? [] : head.slice(at + 1);
    }
    const limit = p["limit"] as number;
    const page = rows.slice(0, limit);
    return { rows: page, next: rows.length > limit ? page[page.length - 1]!.id : null };
  };
  const got = await keptThoughts(call, 2);
  assert.deepEqual([...got.entries()], [
    [160, { text: "a deep thought", cut: 3 }],
    [230, { text: "another", cut: 0 }],
  ]);
  assert.equal("after" in calls[0]!, false);
});

test("keptThoughts stops on a null answer", async () => {
  assert.equal((await keptThoughts(async () => null)).size, 0);
});
