// ram_store: tenancy by caller, FK cascade, search, inject/dump.
// Mirrors python bundled_agents/memory/ram_store/tests/test_ram_store.py.
//
// Callers act through their own dispatch (`sendAs`: a real agent send, gated
// both ways); the operator is a bare `kernel.send`. The root's guid is set by
// hand — the root is not booted here, and the store keys every row by it.

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

async function call(k: Kernel, payload: Payload, who?: string): Promise<Payload> {
  const items = [{ target: "mem", payload }];
  return (await (who === undefined ? k.send(items) : sendAs(k, who, items)))[0]!.result as Payload;
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

test("owner is never a writable field — an `owner` arg is not read", async () => {
  const k = store();
  assert.deepEqual((await call(k, { type: "append", owner: "bob", rows: [{ id: 5 }] }, "alice"))["ids"], [5]);
  assert.deepEqual(ids(await call(k, { type: "read", owner: "bob" }, "alice")), [5]);
  assert.deepEqual((await call(k, { type: "read" }, "bob"))["rows"], []);
});

test("a row without an id takes the kernel's next one", async () => {
  const k = store();
  const before = k.store.nextId;
  const out = await append(k, "alice", [{ content: "x" }]);
  assert.ok(((out["ids"] as number[])[0] as number) >= before);
  // a string id is not a kernel id
  assert.equal((await append(k, "alice", [{ id: "7" }]))["reason"], "bad_arg");
});

test("the operator is served as `kernel`, and its rows are its own key", async () => {
  const k = store();
  await call(k, { type: "append", rows: [{ id: 7, kind: "call" }] });
  assert.deepEqual(ids(await call(k, { type: "read", kinds: ["call"] })), [7]);
  assert.deepEqual((await call(k, { type: "read" }, "alice"))["rows"], []);
  const owners = (((await call(k, { type: "dump" }))["tables"] as Payload)["rows"] as Payload[]).map((r) => r["owner"]);
  assert.deepEqual(owners, ["g1|kernel"]);
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

test("prune cascades to fold refs", async () => {
  const k = store();
  await append(k, "alice", [{ id: 1 }, { id: 2 }, { id: 3, kind: "fold", refs: [1, 2] }]);
  assert.deepEqual(await call(k, { type: "prune", before_id: 2 }, "alice"), { pruned: 1 });
  const fold = await call(k, { type: "read", ids: [3] }, "alice");
  assert.deepEqual((fold["rows"] as Payload[])[0]!["refs"], [2]);
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
  // inject / dump are plain verbs: a caller the gate admits sends them
  const tables = (await call(k, { type: "dump" }, "alice"))["tables"] as Payload;
  assert.deepEqual(Object.keys(tables), ["rows", "fold_refs"]);
  await call(k, { type: "prune", before_id: 99 }, "alice");
  assert.deepEqual((await call(k, { type: "read" }, "alice"))["rows"], []);
  assert.deepEqual(await call(k, { type: "inject", tables }), { rows: 3 });
  assert.deepEqual((await call(k, { type: "dump" }))["tables"], tables);
  const bad = { ...tables, fold_refs: [{ owner: "g1|alice", fold: 3, members: [1, 999] }] };
  assert.equal((await call(k, { type: "inject", tables: bad }))["reason"], "bad_arg");
  assert.deepEqual((await call(k, { type: "dump" }))["tables"], tables);
});

test("the store's verbs reach a reader through the bundle readme", async () => {
  const k = store();
  const r = (await k.send([{ target: "mem", payload: { type: "reflect", readme: true } }]))[0]!.result as Payload;
  assert.match(String(r["readme"]), /append: args: rows/);
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

test("an unknown verb is refused as python does", async () => {
  const k = store();
  const unk = await call(k, { type: "nope" }, "alice");
  assert.deepEqual(unk, { error: "ram_store: unknown type 'nope'", reason: "unhandled", verb: "nope" });
});

// Measured from python's ram_store (a probe over the python kernel fixture):
// each refusal quotes the caller's value as `repr` does.
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
  // python quotes the fold AFTER `_as_id`: a value that is no id reads `None`
  const fold = async (f: Json) =>
    (await call(k, { type: "inject", tables: { rows: [], fold_refs: [{ owner: "g1|alice", fold: f, members: [] }] } }))["error"];
  assert.equal(await fold("x"), "inject: fold_refs names no row None");
  assert.equal(await fold(9), "inject: fold_refs names no row 9");
  assert.equal(await fold(null), "inject: fold_refs names no row None");
});

test("a ref of '0' is an id python reads (0), not a non-id — the refusal names it as python does", async () => {
  const k = store();
  const r = await append(k, "alice", [{ id: 30, kind: "fold", refs: ["0"] }]);
  // measured on python's ram_store
  assert.deepEqual(r, {
    error: "append: row 30 refs rows that do not exist for this owner: ['0']",
    reason: "not_found",
    arg: "rows",
  });
});
