import { test } from "node:test";
import assert from "node:assert/strict";
import { COLS, move, tiles } from "../src/bundles/canvas/action_popup.ts";
import type { Presets } from "../src/bundles/canvas/action_popup.ts";

const P: Presets = {
  templates: [
    { name: "gpt5", handler_module: "responses_backend.tools", model: "gpt-5" },
    { name: "box", handler_module: "vllm_backend.tools", model: "qwen" },
  ],
  preferred: "gpt5",
  dialogs: [{ id: "ai_1", name: "release notes" }],
};

test("empty: every section — do, AI (★ the default), chats", () => {
  const t = tiles("", P);
  assert.deepEqual(t.map((x) => x.label), ["ask", "terminal", "html", "stream", "canvas", "agent", "demo", "@gpt5 ★", "@box", "release notes"]);
  assert.deepEqual([...new Set(t.map((x) => x.section))], ["do", "ai", "chats"]);
  assert.deepEqual(t.at(-1)!.pick, { kind: "chat", id: "ai_1", name: "release notes" });
});

test("a kind typed exactly offers only that kind", () => {
  assert.deepEqual(tiles("terminal", P).map((x) => x.pick), [{ kind: "terminal" }]);
  assert.deepEqual(tiles("agent", P).map((x) => x.pick), [{ kind: "setup" }]);
  assert.deepEqual(tiles("demo", P).map((x) => x.pick), [{ kind: "demo" }]);
});

test("free text: the first tile asks the default; the rest filter", () => {
  const t = tiles("rel", P);
  assert.deepEqual(t[0]!.pick, { kind: "ai", template: null, text: "rel" });
  assert.deepEqual(t.slice(1).map((x) => x.label), ["release notes"]);
});

test("@name picks a template, the rest is the first turn", () => {
  const t = tiles("@bo hi there", P);
  assert.equal(t.length, 1);
  assert.deepEqual(t[0]!.pick, { kind: "ai", template: "box", text: "hi there" });
});

test("no default template → the first tile opens set-up; nothing guessed", () => {
  const t = tiles("hello", { templates: [], preferred: null, dialogs: [] });
  assert.deepEqual(t.map((x) => x.pick), [{ kind: "setup" }]);
  assert.equal(tiles("", null)[0]!.pick.kind, "setup");
});

test("grid moves: across by 1, down/up by a row, clamped", () => {
  assert.equal(move(0, "ArrowRight", 8), 1);
  assert.equal(move(0, "ArrowLeft", 8), 0);
  assert.equal(move(0, "ArrowDown", 8), COLS);
  assert.equal(move(6, "ArrowDown", 8), 7);
  assert.equal(move(6, "ArrowUp", 8), 6 - COLS);
  assert.equal(move(0, "ArrowDown", 0), 0);
});
