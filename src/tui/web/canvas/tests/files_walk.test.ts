import { test } from "node:test";
import assert from "node:assert/strict";
import { AgentWalker } from "../src/bundles/files/agent_walk.ts";
import { AgentIndex, agentIdOf, badge, badgeOf, side } from "../src/bundles/files/agent_index.ts";
import type { FileBridge } from "../src/bundles/files/file_bridge.ts";
import { FakeBridge } from "./fake_bridge.ts";

// THE AGENT FOLDERS: the walk over `.fantastic` (what it reads, what it says when
// it cannot), the agent.yaml head reader, and the badge each agent wears.

const yaml = (id: string, extra = ""): string => `id: ${id}\nhandler_module: ${extra || "x.ts"}\n`;

function walkerOn(b: FileBridge): { w: AgentWalker; ix: AgentIndex; after: number[] } {
  const ix = new AgentIndex();
  const after: number[] = [];
  const w = new AgentWalker(b, ix, () => after.push(after.length + 1));
  return { w, ix, after };
}

test("the walk finds each agent.yaml's id, skips ignored folders, and says the folders it cannot read", async () => {
  const b = new FakeBridge();
  b.put(".fantastic/agents/a/agent.yaml", yaml("alpha", "exec_bridge.py"));
  b.put(".fantastic/agents/b/agent.yaml", "handler_module: x.ts\n");
  b.put(".fantastic/agents/c/agent.yaml", "id:\n");
  b.put(".fantastic/node_modules/z/agent.yaml", yaml("hidden"));
  b.put(".fantastic/agents/bin.yaml/agent.yaml", yaml("deep"));
  b.binary.add(".fantastic/agents/bin.yaml/agent.yaml");
  const { w, ix, after } = walkerOn(b);
  await w.request();
  assert.deepEqual(ix.ids().sort(), ["alpha"], "the id is the yaml's, not the folder's name; the bad ones are not agents");
  assert.equal(ix.folderOf("alpha"), ".fantastic/agents/a");
  assert.equal(w.done, true);
  assert.equal(after.length, 1, "the page is told once");
  assert.match(w.errors.get(".fantastic/agents/b") ?? "", /names no id/);
  assert.match(w.errors.get(".fantastic/agents/c") ?? "", /names no id/);
  assert.match(w.errors.get(".fantastic/agents/bin.yaml") ?? "", /binary|text/);
  assert.equal(w.errors.has(".fantastic/node_modules/z"), false, "an ignored folder is not walked");
});

test("no .fantastic yet is no error; a bridge that does not expose it (its allow list) is said on the row", async () => {
  const empty = new FakeBridge();
  const a = walkerOn(empty);
  await a.w.request();
  assert.equal(a.w.errors.size, 0, "not_found is quiet");
  assert.equal(a.w.done, true);

  const denied = new FakeBridge();
  const was = denied.call.bind(denied);
  denied.call = async (p) =>
    p["type"] === "list" && p["path"] === ".fantastic"
      ? { error: "files: list refused", reason: "not_found", hint: "add .fantastic to the allow list" }
      : was(p);
  const d = walkerOn(denied);
  await d.w.request();
  assert.equal(d.w.errors.get(".fantastic"), "files: list refused — add .fantastic to the allow list");
});

test("a bridge call that throws is said on the .fantastic row, and the walk is still done", async () => {
  const b = new FakeBridge();
  b.call = async () => {
    throw new Error("boom");
  };
  const { w, after } = walkerOn(b);
  await w.request();
  assert.equal(w.errors.get(".fantastic"), "the agent folders could not be read: boom");
  assert.equal(w.done, true);
  assert.equal(after.length, 1);
});

test("the walk goes down twelve levels and no further", async () => {
  const b = new FakeBridge();
  let deep = ".fantastic";
  for (let i = 1; i <= 13; i++) {
    deep = `${deep}/d${i}`;
    b.mkdir(deep);
    if (i === 12) b.put(`${deep}/agent.yaml`, yaml("at_twelve"));
    if (i === 13) b.put(`${deep}/agent.yaml`, yaml("at_thirteen"));
  }
  const { w, ix } = walkerOn(b);
  await w.request();
  assert.deepEqual(ix.ids(), ["at_twelve"], "the folder at depth 12 is read; depth 13 is not reached");
});

test("a request that lands while the walk runs makes it run once more, and both requests share the one run", async () => {
  const b = new FakeBridge();
  b.put(".fantastic/agents/a/agent.yaml", yaml("one"));
  const { w, after } = walkerOn(b);
  const first = w.request();
  const second = w.request();
  assert.equal(first, second, "the second request joins the running walk");
  await first;
  assert.equal(after.length, 2, "one more run after the one in flight");
  assert.equal(b.calls.filter((c) => c["type"] === "list" && c["path"] === ".fantastic").length, 2);
  await w.request();
  assert.equal(after.length, 3, "an idle request runs once");
});

