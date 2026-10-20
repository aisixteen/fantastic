import { test } from "node:test";
import assert from "node:assert/strict";
import { AgentIndex, agentIdOf, badge, badgeOf, side } from "../src/bundles/files/agent_index.ts";

test("agentIdOf reads the top-level id, never an indented one", () => {
  const y = agentIdOf(["handler_module: file_bridge.tools", "id: files", "parent_id: user", "can_call:", "  id: nested", "display_name: 'The files'"]);
  assert.equal(y.id, "files");
  assert.equal(y.handler, "file_bridge.tools");
  assert.equal(y.displayName, "The files");
  assert.equal(y.hasParent, true);
  assert.equal(y.hasMemory, false);
  assert.equal(agentIdOf(["can_call:", "  id: nested"]).id, null);
  assert.equal(agentIdOf(["id: null"]).id, null);
});

test("folderOf, agentAt and drop (a folder or everything below it)", () => {
  const ix = new AgentIndex();
  const info = (id: string) => ({ id, handler: "x.tools", displayName: "", hasParent: true, hasMemory: false });
  ix.set(".fantastic/web/agents/canvas", info("canvas_root"));
  ix.set(".fantastic/web/agents/canvas/agents/view", info("view_root"));
  ix.set(".fantastic/agents/user", info("user"));
  assert.equal(ix.folderOf("canvas_root"), ".fantastic/web/agents/canvas", "the id is not the folder's name");
  assert.equal(ix.agentAt(".fantastic/agents/user")!.id, "user");
  ix.drop(".fantastic/web");
  assert.equal(ix.folderOf("view_root"), undefined);
  assert.equal(ix.folderOf("user"), ".fantastic/agents/user");
  assert.deepEqual(ix.ids(), ["user"]);
});

test("badge: ✦ only for a non-root agent with a memory store, ▣ store, ⌨ exec, ◻ .ts, ◇ else", () => {
  assert.equal(badge({ memory_store_id: "m", parent_id: "p" }, "ai_core.tools"), "✦");
  assert.equal(badge({ memory_store_id: "m" }, "kernel_state.tools"), "◇", "the root names the project store; it is not an AI");
  assert.equal(badge({}, "ram_store.tools"), "▣");
  assert.equal(badge({}, "exec_bridge.tools"), "⌨");
  assert.equal(badge({}, "canvas.ts"), "◻");
  assert.equal(badge({}, "file_bridge.tools"), "◇");
  assert.equal(badgeOf({ id: "a", handler: "ai_core.tools", displayName: "", hasParent: true, hasMemory: true }), "✦");
});

test("side: a .ts handler is the page's", () => {
  assert.equal(side("canvas.ts"), "page");
  assert.equal(side("file_bridge.tools"), "host");
});
