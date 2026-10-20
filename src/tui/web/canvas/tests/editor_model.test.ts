import { test } from "node:test";
import assert from "node:assert/strict";
import { EditorModel } from "../src/bundles/preview/editor_model.ts";

function dirty(): EditorModel {
  const e = new EditorModel();
  e.load("one", "v1");
  e.edit("one two");
  return e;
}

test("save sends if_match = the base version; the echo of our own write is ignored", () => {
  const e = dirty();
  assert.deepEqual(e.save(), [{ kind: "write", bytes: "one two", ifMatch: "v1" }]);
  e.saved("v2");
  assert.equal(e.state.k, "viewing");
  assert.equal(e.text(), "one two");
  e.reread({ text: "one two", version: "v2" });
  assert.equal(e.state.k, "viewing");
  assert.equal(e.version(), "v2");
});

test("a save in flight takes no edits (the editor is read-only meanwhile)", () => {
  const e = dirty();
  e.save();
  e.edit("ignored while saving");
  e.saved("v2");
  assert.equal(e.state.k, "viewing", "a save in flight takes no edits");
});

test("an external change while viewing reloads", () => {
  const e = new EditorModel();
  e.load("a", "v1");
  assert.deepEqual(e.diskChanged(), [{ kind: "read" }]);
  e.reread({ text: "b", version: "v9" });
  assert.equal(e.text(), "b");
  assert.equal(e.version(), "v9");
});

test("an external change while dirty is a conflict and writes nothing", () => {
  const e = dirty();
  e.reread({ text: "disk wins", version: "v7" });
  assert.equal(e.state.k, "conflict");
  assert.equal(e.inConflict(), true);
  assert.equal(e.text(), "one two", "mine is untouched");
  assert.deepEqual(e.save(), [], "no save from a conflict");
  e.reread({ text: "one", version: "v1" }); // a re-read that shows the base again does not end it either
  assert.equal(e.state.k, "conflict");
});

test("dirty ignores a re-read of its own base", () => {
  const e = dirty();
  e.reread({ text: "one", version: "v1" });
  assert.equal(e.state.k, "dirty");
});

test("reload drops mine for theirs", () => {
  const e = dirty();
  e.reread({ text: "disk wins", version: "v7" });
  e.reload();
  assert.equal(e.state.k, "viewing");
  assert.equal(e.text(), "disk wins");
  assert.equal(e.isDirty(), false);
});

test("keep mine writes against the version just seen; refused again is a fresh conflict, never a blind write", () => {
  const e = dirty();
  e.reread({ text: "disk wins", version: "v7" });
  e.compare();
  assert.equal(e.state.k, "comparing");
  assert.deepEqual(e.keepMine(), [{ kind: "write", bytes: "one two", ifMatch: "v7" }]);
  assert.deepEqual(e.saveRefused({ reason: "version_mismatch" }), [{ kind: "read" }]);
  e.reread({ text: "third writer", version: "v8" });
  assert.equal(e.state.k, "conflict");
  assert.equal((e.state as { theirsVersion: string }).theirsVersion, "v8");
  assert.deepEqual(e.keepMine(), [{ kind: "write", bytes: "one two", ifMatch: "v8" }]);
});

test("a save refused with a version mismatch reads, then conflicts; any other refusal returns to dirty, said", () => {
  const e = dirty();
  e.save();
  assert.deepEqual(e.saveRefused({ reason: "version_mismatch" }), [{ kind: "read" }]);
  e.reread({ text: "elsewhere", version: "v3" });
  assert.equal(e.state.k, "conflict");

  const f = dirty();
  f.save();
  assert.deepEqual(f.saveRefused({ error: "readonly", reason: "readonly" }), []);
  assert.equal(f.state.k, "dirty");
  assert.equal(f.error, "readonly");
});

test("revert: edits drop to the base; a save is undone; otherwise nothing to revert", () => {
  const e = dirty();
  assert.deepEqual(e.revert(), { ok: true, what: "edits", effects: [] });
  assert.equal(e.text(), "one");
  assert.deepEqual(e.revert(), { ok: false, reason: "nothing_to_revert" });

  const f = dirty();
  f.save();
  f.saved("v2");
  assert.deepEqual(f.revert(), { ok: true, what: "save", effects: [{ kind: "undo" }, { kind: "read" }] });
  f.reread({ text: "one", version: "v1" });
  assert.equal(f.text(), "one");
  assert.deepEqual(f.revert(), { ok: false, reason: "nothing_to_revert" }, "undo is not redo here");
});
