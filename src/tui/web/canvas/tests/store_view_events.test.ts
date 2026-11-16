import { test } from "node:test";
import assert from "node:assert/strict";
import { file, ownCopiesReadNothing, run } from "./view_harness.ts";

test("the store view re-reads on the store's published writes, never on a copy of a call", async () => {
  const h = run(file("src/lib/rust/crates/bundles/fantastic-sqlite-store/src/store_view.html"), { agent: "mem" }, () => ({ query_allow: [{ file: "mem.db", size: 4096 }] }));
  await h.settle();
  assert.ok(h.watches.has("mem"), "the view watches its store");
  assert.deepEqual(h.intervals, [], "no timer");
  await ownCopiesReadNothing(h);
  for (const copy of [{ type: "reflect" }, { type: "append", rows: [{ content: "a" }] }, { type: "read" }, { type: "prune", ids: [1] }, { type: "query", db: "mem.db", sql: "select 1" }]) {
    const mark = h.sends.length;
    await h.publish("mem", copy);
    assert.deepEqual(h.since(mark), [], `a copy read again: ${JSON.stringify(copy)}`);
  }
  for (const ev of [{ type: "appended", table: "rows", id: 7 }, { type: "pruned", n: 1 }, { type: "injected", rows: 2 }, { type: "bytes_put", ref: "p1" }]) {
    const mark = h.sends.length;
    await h.publish("mem", ev);
    assert.deepEqual(h.since(mark).map((s) => String(s.p["type"])), ["reflect"], `${ev.type}: the sizes are read again`);
  }
});
