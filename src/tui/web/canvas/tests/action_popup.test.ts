import { test } from "node:test";
import assert from "node:assert/strict";
import { COLS, exactLine, layout, move, tiles } from "../src/bundles/canvas/action_popup.ts";
import type { Presets } from "../src/bundles/canvas/action_popup.ts";

const P: Presets = {
  preferred: "gpt5",
  dialogs: [{ id: "ai_1", name: "release notes" }],
};

test("empty: two groups — host (AI agent, terminal, python agent, mcp connector, user store), page (html, stream, file tree, preview, canvas theme, tips) — then chats; no AI saved set-up tiles", () => {
  const t = tiles("", P);
  assert.deepEqual(t.map((x) => x.label), ["AI agent", "terminal", "python agent", "mcp connector", "user store", "html", "stream", "file tree", "preview", "canvas theme", "tips", "release notes"]);
  assert.deepEqual(t.find((x) => x.label === "python agent")!.pick, { kind: "bundle", starter: "python_agent" });
  assert.deepEqual([...new Set(t.map((x) => x.section))], ["host", "page", "chats"]);
  assert.deepEqual(t.at(-1)!.pick, { kind: "chat", id: "ai_1", name: "release notes" });
});

test("a kind typed exactly offers only that kind", () => {
  assert.deepEqual(tiles("terminal", P).map((x) => x.pick), [{ kind: "terminal" }]);
  assert.deepEqual(tiles("new ai agent…", P).map((x) => x.label), ["AI agent · gpt5"], "no \"new AI agent…\": only free text for the one AI agent tile");
  assert.deepEqual(tiles("file tree", P).map((x) => x.pick), [{ kind: "files" }]);
  assert.deepEqual(tiles("preview", P).map((x) => x.pick), [{ kind: "preview" }]);
  assert.deepEqual(tiles("canvas theme", P).map((x) => x.pick), [{ kind: "backgrounds" }]);
});

test("free text: the first tile asks the default; the rest filter", () => {
  const t = tiles("rel", P);
  assert.deepEqual(t[0]!.pick, { kind: "ai", text: "rel" });
  assert.deepEqual(t.slice(1).map((x) => x.label), ["release notes"]);
});

test("no AI set up: the first tile, \"AI agent\", opens the wizard (typed or not) — nothing guessed", () => {
  const t = tiles("hello", { preferred: null, dialogs: [] });
  assert.deepEqual(t.map((x) => x.pick), [{ kind: "setup" }]);
  const empty = tiles("", null);
  assert.ok(!empty.some((x) => x.pick.kind === "ai"), "no default AI tile without a default");
  assert.deepEqual(empty[0]!.label, "AI agent");
  assert.deepEqual(empty[0]!.pick, { kind: "setup" }, "with none set up, \"AI agent\" opens the wizard");
});

test("grid moves: across by 1, down/up by a row, clamped", () => {
  // two groups: host 7 tiles (rows 0-1), page 3 tiles (row 2) — each group starts a row
  const g = layout([7, 3], COLS);
  assert.deepEqual(g.slice(5, 8), [{ row: 1, col: 0 }, { row: 1, col: 1 }, { row: 2, col: 0 }]);
  assert.equal(move(0, "ArrowRight", g), 1);
  assert.equal(move(0, "ArrowLeft", g), 0);
  assert.equal(move(0, "ArrowDown", g), COLS); // same column, next row
  // ↓ from row 1 col 1 lands on row 2 — the page group — col 1, not index 6 + COLS
  assert.equal(move(6, "ArrowDown", g), 8);
  // ↓ from row 0 col 4 into the shorter row 1: the nearest column (col 1)
  assert.equal(move(4, "ArrowDown", g), 6);
  // ↑ from the page group's col 2 back into host row 1: the nearest column
  assert.equal(move(9, "ArrowUp", g), 6);
  assert.equal(move(0, "ArrowUp", g), 0);
  assert.equal(move(0, "ArrowDown", []), 0);
});

test("AI set-ups are the AI agent's option, not the menu's: no tile, and `@name` is plain text", () => {
  assert.ok(!tiles("", P).some((x) => x.label.startsWith("@")), "no set-up tiles on an empty field");
  assert.deepEqual(tiles("@box hi", P).map((x) => x.pick), [{ kind: "ai", text: "@box hi" }], "`@name` no longer picks one: it is the default's first turn");
  assert.deepEqual(tiles("bo", P).map((x) => x.label), ["AI agent · gpt5"], "typing a saved set-up's name shows no tile for it");
  assert.equal(tiles("", P)[0]!.hint, "★ gpt5", "the default is named on the AI agent tile");
});

