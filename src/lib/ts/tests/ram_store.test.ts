// ram_store: tenancy by caller, grants + FK cascade, search, inject/dump.
// Mirrors python bundled_agents/memory/ram_store/tests/test_ram_store.py.
//
// Callers act through `scopedFor` (a real agent send, gated both ways); the
// operator is a bare `kernel.send`. The root's guid is set by hand — the root is
// not booted here, and the store keys every row by it. A caller on ANOTHER
// kernel arrives the way the bridge delivers a host's call: `foreignView`.

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
      meta: {
        operator: true,
        admins: ["kernel"],
        callable_by: { alice: "*", bob: "*", carol: "*" },
      },
    }),
  );
  return k;
}

async function call(k: Kernel, payload: Payload, as?: string): Promise<Payload> {
  const via = as === undefined ? k : k.scopedFor(as, null);
  return (await via.send([{ target: "mem", payload }]))[0]!.result as Payload;
}

const append = (k: Kernel, who: string, rows: Json[]) => call(k, { type: "append", rows }, who);
const ids = (r: Payload) => (r["rows"] as Payload[]).map((x) => x["id"]);

test("rows are keyed by caller; a duplicate id is refused all or none", async () => {
  const k = store();
  assert.deepEqual(await append(k, "alice", [{ id: 101, role: "user", content: "hi" }]), { ids: [101], appended: 1 });
  // the same id is free under another owner — keys are (owner, id)
  assert.deepEqual((await append(k, "bob", [{ id: 101, content: "bob" }]))["ids"], [101]);
  const a = await call(k, { type: "read" }, "alice");
  const b = await call(k, { type: "read" }, "bob");
  assert.deepEqual((a["rows"] as Payload[]).map((r) => r["content"]), ["hi"]);
  assert.deepEqual((b["rows"] as Payload[]).map((r) => r["content"]), ["bob"]);
  const dup = await append(k, "alice", [{ id: 102 }, { id: 101 }]);
  assert.equal(dup["reason"], "exists");
  assert.equal(((await call(k, { type: "read" }, "alice"))["rows"] as Json[]).length, 1);
});

test("owner is never a writable field", async () => {
  const k = store();
  assert.equal((await call(k, { type: "append", owner: "bob", rows: [{ id: 5 }] }, "alice"))["reason"], "unauthorized");
  assert.equal((await call(k, { type: "read", owner: "bob" }, "alice"))["reason"], "unauthorized");
});

test("a row without an id takes the kernel's next one", async () => {
  const k = store();
  const before = k.store.nextId;
  const out = await append(k, "alice", [{ content: "x" }]);
  assert.ok(((out["ids"] as number[])[0] as number) >= before);
  // a string id is not a kernel id
  assert.equal((await append(k, "alice", [{ id: "7" }]))["reason"], "bad_arg");
});

test("the operator needs explicit service, and its rows are its own key", async () => {
  const k = store();
  k.updateMeta("mem", { operator: false });
  assert.equal((await call(k, { type: "read" }))["reason"], "unauthorized");
  k.updateMeta("mem", { operator: true });
  await call(k, { type: "append", rows: [{ id: 7, kind: "call" }] });
  assert.deepEqual(ids(await call(k, { type: "read", kinds: ["call"] })), [7]);
  assert.deepEqual((await call(k, { type: "read" }, "alice"))["rows"], []);
});

test("an admin may name an owner", async () => {
  const k = store();
  await append(k, "alice", [{ id: 1, content: "a" }]);
  assert.deepEqual(ids(await call(k, { type: "read", owner: "alice" })), [1]);
  assert.deepEqual((await call(k, { type: "append", owner: "alice", rows: [{ id: 2 }] }))["ids"], [2]);
});

test("a store refuses when the kernel has no guid", async () => {
  const k = store();
  k.updateMeta(ROOT, { guid: null });
  const out = await call(k, { type: "read" }, "alice");
  assert.equal(out["reason"], "not_found");
  assert.equal(out["hint"], "boot the kernel root first (it mints the guid)");
});

test("refs must name the owner's own rows", async () => {
  const k = store();
  await append(k, "bob", [{ id: 10 }]);
  assert.equal((await append(k, "alice", [{ id: 20, kind: "fold", refs: [10] }]))["reason"], "not_found");
  const out = await append(k, "alice", [{ id: 1 }, { id: 2 }, { id: 3, kind: "fold", refs: [1, 2] }]);
  assert.deepEqual(out["ids"], [1, 2, 3]);
});

