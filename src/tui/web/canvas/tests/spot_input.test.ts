import { test } from "node:test";
import assert from "node:assert/strict";
import { rows } from "../src/bundles/canvas/spot_input.ts";

// THE SPOT INPUT: what is typed at a spot decides what the rows offer.
const presets = {
  templates: [
    { name: "opus", handler_module: "anthropic_backend.tools", model: "opus" },
    { name: "gpt5", handler_module: "responses_backend.tools", model: "gpt-5" },
  ],
  preferred: "gpt5",
};

test("free text asks the default AI first, a new cell at the spot", () => {
  const r = rows("put a player next to the video", presets);
  assert.deepEqual(r[0]!.pick, { kind: "ai", template: null, text: "put a player next to the video" });
  assert.equal(r[0]!.label, "ask gpt5");
});

test("with no default template the top row opens set-up — nothing is guessed", () => {
  assert.deepEqual(rows("hello", { templates: presets.templates, preferred: null })[0]!.pick, { kind: "setup" });
  assert.deepEqual(rows("hello", null)[0]!.pick, { kind: "setup" });
});

test("a kind typed exactly offers only the kind; a prefix offers it after the AI", () => {
  assert.deepEqual(rows("terminal", presets).map((x) => x.pick), [{ kind: "terminal" }]);
  const t = rows("te", presets);
  assert.equal(t[0]!.pick.kind, "ai");
  assert.deepEqual(t[1]!.pick, { kind: "terminal" });
});

test("@name picks a template and the rest is the first turn", () => {
  assert.deepEqual(rows("@op summarize this", presets).map((x) => x.pick), [
    { kind: "ai", template: "opus", text: "summarize this" },
  ]);
});

test("empty lists everything available", () => {
  const labels = rows("", presets).map((x) => x.label);
  assert.deepEqual(labels, ["new AI cell · gpt5", "terminal", "html", "@opus"]);
});
