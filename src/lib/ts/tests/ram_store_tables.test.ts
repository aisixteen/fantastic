// ram_store's TABLES, pinned whole: row order (`order` + insort), the FK
// cascades through `fold_refs` and the reverse member index, and every refusal's
// full shape (`{error, reason, arg|id…}`, nothing more).
//
// The expected replies were produced by python's ram_store
// (bundled_agents/memory/ram_store/tools.py) from the same calls, in the same
// order, on 2026-10-01; the grant tables were removed 2026-10-03.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import { registerRamStore } from "../src/bundles/memory/ram_store.ts";
import { sendAs } from "./kit.ts";

const ROOT = "kernel_state";

function store(): Kernel {
  const k = new Kernel();
  registerRamStore(k);
  k.setRoot(new Agent({ id: ROOT, meta: { guid: "g1" } }));
  for (const id of ["alice", "bob", "carol"]) {
    k.register(new Agent({ id, parentId: ROOT, meta: { can_call: { mem: "*" } } }));
  }
  k.register(
    new Agent({
      id: "mem",
      parentId: ROOT,
      handlerModule: "ram_store.ts",
      meta: { callable_by: { alice: "*", bob: "*", carol: "*" } },
    }),
  );
  return k;
}

/** One call to `mem` as `who` (null = the operator). */
async function call(k: Kernel, who: string | null, payload: Payload): Promise<Json> {
  const items = [{ target: "mem", payload }];
  return (await (who === null ? k.send(items) : sendAs(k, who, items)))[0]!.result;
}

const BAD = (arg: string, error: string): Payload => ({ error, reason: "bad_arg", arg });
const MISSING = (arg: string, error: string): Payload => ({ error, reason: "missing_arg", arg });

const THREE = { id: 3, content: "three" };
const SEVEN = { id: 7, kind: "fold", refs: [10, 3], content: "Fold Seven" };
const TEN = { id: 10, content: "ten" };

async function seeded(): Promise<Kernel> {
  const k = store();
  assert.deepEqual(await call(k, "alice", { type: "append", rows: [{ id: 10, content: "ten" }, { id: 3, content: "three" }] }), {
    ids: [10, 3],
    appended: 2,
  });
  assert.deepEqual(
    await call(k, "alice", { type: "append", rows: [{ id: 7, kind: "fold", refs: [10, 3], content: "Fold Seven" }] }),
    { ids: [7], appended: 1 },
  );
  return k;
}

test("rows appended out of id order read back in id order, every selector included", async () => {
  const k = await seeded();
  assert.deepEqual(await call(k, "alice", { type: "read" }), { rows: [THREE, SEVEN, TEN], next: null, total: 3 });
  assert.deepEqual(await call(k, "alice", { type: "read", order: "desc" }), { rows: [TEN, SEVEN, THREE], next: null, total: 3 });
  assert.deepEqual(await call(k, "alice", { type: "read", after: 3 }), { rows: [SEVEN, TEN], next: null, total: 2 });
  assert.deepEqual(await call(k, "alice", { type: "read", from_last_kind: "fold" }), { rows: [SEVEN, TEN], next: null, total: 2 });
  assert.deepEqual(await call(k, "alice", { type: "read", order: "desc", after: 10, limit: 1 }), { rows: [SEVEN], next: 7, total: 2 });
  // the dump walks the same order; a fold's members live in fold_refs, not the row
  assert.deepEqual(await call(k, null, { type: "dump" }), {
    tables: {
      rows: [
        { owner: "g1|alice", row: THREE },
        { owner: "g1|alice", row: { id: 7, kind: "fold", content: "Fold Seven" } },
        { owner: "g1|alice", row: TEN },
      ],
      fold_refs: [{ owner: "g1|alice", fold: 7, members: [10, 3] }],
    },
  });
});

test("append, search and prune refusals, whole", async () => {
  const k = await seeded();
  const ROWS = BAD("rows", "append: `rows` must be a non-empty list of rows");
  const cases: [string, Payload, Payload][] = [
    ["alice", { type: "append", rows: [] }, ROWS],
    ["alice", { type: "append", rows: "x" }, ROWS],
    ["alice", { type: "append", rows: [1] }, ROWS],
    ["alice", { type: "append" }, ROWS],
    ["alice", { type: "append", rows: [{ id: 20, refs: "x" }] }, BAD("rows", "append: row 20 refs must be row ids")],
    ["alice", { type: "append", rows: [{ id: 21, refs: [1.5] }] }, BAD("rows", "append: row 21 refs must be row ids")],
    ["alice", { type: "append", rows: [{ id: 22, refs: [true] }] }, BAD("rows", "append: row 22 refs must be row ids")],
    ["alice", { type: "search", query: "  " }, MISSING("query", "search: query required")],
    ["alice", { type: "search" }, MISSING("query", "search: query required")],
    ["alice", { type: "prune" }, MISSING("before_id", "prune: before_id required")],
    ["alice", { type: "prune", before_id: "x" }, MISSING("before_id", "prune: before_id required")],
  ];
  for (const [who, payload, want] of cases) {
    const got = await call(k, who, payload);
    assert.deepEqual(got, want, JSON.stringify(payload));
    assert.deepEqual(Object.keys(got as Payload), ["error", "reason", "arg"], JSON.stringify(payload));
  }
  // nothing above changed a row
  assert.deepEqual(await call(k, "alice", { type: "read" }), { rows: [THREE, SEVEN, TEN], next: null, total: 3 });
});

