import { test } from "node:test";
import assert from "node:assert/strict";
import { dropParent, importCall, isBlueprintDir, previewLines, portsOf, runImportDrop } from "../src/bundles/files/import_drop.ts";
import type { ImportAnswer, ImportView } from "../src/bundles/files/import_drop.ts";
import { FakeBridge } from "./fake_bridge.ts";

// #269: a folder holding blueprint.yaml dropped on the canvas is previewed, then imported only
// when approved. The DOM dialog is the e2e probe's; here `send` and `show` are fakes.

type Rec = Record<string, unknown>;

const PREVIEW: Rec = {
  approval_required: true,
  agents: 2,
  new_ids: { a: "a_1", b: "b_1" },
  reaches: [{ id: "a_1", from: "a", can_call: ["m1"], callable_by: [] }, { id: "b_1", from: "b" }],
  ports: { "@memory": "m1" },
  code: [],
  hint: "nothing was created: call again with approve: true to import",
};
const ITEM = { kind: "dir" as const, path: "bp" };

/** a fake `user`: records every payload, answers by `reply` */
function rig(reply: (p: Rec) => Rec) {
  const sent: Rec[] = [];
  const shown: ImportView[] = [];
  return {
    sent,
    shown,
    drop: (answer: ImportAnswer | ((v: ImportView) => ImportAnswer)) => ({
      item: ITEM,
      parentId: "home",
      send: async (p: Rec): Promise<Rec> => (sent.push(p), reply(p)),
      show: (v: ImportView): ImportAnswer => (shown.push(v), typeof answer === "function" ? answer(v) : answer),
    }),
  };
}
const yes: ImportAnswer = { approve: true, bind: {}, allowCode: false };
const no: ImportAnswer = { approve: false, bind: {}, allowCode: false };

test("a_dir_with_blueprint_yaml_is_a_blueprint_and_a_plain_dir_is_not", async () => {
  const b = new FakeBridge();
  b.dirs.add("bp");
  b.dirs.add("plain");
  b.dirs.add("tricky");
  b.dirs.add("tricky/blueprint.yaml");
  b.files.set("bp/blueprint.yaml", "agents: []\n");
  b.files.set("plain/notes.md", "x");
  const listing = async (p: string) => b.call({ type: "list", path: p });
  assert.equal(isBlueprintDir({ kind: "dir", path: "bp" }, await listing("bp")), true);
  assert.equal(isBlueprintDir({ kind: "dir", path: "plain" }, await listing("plain")), false);
  // a folder NAMED blueprint.yaml is not the file
  assert.equal(isBlueprintDir({ kind: "dir", path: "tricky" }, await listing("tricky")), false);
  // only a dir drag item counts, and a refusal is not a listing
  assert.equal(isBlueprintDir({ kind: "file", path: "bp/blueprint.yaml" }, await listing("bp")), false);
  assert.equal(isBlueprintDir({ kind: "dir", path: "nope" }, await listing("nope")), false);
});

test("import_call_has_literal_approve_and_allow_code_only_when_set", () => {
  const p = importCall("bp", "home", {});
  assert.deepEqual(p, { type: "import_subtree", path: "bp", parent_id: "home", bind: {} });
  assert.equal("approve" in p || "allow_code" in p, false);
  const q = importCall("bp", "home", { bind: { "@memory": "m1" }, allowCode: true, approve: true });
  assert.equal(q["approve"], true);
  assert.equal(q["allow_code"], true);
  assert.deepEqual(q["bind"], { "@memory": "m1" });
  // falsy flags are absent, never "false" strings
  const r = importCall("bp", "home", { allowCode: false, approve: false });
  assert.equal("approve" in r || "allow_code" in r, false);
});

test("dropping_a_blueprint_sends_the_preview_call_without_approve_and_shows_it", async () => {
  const r = rig(() => PREVIEW);
  await runImportDrop(r.drop(no));
  assert.equal(r.sent.length, 1);
  assert.equal("approve" in r.sent[0]!, false);
  assert.equal(r.sent[0]!["type"], "import_subtree");
  assert.equal(r.sent[0]!["path"], "bp");
  assert.equal(r.sent[0]!["parent_id"], "home");
  assert.equal(r.shown.length, 1);
  assert.equal(r.shown[0]!.kind, "preview");
  assert.deepEqual(r.shown[0]!.lines, previewLines(PREVIEW));
  const text = r.shown[0]!.lines.join("\n");
  for (const want of ["a_1, b_1", "a_1 (from a): reaches calls m1", "b_1 (from b): reaches nothing outside", "@memory = m1", "no code", "nothing was created"]) {
    assert.ok(text.includes(want), `${want}: ${text}`);
  }
});