test("share, read under the grant, and zero tolerance", async () => {
  const k = store();
  await append(k, "alice", [1, 2, 3].map((i) => ({ id: i, content: `a${i}` })));
  await append(k, "bob", [{ id: 50, content: "b" }]);
  const g = await call(k, { type: "share", with: "bob", scope: { head_until: 2 }, why: "context" }, "alice");
  assert.equal(g["created"], true);
  assert.equal(g["anchor"], 1);
  assert.deepEqual(ids(await call(k, { type: "read", grant: g["grant"] as number }, "bob")), [1, 2]);
  assert.equal((await call(k, { type: "read", grant: g["grant"] as number }, "carol"))["reason"], "unauthorized");
  // bob cannot share alice's rows — not under his key
  assert.equal((await call(k, { type: "share", with: "carol", scope: { row: 1 } }, "bob"))["reason"], "not_found");
  const again = await call(k, { type: "share", with: "bob", scope: { head_until: 2 } }, "alice");
  assert.equal(again["grant"], g["grant"]);
  assert.equal(again["created"], false);
});

test("frozen stops at its bound; live follows the head", async () => {
  const k = store();
  await append(k, "alice", [{ id: 1 }, { id: 2 }]);
  const frozen = await call(k, { type: "share", with: "bob", scope: { head_until: 2 } }, "alice");
  const live = await call(k, { type: "share", with: "bob", scope: { head_until: 2 }, mode: "live" }, "alice");
  await append(k, "alice", [{ id: 3 }]);
  assert.deepEqual(ids(await call(k, { type: "read", grant: frozen["grant"] as number }, "bob")), [1, 2]);
  assert.deepEqual(ids(await call(k, { type: "read", grant: live["grant"] as number }, "bob")), [1, 2, 3]);
});

test("a fold scope reaches its members", async () => {
  const k = store();
  await append(k, "alice", [{ id: 1 }, { id: 2 }, { id: 3, kind: "fold", refs: [1, 2] }, { id: 4 }]);
  const g = await call(k, { type: "share", with: "bob", scope: { fold: 3 } }, "alice");
  const got = await call(k, { type: "read", grant: g["grant"] as number }, "bob");
  assert.deepEqual(ids(got), [1, 2, 3]);
  assert.deepEqual((got["rows"] as Payload[])[2]!["refs"], [1, 2]);
});

test("prune cascades to grants and fold refs", async () => {
  const k = store();
  await append(k, "alice", [{ id: 1 }, { id: 2 }, { id: 3, kind: "fold", refs: [1, 2] }]);
  const g = await call(k, { type: "share", with: "bob", scope: { head_until: 3 } }, "alice");
  assert.deepEqual(await call(k, { type: "prune", before_id: 2 }, "alice"), { pruned: 1, grants_removed: 1 });
  assert.equal((await call(k, { type: "read", grant: g["grant"] as number }, "bob"))["reason"], "not_found");
  const fold = await call(k, { type: "read", ids: [3] }, "alice");
  assert.deepEqual((fold["rows"] as Payload[])[0]!["refs"], [2]);
  assert.deepEqual((await call(k, { type: "grants", side: "given" }, "alice"))["grants"], []);
});

test("unshare and the grants listing", async () => {
  const k = store();
  await append(k, "alice", [{ id: 1 }]);
  const g = await call(k, { type: "share", with: "bob", scope: { row: 1 } }, "alice");
  const rec = await call(k, { type: "grants", side: "received" }, "bob");
  assert.deepEqual((rec["grants"] as Payload[]).map((x) => x["owner"]), ["alice"]);
  assert.equal((await call(k, { type: "unshare", grant: g["grant"] as number }, "bob"))["reason"], "unauthorized");
  assert.equal((await call(k, { type: "unshare", grant: g["grant"] as number }, "alice"))["unshared"], true);
  assert.equal((await call(k, { type: "read", grant: g["grant"] as number }, "bob"))["reason"], "not_found");
});

test("search finds lines and descends folds", async () => {
  const k = store();
  await append(k, "alice", [
    { id: 1, content: "alpha\nthe audit marker is 7\nomega" },
    { id: 2, content: "nothing" },
    { id: 3, kind: "fold", refs: [1, 2], content: "[fold] survey" },
  ]);
  const got = await call(k, { type: "search", query: "AUDIT" }, "alice");
  assert.deepEqual(got["matches"], [{ id: 1, line: 2, text: "the audit marker is 7" }]);
  const scoped = await call(k, { type: "search", query: "marker", ids: [3] }, "alice");
  assert.equal(scoped["total"], 1);
});

