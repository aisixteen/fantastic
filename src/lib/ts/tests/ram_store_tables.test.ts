// ram_store's TABLES, pinned whole: row order (`order` + insort), range scopes,
// the identical-share dedup (`grantKey`), the FK cascades through `fold_refs`,
// the reverse member index and the grant anchors, and every refusal's full
// shape (`{error, reason, arg|id|side…}`, nothing more).
//
// Every expected reply here was produced by python's ram_store
// (bundled_agents/memory/ram_store/tools.py) from the same calls, in the same
// order, on 2026-10-01 — byte-identical once `ts` and `created_by` (a clock and a
// task id) are set aside.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import { registerRamStore } from "../src/bundles/memory/ram_store.ts";

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
      meta: { operator: true, admins: ["kernel"], callable_by: { alice: "*", bob: "*", carol: "*" } },
    }),
  );
  return k;
}

/** One call to `mem` as `who` (null = the operator, an admin here). */
async function call(k: Kernel, who: string | null, payload: Payload): Promise<Json> {
  const via = who === null ? k : k.scopedFor(who, null);
  return (await via.send([{ target: "mem", payload }]))[0]!.result;
}

/** A grants listing without its clock and its task id. */
function views(r: Json): Payload[] {
  return ((r as Payload)["grants"] as Payload[]).map((g) => {
    const { ts, created_by, ...rest } = g;
    assert.equal(typeof ts, "number");
    assert.ok(Number.isInteger(created_by), `created_by is the share's task id: ${String(created_by)}`);
    return rest;
  });
}

/** The operator's dump, each grant without its clock and its task id. */
async function dumpNoClock(k: Kernel): Promise<Payload> {
  const tables = (await call(k, null, { type: "dump" }) as Payload)["tables"] as Payload;
  const grants = (tables["grants"] as Payload[]).map((g) => {
    const { ts, created_by, ...rest } = g;
    assert.equal(typeof ts, "number");
    assert.ok(Number.isInteger(created_by));
    return rest;
  });
  return { ...tables, grants };
}

const BAD = (arg: string, error: string): Payload => ({ error, reason: "bad_arg", arg });
const NOT_FOUND = (arg: string, error: string): Payload => ({ error, reason: "not_found", arg });
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
      grants: [],
    },
  });
});

test("range scopes: anchored at the lowest row inside; frozen keeps its bound, live follows the head", async () => {
  const k = await seeded();
  assert.deepEqual(
    await call(k, "alice", { type: "share", with: "bob", scope: { range: { from: 3, to: 7 } } }),
    { grant: 1, anchor: 3, as_of: 10, created: true },
  );
  assert.deepEqual(await call(k, "bob", { type: "read", grant: 1 }), { rows: [THREE, SEVEN], next: null, total: 2 });
  // the same scope, written as strings and with a reason: the SAME grant
  assert.deepEqual(
    await call(k, "alice", { type: "share", with: "bob", scope: { range: { from: "3", to: " 7 " } }, why: "again,  with   words" }),
    { grant: 1, anchor: 3, as_of: 10, created: false },
  );
  assert.deepEqual(
    await call(k, "alice", { type: "share", with: "bob", scope: { range: { from: 4, to: 10 } }, mode: "live" }),
    { grant: 2, anchor: 7, as_of: null, created: true },
  );
  await call(k, "alice", { type: "append", rows: [{ id: 12 }] });
  assert.deepEqual(await call(k, "bob", { type: "read", grant: 1 }), { rows: [THREE, SEVEN], next: null, total: 2 });
  // live: every row from `from` on — `to` does not bound a live range
  assert.deepEqual(await call(k, "bob", { type: "read", grant: 2 }), { rows: [SEVEN, TEN, { id: 12 }], next: null, total: 3 });
  assert.deepEqual(views(await call(k, "alice", { type: "grants", side: "given" })), [
    { grant: 1, owner: "alice", reader: "bob", scope: { range: { from: 3, to: 7 } }, mode: "frozen", as_of: 10, until: null, why: null },
    { grant: 2, owner: "alice", reader: "bob", scope: { range: { from: 4, to: 10 } }, mode: "live", as_of: null, until: null, why: null },
  ]);
});