test("fantastic on PATH: the FIRST of the coding agents", () => {
  const t = tiles("", { ...P, coding: [{ name: "fantastic", path: "/u/.local/bin/fantastic" }, { name: "claude", path: "/c" }] });
  const f = t.find((x) => x.label === "fantastic")!;
  assert.equal(f.section, "coding");
  assert.deepEqual(t.filter((x) => x.section === "coding").map((x) => x.label), ["fantastic", "claude"]);
  assert.deepEqual(f.pick, { kind: "coding", name: "fantastic", path: "/u/.local/bin/fantastic" });
});

test("coding agents found on PATH: their own group, after host, launching the full path", () => {
  const t = tiles("", { ...P, coding: [{ name: "claude", path: "/opt/bin/claude" }, { name: "aider", path: "/u/.local/bin/aider" }] });
  const coding = t.filter((x) => x.section === "coding");
  assert.deepEqual(coding.map((x) => x.label), ["claude", "aider"]);
  assert.deepEqual(coding[0]!.pick, { kind: "coding", name: "claude", path: "/opt/bin/claude" });
  assert.equal(coding[0]!.icon, "claude");
  assert.deepEqual([...new Set(t.map((x) => x.section))], ["host", "coding", "page", "chats"]);
  assert.deepEqual(tiles("cl", { ...P, coding: [{ name: "claude", path: "/c" }] }).filter((x) => x.section === "coding").map((x) => x.label), ["claude"]);
});

test("exactLine: the exec allow-list entry for exactly one argv (shlex-joined, regex-escaped)", () => {
  assert.equal(exactLine(["/Users/me/.local/bin/claude"]), "/Users/me/\\.local/bin/claude");
  assert.equal(exactLine(["/opt/my tools/codex", "--x"]), "'/opt/my tools/codex' --x");
  assert.ok(new RegExp(`^${exactLine(["/a/b.c"])}$`).test("/a/b.c"));
  assert.ok(!new RegExp(`^${exactLine(["/a/b.c"])}$`).test("/a/bxc"));
});

test("the project's own bundles: their own group after the coding agents, each placing one more instance", () => {
  const t = tiles("", { ...P, bundles: [{ name: "project_dashboard", sentence: "a dashboard" }, { name: "py_61a709", sentence: "" }] });
  const b = t.filter((x) => x.section === "bundles");
  assert.deepEqual(b.map((x) => x.label), ["project_dashboard", "py_61a709"]);
  assert.deepEqual(b[0]!.pick, { kind: "instance", bundle: "project_dashboard" });
  assert.deepEqual(tiles("proj", { ...P, bundles: [{ name: "project_dashboard", sentence: "" }] }).filter((x) => x.section === "bundles").map((x) => x.label), ["project_dashboard"]);
  // its exact name: that tile alone, so ⏎ places it
  assert.deepEqual(tiles("project_dashboard", { ...P, bundles: [{ name: "project_dashboard", sentence: "" }] }).map((x) => x.pick), [{ kind: "instance", bundle: "project_dashboard" }]);
});

test("this canvas's page bundles (an html cell bundled): their own group, each placing one more instance", () => {
  const t = tiles("", { ...P, pageBundles: [{ name: "card", sentence: "a card" }] });
  const b = t.filter((x) => x.section === "page_bundles");
  assert.deepEqual(b.map((x) => [x.label, x.pick]), [["card", { kind: "page_instance", bundle: "card" }]]);
  assert.deepEqual(tiles("card", { ...P, pageBundles: [{ name: "card", sentence: "" }] }).map((x) => x.pick), [{ kind: "page_instance", bundle: "card" }]);
});

test("ONE way to make an AI: the \"AI agent\" tile — typed exactly, that tile alone (the default, or the wizard with none)", () => {
  assert.deepEqual(tiles("AI agent", P).map((x) => x.pick), [{ kind: "ai", text: "" }]);
  assert.deepEqual(tiles("ai agent", { preferred: null, dialogs: [] }).map((x) => x.pick), [{ kind: "setup" }]);
  assert.equal(tiles("", P).filter((x) => x.label.toLowerCase().includes("ai agent")).length, 1);
});