test("nothing_more_is_sent_until_approved_and_cancel_sends_nothing", async () => {
  // while the dialog is open (its answer pending) only the preview has been sent
  let release!: (a: ImportAnswer) => void;
  const pending = new Promise<ImportAnswer>((res) => (release = res));
  const sent: Rec[] = [];
  const run = runImportDrop({ item: ITEM, parentId: "home", send: async (p) => (sent.push(p), PREVIEW), show: () => pending });
  await new Promise((r) => setTimeout(r, 5));
  assert.equal(sent.length, 1);
  release(no);
  await run;
  assert.equal(sent.length, 1, "Cancel sends nothing more");
});

test("approve_resends_the_same_call_with_approve_true_and_bind", async () => {
  const r = rig((p) => (p["approve"] === true ? { imported: 2, ids: ["a_1", "b_1"], parent_id: "home", ports: {} } : PREVIEW));
  await runImportDrop(r.drop(yes));
  assert.equal(r.sent.length, 2);
  assert.deepEqual({ ...r.sent[1]!, approve: undefined }, { ...r.sent[0]!, approve: undefined });
  assert.equal(r.sent[1]!["approve"], true);
  assert.equal("allow_code" in r.sent[1]!, false);
  assert.equal(r.shown.at(-1)!.kind, "done");
  assert.ok(r.shown.at(-1)!.lines[0]!.includes("a_1, b_1"));

  // unbound ports: asked for, previewed again WITH the bind, approved with it
  const unbound = { error: 'unbound required ports: @memory (memory), @exec (exec): pass bind {"@memory": "<agent id>"}', reason: "unbound_ports" };
  assert.deepEqual(portsOf(unbound), ["@memory", "@exec"]);
  const b = rig((p) => {
    const bind = p["bind"] as Rec;
    if (Object.keys(bind).length === 0) return unbound;
    return p["approve"] === true ? { imported: 2, ids: ["a_1"], parent_id: "home", ports: bind } : PREVIEW;
  });
  await runImportDrop(b.drop((v) => (v.kind === "bind" ? { approve: true, bind: { "@memory": "m1", "@exec": "x1" }, allowCode: false } : yes)));
  assert.equal(b.sent.length, 3);
  assert.deepEqual(b.sent[2]!["bind"], { "@memory": "m1", "@exec": "x1" });
  assert.equal(b.sent[2]!["approve"], true);
  assert.deepEqual(b.shown[0]!.ports, ["@memory", "@exec"]);

  // code in the preview needs its own tick: without it the approval carries no allow_code
  const withCode = { ...PREVIEW, code: ["b_1: exec_bridge.tools"] };
  const c = rig((p) => (p["approve"] === true ? { imported: 1, ids: ["a_1"] } : withCode));
  await runImportDrop(c.drop(yes));
  assert.equal(c.shown[0]!.code, true);
  assert.equal("allow_code" in c.sent[1]!, false);
  const d = rig((p) => (p["approve"] === true ? { imported: 1, ids: ["a_1"] } : withCode));
  await runImportDrop(d.drop({ ...yes, allowCode: true }));
  assert.equal(d.sent[1]!["allow_code"], true);
});

test("the_done_line_lists_the_ids_of_the_verbs_map", async () => {
  // the verb replies `ids` as {blueprint id: new id}
  const r = rig((p) => (p["approve"] === true ? { imported: 2, ids: { a: "a_1", b: "b_1" }, parent_id: "home", ports: {} } : PREVIEW));
  await runImportDrop(r.drop(yes));
  assert.equal(r.shown.at(-1)!.kind, "done");
  assert.equal(r.shown.at(-1)!.lines[0], "imported a_1, b_1");
});

test("a_refusal_is_shown_with_its_message_and_sends_nothing_else", async () => {
  const msg = "the blueprint carries code: pass allow_code: true";
  const r = rig(() => ({ error: msg, reason: "code_not_allowed" }));
  await runImportDrop(r.drop(yes));
  assert.equal(r.sent.length, 1);
  assert.equal(r.shown.length, 1);
  assert.equal(r.shown[0]!.kind, "refused");
  assert.deepEqual(r.shown[0]!.lines, [`✕ ${msg}`]);
});

test("drop_parent_is_the_cell_under_the_pointer_else_the_canvas_host", () => {
  const where = { backendOf: (id: string) => (id === "cell1" ? "backend1" : undefined), canvasHost: "canvas_node" };
  const over = (id: string) => ({ closest: (sel: string) => (sel === ".agent-frame" ? { dataset: { id } } : null) });
  assert.equal(dropParent(over("cell1"), where), "backend1");
  // a cell with no backend, a drop on bare canvas, and no target: the canvas's host node
  assert.equal(dropParent(over("cell2"), where), "canvas_node");
  assert.equal(dropParent({ closest: () => null }, where), "canvas_node");
  assert.equal(dropParent(null, where), "canvas_node");
});