test("inject and dump are plain verbs; every inject refusal, whole, and none of them changes the store", async () => {
  const k = await seeded();
  assert.deepEqual(await call(k, "alice", { type: "dump" }), await call(k, null, { type: "dump" }));
  const before = await call(k, null, { type: "dump" });
  const T = BAD.bind(null, "tables");
  const ROW1 = { owner: "g1|a", row: { id: 1 } };
  const cases: [Json, Payload][] = [
    [5, T("inject: `tables` must be a dump dict")],
    [[1], T("inject: `tables` must be a dump dict")],
    [{ rows: 5 }, T("inject: tables.rows must be a list")],
    [{ rows: [5] }, T("inject: every row needs {owner, row}")],
    [{ rows: [{ row: { id: 1 } }] }, T("inject: every row needs {owner, row}")],
    [{ rows: [{ owner: 5, row: { id: 1 } }] }, T("inject: every row needs {owner, row}")],
    [{ rows: [{ owner: "g1|a", row: {} }] }, T("inject: every row needs an int id")],
    [{ rows: [{ owner: "g1|a", row: 5 }] }, T("inject: every row needs an int id")],
    [{ rows: [{ owner: "g1|a", row: { id: "x" } }] }, T("inject: every row needs an int id")],
    [{ rows: [ROW1, ROW1] }, { error: "inject: duplicate row (g1|a, 1)", reason: "exists", id: 1 }],
    [{ rows: [ROW1], fold_refs: [{ owner: "g1|a", fold: 1, members: [99] }] }, T("inject: fold 1 refs a missing row")],
    [{ rows: [ROW1], fold_refs: [{ owner: "g1|b", fold: 1, members: [] }] }, T("inject: fold_refs names no row 1")],
  ];
  for (const [tables, want] of cases) {
    assert.deepEqual(await call(k, null, { type: "inject", tables }), want, JSON.stringify(tables));
    assert.deepEqual(await call(k, null, { type: "dump" }), before, `a refused inject changed the store: ${JSON.stringify(tables)}`);
  }
  // absent / null sections are empty: the whole store is replaced by nothing
  assert.deepEqual(await call(k, null, { type: "inject", tables: { rows: null, fold_refs: null } }), { rows: 0 });
  assert.deepEqual(await call(k, null, { type: "dump" }), { tables: { rows: [], fold_refs: [] } });
});

test("FK cascades: a pruned fold leaves its members' index, a pruned member leaves its folds", async () => {
  const k = store();
  const a = (p: Payload) => call(k, "alice", p);
  assert.deepEqual(await a({ type: "append", rows: [{ id: 1 }, { id: 2 }, { id: 3, kind: "fold", refs: [1, 2] }, { id: 4 }, { id: 5 }] }), {
    ids: [1, 2, 3, 4, 5],
    appended: 5,
  });
  assert.deepEqual(await a({ type: "prune", ids: [4] }), { pruned: 1 });
  // the fold itself: its member list goes with it
  assert.deepEqual(await a({ type: "prune", ids: [3] }), { pruned: 1 });
  assert.deepEqual(await call(k, null, { type: "dump" }), {
    tables: {
      rows: [
        { owner: "g1|alice", row: { id: 1 } },
        { owner: "g1|alice", row: { id: 2 } },
        { owner: "g1|alice", row: { id: 5 } },
      ],
      fold_refs: [],
    },
  });
  // a member of the pruned fold goes cleanly; a NEW fold under the old id holds only what it names
  assert.deepEqual(await a({ type: "prune", ids: [1] }), { pruned: 1 });
  assert.deepEqual(await a({ type: "append", rows: [{ id: 3, kind: "fold", refs: [2] }] }), { ids: [3], appended: 1 });
  assert.deepEqual(await a({ type: "read", ids: [3] }), { rows: [{ id: 3, kind: "fold", refs: [2] }], next: null, total: 1 });
  // pruning the member: it leaves the fold
  assert.deepEqual(await a({ type: "prune", ids: [2] }), { pruned: 1 });
  assert.deepEqual(await a({ type: "read", ids: [3] }), { rows: [{ id: 3, kind: "fold", refs: [] }], next: null, total: 1 });
  assert.deepEqual(await call(k, null, { type: "dump" }), {
    tables: {
      rows: [
        { owner: "g1|alice", row: { id: 3, kind: "fold" } },
        { owner: "g1|alice", row: { id: 5 } },
      ],
      fold_refs: [{ owner: "g1|alice", fold: 3, members: [] }],
    },
  });
});

test("`keep` spares a kind; a prune that matches nothing removes nothing", async () => {
  const k = await seeded();
  assert.deepEqual(await call(k, "alice", { type: "prune", ids: [99] }), { pruned: 0 });
  assert.deepEqual(await call(k, "alice", { type: "prune", before_id: 1 }), { pruned: 0 });
  assert.deepEqual(await call(k, "alice", { type: "read" }), { rows: [THREE, SEVEN, TEN], next: null, total: 3 });
  assert.deepEqual(await call(k, "alice", { type: "prune", before_id: 99, keep: ["fold"] }), { pruned: 2 });
  assert.deepEqual(await call(k, "alice", { type: "read" }), { rows: [{ id: 7, kind: "fold", refs: [], content: "Fold Seven" }], next: null, total: 1 });
});
