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

test("empty: two groups — host (ask, terminal, agent, python agent), page (html, stream, demo, sidebar, canvases) — then AI (★ the default), chats", () => {
  const t = tiles("", P);
  assert.deepEqual(t.map((x) => x.label), ["ask", "terminal", "agent", "python agent", "html", "stream", "demo", "sidebar", "canvases", "@gpt5 ★", "@box", "release notes"]);
  assert.deepEqual(t.find((x) => x.label === "python agent")!.pick, { kind: "bundle", starter: "python_agent" });
  assert.deepEqual([...new Set(t.map((x) => x.section))], ["host", "page", "ai", "chats"]);
  assert.deepEqual(t.at(-1)!.pick, { kind: "chat", id: "ai_1", name: "release notes" });
});

test("a kind typed exactly offers only that kind", () => {
  assert.deepEqual(tiles("terminal", P).map((x) => x.pick), [{ kind: "terminal" }]);
  assert.deepEqual(tiles("agent", P).map((x) => x.pick), [{ kind: "setup" }]);
  assert.deepEqual(tiles("demo", P).map((x) => x.pick), [{ kind: "demo" }]);
  assert.deepEqual(tiles("sidebar", P).map((x) => x.pick), [{ kind: "sidebar" }]);
  assert.deepEqual(tiles("canvases", P).map((x) => x.pick), [{ kind: "canvases" }]);
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

test("a plugin template is a HOST tile (not an AI one); picking it places its agent", () => {
  const withPlugin: Presets = { ...P, templates: [...P.templates, { name: "web_search", handler_module: "exec_bridge.tools", model: "", kind: "agent" }] };
  const t = tiles("", withPlugin);
  const plugin = t.find((x) => x.label === "web_search")!;
  assert.equal(plugin.section, "host");
  assert.deepEqual(plugin.pick, { kind: "plugin", template: "web_search" });
  assert.ok(!t.some((x) => x.label.startsWith("@web_search")), "not offered as an AI template");
  assert.deepEqual(tiles("web", withPlugin).slice(1).map((x) => x.label), ["web_search"], "typing filters plugins too");
});

test("coding agents found on PATH: their own group, after host, launching the full path", () => {
  const t = tiles("", { ...P, coding: [{ name: "claude", path: "/opt/bin/claude" }, { name: "aider", path: "/u/.local/bin/aider" }] });
  const coding = t.filter((x) => x.section === "coding");
  assert.deepEqual(coding.map((x) => x.label), ["claude", "aider"]);
  assert.deepEqual(coding[0]!.pick, { kind: "coding", name: "claude", path: "/opt/bin/claude" });
  assert.equal(coding[0]!.icon, "claude");
  assert.deepEqual([...new Set(t.map((x) => x.section))], ["host", "coding", "page", "ai", "chats"]);
  assert.deepEqual(tiles("cl", { ...P, coding: [{ name: "claude", path: "/c" }] }).filter((x) => x.section === "coding").map((x) => x.label), ["claude"]);
});