test("read paging and selectors", async () => {
  const k = store();
  await append(k, "alice", [
    { id: 1, kind: "call", name: "job" },
    { id: 2, kind: "clear" },
    { id: 3 },
    { id: 4, kind: "view" },
  ]);
  assert.deepEqual(ids(await call(k, { type: "read", from_last_kind: "clear" }, "alice")), [2, 3, 4]);
  assert.deepEqual(ids(await call(k, { type: "read", match: { name: "job" } }, "alice")), [1]);
  assert.deepEqual(ids(await call(k, { type: "read", kinds: ["view"], order: "desc", limit: 1 }, "alice")), [4]);
  const page = await call(k, { type: "read", limit: 2 }, "alice");
  assert.deepEqual(ids(page), [1, 2]);
  assert.equal(page["next"], 2);
  assert.deepEqual(ids(await call(k, { type: "read", after: 2 }, "alice")), [3, 4]);
});

test("dump → inject round-trips; a broken FK is refused and changes nothing", async () => {
  const k = store();
  await append(k, "alice", [{ id: 1 }, { id: 2 }, { id: 3, kind: "fold", refs: [1, 2] }]);
  await call(k, { type: "share", with: "bob", scope: { fold: 3 } }, "alice");
  assert.equal((await call(k, { type: "dump" }, "alice"))["reason"], "unauthorized", "only admins");
  const tables = (await call(k, { type: "dump" }))["tables"] as Payload;
  await call(k, { type: "prune", before_id: 99 }, "alice");
  assert.deepEqual((await call(k, { type: "read" }, "alice"))["rows"], []);
  assert.deepEqual(await call(k, { type: "inject", tables }), { rows: 3, grants: 1 });
  assert.deepEqual((await call(k, { type: "dump" }))["tables"], tables);
  const bad = { ...tables, grants: [{ ...((tables["grants"] as Payload[])[0] as Payload), anchor: 999 }] };
  assert.equal((await call(k, { type: "inject", tables: bad }))["reason"], "bad_arg");
  assert.deepEqual((await call(k, { type: "dump" }))["tables"], tables);
});

test("the store's verbs reach a reader through the bundle readme", async () => {
  const k = store();
  const r = (await k.send([{ target: "mem", payload: { type: "reflect", readme: true } }]))[0]!.result as Payload;
  assert.match(String(r["readme"]), /append: args: rows/);
  assert.equal(r["operator"], true);
  assert.deepEqual(r["admins"], ["kernel"]);
});

// python `test_remote_caller_is_keyed_by_its_qualified_address`: a host's call
// relayed in over the bridge is keyed by ITS qualified address, verbatim — never
// under this kernel's guid — and the local callers' rows stay theirs.
test("a caller on another kernel is keyed by its qualified address", async () => {
  const k = store();
  const mem = k.get("mem")!;
  mem.meta["callable_by"] = { ...(mem.meta["callable_by"] as Payload), "g2|scout": "*" };
  const scout = k.foreignView("g2|scout");
  const out = (await scout.send([{ target: "mem", payload: { type: "append", rows: [{ id: 1 }] } }]))[0]!;
  assert.deepEqual((out.result as Payload)["ids"], [1]);
  const owners = async (): Promise<Json[]> =>
    ((((await call(k, { type: "dump" }))["tables"] as Payload)["rows"]) as Payload[]).map((r) => r["owner"]!).sort();
  assert.deepEqual(await owners(), ["g2|scout"]);
  await append(k, "alice", [{ id: 2 }]);
  assert.deepEqual(await owners(), ["g1|alice", "g2|scout"]);
  // a share NAMING the foreign reader is the key it reads under
  const g = await call(k, { type: "share", with: "g2|scout", scope: { row: 2 } }, "alice");
  const got = (await scout.send([{ target: "mem", payload: { type: "read", grant: g["grant"]! } }]))[0]!;
  assert.deepEqual(ids(got.result as Payload), [2]);
  // and it needs no guid of THIS kernel's: the qualified address is the key
  k.updateMeta(ROOT, { guid: null });
  const read = (await scout.send([{ target: "mem", payload: { type: "read" } }]))[0]!;
  assert.deepEqual(ids(read.result as Payload), [1]);
});

// python `_text_of` / `str.splitlines()`: structured content is indexed as
// `json.dumps` writes it (`", "` / `": "`), and every python line boundary splits.
test("search reads structured content and lines the way python does", async () => {
  const k = store();
  await append(k, "alice", [
    { id: 1, content: { a: 1, b: "é" } },
    { id: 2, content: "one\rtwo\u2028three\x0bfour" },
  ]);
  const s1 = await call(k, { type: "search", query: '"a": 1' }, "alice");
  assert.deepEqual(s1["matches"], [{ id: 1, line: 1, text: '{"a": 1, "b": "é"}' }]);
  const s2 = await call(k, { type: "search", query: "three" }, "alice");
  assert.deepEqual(s2["matches"], [{ id: 2, line: 3, text: "three" }]);
  const s3 = await call(k, { type: "search", query: "four" }, "alice");
  assert.deepEqual(s3["matches"], [{ id: 2, line: 4, text: "four" }]);
});