test("share refusals, whole", async () => {
  const k = await seeded();
  const DESCRIPTOR =
    "share: `scope` must be ONE of {head_until: <row>}, {range: {from, to}}, {fold: <id>}, {row: <id>} — a descriptor, never a list of messages";
  const cases: [string, Payload, Payload][] = [
    ["alice", { scope: { range: 5 } }, BAD("scope", "share: range must be {from, to}")],
    ["alice", { scope: { range: { from: 7, to: 3 } } }, BAD("scope", "share: range needs from <= to (row ids)")],
    ["alice", { scope: { range: { from: "x", to: 3 } } }, BAD("scope", "share: range needs from <= to (row ids)")],
    ["alice", { scope: { range: { from: 50, to: 60 } } }, NOT_FOUND("scope", "share: none of YOUR rows lie in 50..60")],
    ["alice", { scope: [1] }, BAD("scope", DESCRIPTOR)],
    ["alice", { scope: {} }, BAD("scope", DESCRIPTOR)],
    ["alice", { scope: { row: 3, fold: 7 } }, BAD("scope", DESCRIPTOR)],
    ["alice", { scope: { nope: 1 } }, BAD("scope", DESCRIPTOR)],
    ["alice", {}, BAD("scope", DESCRIPTOR)],
    ["alice", { scope: { row: "x" } }, BAD("scope", "share: row must be a row id")],
    ["alice", { scope: { head_until: null } }, BAD("scope", "share: head_until must be a row id")],
    ["alice", { scope: { head_until: 1 } }, NOT_FOUND("scope", "share: you have no rows up to 1")],
    ["bob", { scope: { head_until: 5 } }, NOT_FOUND("scope", "share: you have no rows up to 5")],
    ["alice", { scope: { fold: 3 } }, BAD("scope", "share: row 3 is not a fold")],
    ["alice", { scope: { row: 99 } }, NOT_FOUND("scope", "share: row 99 is not one of YOUR rows — only rows you own can be shared")],
  ];
  for (const [who, extra, want] of cases) {
    const reader = who === "bob" ? "alice" : "bob";
    assert.deepEqual(await call(k, who, { type: "share", with: reader, ...extra }), want, JSON.stringify(extra));
  }
  assert.deepEqual(await call(k, "alice", { type: "share", with: "alice", scope: { row: 3 } }), BAD("with", "share: you already read your own rows"));
  assert.deepEqual(await call(k, "alice", { type: "share", scope: { row: 3 } }), MISSING("with", "share: with required"));
  assert.deepEqual(await call(k, "alice", { type: "share", with: "  ", scope: { row: 3 } }), MISSING("with", "share: with required"));
  assert.deepEqual(views(await call(k, "alice", { type: "grants", side: "given" })), [], "no refusal created a grant");
});

test("append, grants, unshare, owner, search and prune refusals, whole", async () => {
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
    ["alice", { type: "grants", side: "x" }, BAD("side", "grants: side must be 'given' or 'received'")],
    ["alice", { type: "grants" }, BAD("side", "grants: side must be 'given' or 'received'")],
    ["alice", { type: "unshare" }, MISSING("grant", "unshare: grant required")],
    ["alice", { type: "unshare", grant: 99 }, NOT_FOUND("grant", "unshare: no grant 99")],
    ["alice", { type: "unshare", grant: "x" }, MISSING("grant", "unshare: grant required")],
    ["alice", { type: "read", owner: "" }, BAD("owner", "read: `owner` must be an agent id")],
    ["alice", { type: "read", owner: 5 }, BAD("owner", "read: `owner` must be an agent id")],
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
  assert.deepEqual(await call(k, "alice", { type: "read", owner: "bob" }), {
    error: "read: 'alice' may act only on its own rows, not 'bob'",
    reason: "unauthorized",
    hint: "a caller writes and reads its OWN key; another owner's rows are reached through a grant it gives (`share`), or by an agent listed in mem.admins",
    side: "store",
  });
  // nothing above changed a row
  assert.deepEqual(await call(k, "alice", { type: "read" }), { rows: [THREE, SEVEN, TEN], next: null, total: 3 });
});

