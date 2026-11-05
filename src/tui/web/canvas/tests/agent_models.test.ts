import { test } from "node:test";
import assert from "node:assert/strict";
import { hitsState, storeRows, storeState } from "../src/bundles/agent_models/store_model.ts";
import { lastFire, nextText, schedulerRows, schedulerState } from "../src/bundles/agent_models/scheduler_model.ts";
import { cabinetsRows, cabinetsState } from "../src/bundles/agent_models/cabinets_model.ts";
import { credentialRows, credentialsState, grantsOf } from "../src/bundles/agent_models/credentials_model.ts";
import type { Row } from "../src/bundles/agent_models/rows.ts";

const text = (rows: Row[]): string => rows.map((r) => `${"  ".repeat(r.depth ?? 0)}${r.text}`).join("\n");

test("store_model_counts_per_owner_and_folds_and_drops_row_bodies", () => {
  const BODY = "SENTINEL-ROW-BODY";
  const dump = {
    rows: [1, 2, 3].map((i) => ({ owner: "alice", row: { id: i, content: BODY } })).concat([{ owner: "bob", row: { id: 1, content: BODY } }]),
    fold_refs: [{ owner: "alice", fold: 3, members: [1, 2] }],
  };
  const s = storeState(dump);
  assert.deepEqual(s.owners, { alice: { rows: 3, folds: 1 }, bob: { rows: 1, folds: 0 } });
  assert.equal(s.total, 4);
  assert.ok(!JSON.stringify(s).includes(BODY));
  assert.ok(!text(storeRows(s, null)).includes(BODY));
  assert.match(text(storeRows(s, null)), /alice {2}3 rows {2}1 folds/);
  const h = hitsState("q", { matches: [{ id: 7, line: 2, text: "my own line" }], total: 1, truncated: false });
  assert.match(text(storeRows(s, h)), /7:2 {2}my own line/);
  // a refused dump says so, and the search section is still there
  const t = text(storeRows(storeState({ error: "not_allowed" }), null));
  assert.match(t, /owners not readable: not_allowed/);
  assert.match(t, /SEARCH/);
});

test("scheduler_model_next_run_target_verb_last_result", () => {
  const s = schedulerState(
    { schedules: [
      { id: "s1", target: "ai", payload: { type: "send", text: "hi" }, interval_seconds: 60, next_run: 1100, run_count: 3, paused: false },
      { id: "s2", target: "files", payload: { type: "list" }, interval_seconds: 30, next_run: 900, run_count: 0, paused: true },
    ] },
    { history: [
      { schedule_id: "s1", result: { ok: true }, error: null, ts: 10 },
      { schedule_id: "s1", result: null, error: "timeout", ts: 20 },
    ] },
  );
  const t = text(schedulerRows(s, 1000));
  assert.match(t, /s1 {2}ai · send {2}every 60s {2}next in 100s {2}runs 3/);
  assert.match(t, /last: ERROR timeout/, "the newest event wins");
  assert.match(t, /s2 {2}files · list.*PAUSED/);
  assert.match(t, /last: no runs yet/);
  assert.equal(nextText(900, 1000), "due");
  assert.equal(lastFire(s.history, "s1")?.error, "timeout");
});

test("cabinets_model_rows", () => {
  const m = cabinetsState({ count: 1, cabinets: [{ id: "c1", task: "build", worker: "w1", canvas: "cv", status: { line: "halfway", progress: 50 }, equipment: [{}, {}] }] });
  const t = text(cabinetsRows(m));
  for (const w of ["CABINETS · 1", "c1  build", "worker w1", "canvas cv", "2 equipment", "halfway  50%"]) assert.ok(t.includes(w), w);
  assert.match(text(cabinetsRows(cabinetsState({ cabinets: [] }))), /no cabinets yet/);
  const cell = cabinetsState({ id: "c1", task: "t", worker: "w", canvas: "cv", status: null, equipment: [] });
  assert.equal(cell.manager, false);
  assert.match(text(cabinetsRows(cell)), /not reported yet/);
});

test("credentials_model_exposes_names_hosts_field_names_and_grants_only", () => {
  const list = { refs: [{ ref: "openai", host: "api.openai.com", fields: { api_key: "sk-SECRET-123" }, value: "sk-SECRET-123", len: 13, masked: "****" }], masked: "****" };
  const s = credentialsState(list);
  assert.deepEqual(s.refs, [{ name: "openai", host: "api.openai.com", fields: ["api_key"] }]);
  const g = grantsOf({ callable_by: { $self: "*", "openai-key": ["set", "get", "forget"], exec: "*" } });
  assert.deepEqual(g, [["openai-key", "set, get, forget"], ["exec", "*"]]);
  const rows = credentialRows(s, g);
  const all = JSON.stringify({ s, rows });
  for (const bad of ["SECRET", "sk-", "****"]) assert.ok(!all.includes(bad), bad);
  assert.match(text(rows), /openai {2}host api\.openai\.com/);
  assert.match(text(rows), /fields: api_key/);
  assert.match(text(rows), /openai-key: set, get, forget/);
});

test("credentials_model_never_includes_a_length_or_mask", () => {
  const s = credentialsState({ refs: [{ ref: "k", host: "h", fields: ["a"], len: 13, length: 13, masked: "sk-****3", mask: "••••" }] });
  assert.deepEqual(Object.keys(s.refs[0]!).sort(), ["fields", "host", "name"]);
  const t = text(credentialRows(s, []));
  assert.ok(!/13|\*|•/.test(t), t);
});
