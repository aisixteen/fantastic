import { test } from "node:test";
import assert from "node:assert/strict";
import { countText, resourcesRows, resourcesState } from "../src/bundles/agent_models/resources_model.ts";
import { holderText, taskOrder, tasksRows, tasksState } from "../src/bundles/agent_models/tasks_model.ts";
import { hitsState, storeRows, storeState } from "../src/bundles/agent_models/store_model.ts";
import { lastFire, nextText, schedulerRows, schedulerState } from "../src/bundles/agent_models/scheduler_model.ts";
import { cabinetsRows, cabinetsState } from "../src/bundles/agent_models/cabinets_model.ts";
import { credentialRows, credentialsState, grantsOf } from "../src/bundles/agent_models/credentials_model.ts";
import type { Row } from "../src/bundles/agent_models/rows.ts";

const text = (rows: Row[]): string => rows.map((r) => `${"  ".repeat(r.depth ?? 0)}${r.text}`).join("\n");

test("resources_model_rows_roles_fleet_and_counts", () => {
  const s = resourcesState({
    roles: { roles: [{ name: "worker", purpose: "does the thing" }] },
    fleet: { roles: { worker: [{ id: "w1", parent: "boss", paused: true }, { id: "w2" }] } },
    total: { running: 2, max: null },
    cards: { worker: { name: "worker", tier: "small" } },
    counts: { worker: { running: 2, max: 5 } },
  });
  const t = text(resourcesRows(s));
  assert.match(t, /worker {2}tier small {2}2\/5/);
  assert.match(t, /does the thing/);
  assert.match(t, /w1 {2}worker {2}parent boss {2}paused/);
  assert.match(t, /w2 {2}worker {2}parent - {2}active/, "no answer is active");
  assert.match(t, /2 · no limit/, "uncapped says no limit");
  assert.equal(countText({ running: 1, max: 3 }), "1/3");
  assert.match(text(resourcesRows(resourcesState({ roles: { error: "denied" } }))), /refused: denied/);
});

const TASKS = {
  tasks: [
    { id: "b", parent: "a", status: "waiting", source: "gh:o/r#2", holder: null, history: [] },
    { id: "a", parent: null, status: "running", source: "gh:o/r#1", holder: "w1", update_to: "gh:o/r#9", versions: "1.2",
      history: [{ from: null, to: "w1", by: "lead", at: 1 }, { from: "w1", to: { subtree: "lead" }, by: "lead", at: 2 }],
      report: { from: "w1", state: "running", result: "half", at: 3 } },
    { id: "c", parent: "a", status: "done", source: "gh:o/r#3", holder: { subtree: "lead" } },
    { id: "o", parent: "ghost", status: "waiting", source: "s", holder: null },
    { id: "x", parent: "y", status: "waiting", source: "s", holder: null },
    { id: "y", parent: "x", status: "waiting", source: "s", holder: null },
  ],
};

test("tasks_model_builds_the_tree_with_holder_status_source_and_shows_versions_only_when_present", () => {
  const s = tasksState(TASKS);
  const order = taskOrder(s.tasks).map(([d, t]) => `${d}${t.id}`);
  assert.deepEqual(order, ["0a", "1b", "1c", "0o", "0x", "1y"], "parents first, children by id, orphan and cycle at the root, no loop");
  const t = text(tasksRows(s, new Set()));
  assert.match(t, /a {2}running {2}w1 {2}gh:o\/r#1 {2}v1\.2/);
  assert.match(t, /c {2}done {2}subtree:lead/);
  assert.match(t, /b {2}waiting {2}unheld/);
  const b = t.split("\n").find((l) => l.includes(" b  waiting"))!;
  assert.ok(!b.includes("v1") && !b.includes("versions"), "no versions, nothing shown");
  assert.equal(holderText({ subtree: "r" }), "subtree:r");
  assert.match(text(tasksRows(tasksState({ tasks: [{ id: "a", versions: ["1", "2"] }] }), new Set())), /versions \[1, 2\]/);
});

test("tasks_model_history_rows_only_when_expanded", () => {
  const s = tasksState(TASKS);
  assert.ok(!text(tasksRows(s, new Set())).includes(" by "));
  const t = text(tasksRows(s, new Set(["a"])));
  assert.match(t, /unheld -> w1 by lead/);
  assert.match(t, /w1 -> subtree:lead by lead/);
  assert.match(t, /update_to gh:o\/r#9/);
  assert.match(t, /report w1 running half/);
});

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