test("share: a falsy mode is the default; refusals quote as python does", async () => {
  const k = store();
  await append(k, "alice", [{ id: 1 }]);
  const g = await call(k, { type: "share", with: "bob", scope: { row: 1 }, mode: "" }, "alice");
  assert.equal(g["created"], true);
  const listed = (await call(k, { type: "grants", side: "given" }, "alice"))["grants"] as Payload[];
  assert.equal(listed[0]!["mode"], "frozen");
  const bad = await call(k, { type: "share", with: "bob", scope: { row: 1 }, mode: "x" }, "alice");
  assert.equal(bad["error"], "share: mode must be one of ('frozen', 'live')");
  const unk = await call(k, { type: "nope" }, "alice");
  assert.deepEqual(unk, { error: "ram_store: unknown type 'nope'", reason: "unhandled", verb: "nope" });
});

// python `_caller`: `not agent.record.get("operator")` — truthiness, not `=== true`;
// the hint is python's text verbatim (python ram_store/tools.py `_caller`).
test("the operator flag reads as python truthiness; the hint is python's", async () => {
  const k = store();
  for (const off of [false, 0, "", null, [], {}]) {
    k.updateMeta("mem", { operator: off as Json });
    assert.deepEqual(await call(k, { type: "read" }), {
      error: "mem: the operator is not served here",
      reason: "unauthorized",
      hint: "update_agent id=mem operator=true to serve the operator (as 'kernel')",
      side: "store",
    });
  }
  for (const on of [true, 1, "yes", [0], { a: 1 }]) {
    k.updateMeta("mem", { operator: on as Json });
    assert.deepEqual((await call(k, { type: "read" }))["rows"], [], `operator=${JSON.stringify(on)} serves`);
  }
});

// Measured from python's ram_store (a probe over the python kernel fixture,
// 2026-10-01): each refusal quotes the caller's value as `repr` does.
test("refusals quote a caller's value as python repr does", async () => {
  const k = store();
  const rid = async (id: Json) => (await append(k, "alice", [{ id }]))["error"];
  assert.equal(await rid("7"), "append: a row id is a kernel int, got '7'");
  assert.equal(await rid(1.5), "append: a row id is a kernel int, got 1.5");
  assert.equal(await rid(true), "append: a row id is a kernel int, got True");
  assert.equal(await rid(0.00001), "append: a row id is a kernel int, got 1e-05");
  assert.equal(await rid(-3), "append: a row id is a kernel int, got -3");
  assert.equal(await rid([1]), "append: a row id is a kernel int, got [1]");
  assert.equal(await rid({ a: 1 }), "append: a row id is a kernel int, got {'a': 1}");
  assert.equal(await rid(0), "append: a row id is a kernel int, got 0");
  assert.equal(
    (await append(k, "alice", [{ id: 20, refs: [10, 11] }]))["error"],
    "append: row 20 refs rows that do not exist for this owner: [10, 11]",
  );
  assert.equal(
    (await append(k, "alice", [{ id: 21, refs: ["10"] }]))["error"],
    "append: row 21 refs rows that do not exist for this owner: ['10']",
  );
  const grant = async (g: Json) => (await call(k, { type: "read", grant: g }, "alice"))["error"];
  assert.equal(await grant("abc"), "read: grant 'abc' does not exist — revoked (unshare) or its rows were pruned");
  assert.equal(await grant(99), "read: grant 99 does not exist — revoked (unshare) or its rows were pruned");
  assert.equal(await grant(1.5), "read: grant 1.5 does not exist — revoked (unshare) or its rows were pruned");
  // python quotes the fold AFTER `_as_id`: a value that is no id reads `None`
  const fold = async (f: Json) =>
    (await call(k, { type: "inject", tables: { rows: [], fold_refs: [{ owner: "g1|alice", fold: f, members: [] }], grants: [] } }))["error"];
  assert.equal(await fold("x"), "inject: fold_refs names no row None");
  assert.equal(await fold(9), "inject: fold_refs names no row 9");
  assert.equal(await fold(null), "inject: fold_refs names no row None");
});

test("a ref of '0' is an id python reads (0), not a non-id — the refusal names it as python does", async () => {
  const k = store();
  const r = await append(k, "alice", [{ id: 30, kind: "fold", refs: ["0"] }]);
  // measured on python's ram_store (2026-10-01)
  assert.deepEqual(r, {
    error: "append: row 30 refs rows that do not exist for this owner: ['0']",
    reason: "not_found",
    arg: "rows",
  });
});