test("inject and dump are admins-only; every inject refusal, whole, and none of them changes the store", async () => {
  const k = await seeded();
  assert.deepEqual(await call(k, "alice", { type: "inject", tables: {} }), {
    error: "inject: 'alice' is not an admin of mem",
    reason: "unauthorized",
    side: "store",
  });
  assert.deepEqual(await call(k, "alice", { type: "dump" }), {
    error: "dump: 'alice' is not an admin of mem",
    reason: "unauthorized",
    side: "store",
  });
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
    [{ rows: [ROW1], grants: [{ owner: "g1|a", anchor: 1 }] }, T("inject: a grant needs an int `grant`")],
    [{ rows: [ROW1], grants: [{ owner: "g1|a", anchor: 2, grant: 1 }] }, T("inject: a grant's anchor names no row")],
    [{ rows: [ROW1], grants: [5] }, T("inject: a grant's anchor names no row")],
  ];
  for (const [tables, want] of cases) {
    assert.deepEqual(await call(k, null, { type: "inject", tables }), want, JSON.stringify(tables));
    assert.deepEqual(await call(k, null, { type: "dump" }), before, `a refused inject changed the store: ${JSON.stringify(tables)}`);
  }
  // absent / null sections are empty: the whole store is replaced by nothing
  assert.deepEqual(await call(k, null, { type: "inject", tables: { rows: null, fold_refs: null, grants: null } }), { rows: 0, grants: 0 });
  assert.deepEqual(await call(k, null, { type: "dump" }), { tables: { rows: [], fold_refs: [], grants: [] } });
});

test("FK cascades: a pruned anchor takes its grants, a pruned fold leaves its members' index, a pruned member leaves its folds", async () => {
  const k = store();
  const a = (p: Payload) => call(k, "alice", p);
  assert.deepEqual(await a({ type: "append", rows: [{ id: 1 }, { id: 2 }, { id: 3, kind: "fold", refs: [1, 2] }, { id: 4 }, { id: 5 }] }), {
    ids: [1, 2, 3, 4, 5],
    appended: 5,
  });
  assert.deepEqual(await a({ type: "share", with: "bob", scope: { range: { from: 2, to: 4 } } }), { grant: 1, anchor: 2, as_of: 5, created: true });
  assert.deepEqual(await a({ type: "share", with: "bob", scope: { fold: 3 } }), { grant: 2, anchor: 3, as_of: 5, created: true });
  // a row INSIDE a range that is not its anchor: the grant stays
  assert.deepEqual(await a({ type: "prune", ids: [4] }), { pruned: 1, grants_removed: 0 });
  assert.deepEqual((views(await a({ type: "grants", side: "given" }))).map((g) => g["grant"]), [1, 2]);
  // the fold itself: its grant goes, and so does its member list
  assert.deepEqual(await a({ type: "prune", ids: [3] }), { pruned: 1, grants_removed: 1 });
  assert.deepEqual((views(await a({ type: "grants", side: "given" }))).map((g) => g["grant"]), [1]);
  assert.deepEqual(await dumpNoClock(k), {
    rows: [
      { owner: "g1|alice", row: { id: 1 } },
      { owner: "g1|alice", row: { id: 2 } },
      { owner: "g1|alice", row: { id: 5 } },
    ],
    fold_refs: [],
    grants: [
      {
        owner: "g1|alice",
        reader: "g1|bob",
        scope: { range: { from: 2, to: 4 } },
        mode: "frozen",
        anchor: 2,
        as_of: 5,
        until: null,
        why: null,
        grant: 1,
      },
    ],
  });
  // a member of the pruned fold goes cleanly; a NEW fold under the old id holds only what it names
  assert.deepEqual(await a({ type: "prune", ids: [1] }), { pruned: 1, grants_removed: 0 });
  assert.deepEqual(await a({ type: "append", rows: [{ id: 3, kind: "fold", refs: [2] }] }), { ids: [3], appended: 1 });
  assert.deepEqual(await a({ type: "read", ids: [3] }), { rows: [{ id: 3, kind: "fold", refs: [2] }], next: null, total: 1 });
  // pruning the member: it leaves the fold (and the range grant anchored on it goes)
  assert.deepEqual(await a({ type: "prune", ids: [2] }), { pruned: 1, grants_removed: 1 });
  assert.deepEqual(await a({ type: "read", ids: [3] }), { rows: [{ id: 3, kind: "fold", refs: [] }], next: null, total: 1 });
  assert.deepEqual(await dumpNoClock(k), {
    rows: [
      { owner: "g1|alice", row: { id: 3, kind: "fold" } },
      { owner: "g1|alice", row: { id: 5 } },
    ],
    fold_refs: [{ owner: "g1|alice", fold: 3, members: [] }],
    grants: [],
  });
});