test("a second walk forgets the agents the first one found and the errors it had", async () => {
  const b = new FakeBridge();
  b.put(".fantastic/agents/a/agent.yaml", yaml("gone_soon"));
  b.put(".fantastic/agents/b/agent.yaml", "handler_module: x.ts\n");
  const { w, ix } = walkerOn(b);
  await w.request();
  assert.deepEqual(ix.ids(), ["gone_soon"]);
  assert.equal(w.errors.size, 1);
  b.remove(".fantastic/agents/a");
  b.remove(".fantastic/agents/b");
  await w.request();
  assert.deepEqual(ix.ids(), []);
  assert.equal(w.errors.size, 0);
});

test("agentIdOf: the top-level keys only; a null or empty value is none; quotes come off", () => {
  const head = [
    "display_name: \"The Alpha\"",
    "  id: nested_not_me",
    "id: 'alpha'",
    "handler_module: exec_bridge.py",
    "parent_id: ~",
    "memory_store_id: store_1",
  ];
  const r = agentIdOf(head);
  assert.equal(r.id, "alpha");
  assert.equal(r.handler, "exec_bridge.py");
  assert.equal(r.displayName, "The Alpha");
  assert.equal(r.hasParent, false, "parent_id: ~ names no parent");
  assert.equal(r.hasMemory, true);
  assert.equal(agentIdOf(["id: null", "handler_module: \"\""]).id, null);
  assert.equal(agentIdOf(["id: null", "handler_module: \"\""]).handler, null);
  assert.equal(agentIdOf([]).id, null);
  assert.equal(agentIdOf(["id:"]).id, null, "an empty id is none");
});

test("badges: an AI agent (a non-root with a memory store) is ✦; then the handler's kind; badgeOf reads the same from the yaml's flags", () => {
  assert.equal(badge({ memory_store_id: "m", parent_id: "p" }, "x.ts"), "✦");
  assert.equal(badge({ memory_store_id: "m" }, "x.ts"), "◻", "a root with a store is not an AI agent");
  assert.equal(badge({ parent_id: "p" }, "exec_bridge.py"), "⌨");
  assert.equal(badge({}, "file_store.tools"), "▣");
  assert.equal(badge({ memory_store_id: 5, parent_id: "p" }, "x.ts"), "◻", "a memory store must be named");
  assert.equal(badge({}, "x.ts"), "◻");
  assert.equal(badge({}, "terminal.py"), "◇");
  const info = (over: Partial<{ hasParent: boolean; hasMemory: boolean; handler: string }>) => ({ id: "i", handler: "x.ts", displayName: "", hasParent: false, hasMemory: false, ...over });
  assert.equal(badgeOf(info({ hasParent: true, hasMemory: true })), "✦");
  assert.equal(badgeOf(info({ hasMemory: true })), "◻");
  assert.equal(badgeOf(info({ handler: "exec_bridge.py" })), "⌨");
  assert.equal(side("page.ts"), "page");
  assert.equal(side("exec_bridge.py"), "host");
});

test("the walk: a yaml that is not a list of lines, or a listing that carries no files, is read as nothing", async () => {
  const b = new FakeBridge();
  b.put(".fantastic/agents/a/agent.yaml", "id: a\n");
  b.put(".fantastic/agents/x/agent.yaml", "id: x\n");
  const was = b.call.bind(b);
  b.call = async (p) => {
    if (p["type"] === "read" && p["path"] === ".fantastic/agents/a/agent.yaml") return { path: p["path"], lines: "not lines" };
    if (p["type"] === "list" && p["path"] === ".fantastic/agents/x") return { path: ".fantastic/agents/x", files: "not files" };
    return was(p);
  };
  const { w, ix } = walkerOn(b);
  await w.request();
  assert.deepEqual(ix.ids(), [], "no id is read from a yaml that is not lines, and a listing with no files has no agents");
  assert.match(w.errors.get(".fantastic/agents/a") ?? "", /names no id/);
});

test("an agent whose yaml names no handler or display name is an agent with empty ones", async () => {
  const b = new FakeBridge();
  b.put(".fantastic/agents/bare/agent.yaml", "id: bare\n");
  const { w, ix } = walkerOn(b);
  await w.request();
  const info = ix.agentAt(".fantastic/agents/bare");
  assert.equal(info?.id, "bare");
  assert.equal(info?.handler, "");
  assert.equal(info?.displayName, "");
});

test("the walk: a yaml line with no text is an empty line, so the id is not in it", async () => {
  const b = new FakeBridge();
  b.put(".fantastic/agents/t/agent.yaml", "id: t\n");
  const was = b.call.bind(b);
  b.call = async (p) => (p["type"] === "read" && p["path"] === ".fantastic/agents/t/agent.yaml" ? { path: p["path"], lines: [{ n: 1 }, { n: 2, text: "id: t" }] } : was(p));
  const { w, ix } = walkerOn(b);
  await w.request();
  assert.deepEqual(ix.ids(), ["t"], "the second line is the id; the first, with no text, is nothing");
});