test("an identical share is ONE grant; any difference, an unshare or a pruned anchor makes the next one new", async () => {
  const k = store();
  const a = (p: Payload) => call(k, "alice", p);
  await a({ type: "append", rows: [{ id: 3, kind: "fold" }, { id: 5 }] });
  assert.deepEqual(await a({ type: "share", with: "bob", scope: { row: 5 } }), { grant: 1, anchor: 5, as_of: 5, created: true });
  assert.deepEqual(await a({ type: "unshare", grant: 1 }), { unshared: true, grant: 1 });
  // revoked: the identical share is a NEW grant, never the dead id
  assert.deepEqual(await a({ type: "share", with: "bob", scope: { row: 5 } }), { grant: 2, anchor: 5, as_of: 5, created: true });
  // `why` is not part of the identity
  assert.deepEqual(await a({ type: "share", with: "bob", scope: { row: 5 }, why: "other" }), { grant: 2, anchor: 5, as_of: 5, created: false });
  // the reader and the mode are
  assert.deepEqual(await a({ type: "share", with: "carol", scope: { row: 5 } }), { grant: 3, anchor: 5, as_of: 5, created: true });
  assert.deepEqual(await a({ type: "share", with: "bob", scope: { row: 5 }, mode: "live" }), { grant: 4, anchor: 5, as_of: null, created: true });
  // `keep` spares the fold; the three grants anchored on row 5 go with it
  assert.deepEqual(await a({ type: "prune", before_id: 99, keep: ["fold"] }), { pruned: 1, grants_removed: 3 });
  assert.deepEqual(await a({ type: "read" }), { rows: [{ id: 3, kind: "fold" }], next: null, total: 1 });
  assert.deepEqual(await a({ type: "share", with: "bob", scope: { row: 3 } }), { grant: 5, anchor: 3, as_of: 3, created: true });
  assert.deepEqual(await a({ type: "prune", before_id: 99 }), { pruned: 1, grants_removed: 1 });
  // a re-appended row under the old id carries no old grant: the next share is new
  await a({ type: "append", rows: [{ id: 3 }] });
  assert.deepEqual(await a({ type: "share", with: "bob", scope: { row: 3 } }), { grant: 6, anchor: 3, as_of: 3, created: true });
  assert.deepEqual(await a({ type: "search", query: "x" }), { matches: [], total: 0, truncated: false });
});

test("the admin gate: `admins` may be one bare id; who is calling is settled first; a prune that matches nothing removes nothing", async () => {
  const k = await seeded();
  k.updateMeta("mem", { admins: "alice" });
  const dumped = (await call(k, "alice", { type: "dump" })) as Payload;
  assert.deepEqual(Object.keys(dumped), ["tables"]);
  assert.deepEqual(await call(k, "alice", { type: "inject", tables: dumped["tables"]! }), { rows: 3, grants: 0 });
  assert.deepEqual(await call(k, null, { type: "dump" }), {
    error: "dump: 'kernel' is not an admin of mem",
    reason: "unauthorized",
    side: "store",
  });
  // an operator the store does not serve is refused as THAT, before any admin check
  k.updateMeta("mem", { operator: false, admins: ["kernel"] });
  const notServed = {
    error: "mem: the operator is not served here",
    reason: "unauthorized",
    hint: "update_agent id=mem operator=true to serve the operator (as 'kernel')",
    side: "store",
  };
  assert.deepEqual(await call(k, null, { type: "dump" }), notServed);
  assert.deepEqual(await call(k, null, { type: "inject", tables: {} }), notServed);
  assert.deepEqual(await call(k, "alice", { type: "prune", ids: [99] }), { pruned: 0, grants_removed: 0 });
  assert.deepEqual(await call(k, "alice", { type: "prune", before_id: 1 }), { pruned: 0, grants_removed: 0 });
  assert.deepEqual(await call(k, "alice", { type: "read" }), { rows: [THREE, SEVEN, TEN], next: null, total: 3 });
});
