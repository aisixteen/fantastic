import { test } from "node:test";
import assert from "node:assert/strict";
import { AgentIndex, agentIdOf, badge, side } from "../src/bundles/files/agent_index.ts";

test("agentIdOf reads the top-level id, never an indented one", () => {
  const y = agentIdOf(["handler_module: file_bridge.tools", "id: files", "parent_id: user", "can_call:", "  id: nested", "display_name: 'The files'"]);
  assert.equal(y.id, "files");
  assert.equal(y.handler, "file_bridge.tools");
  assert.equal(y.displayName, "The files");
  assert.equal(agentIdOf(["can_call:", "  id: nested"]).id, null);
  assert.equal(agentIdOf(["id: null"]).id, null);
});

test("folderOf, agentAt and drop (a folder or everything below it)", () => {
  const ix = new AgentIndex();
  const info = (id: string) => ({ id, handler: "x.tools", displayName: "" });
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

test("badge: ✦ for the six AI handlers, ▣ the five stores, ⌨ exec, ◻ .ts, ◇ else", () => {
  for (const h of ["anthropic_backend.tools", "ollama_backend.tools", "responses_backend.tools", "vllm_backend.tools", "foundation_models_backend.tools", "ai_chat.tools"]) assert.equal(badge(h), "✦", h);
  assert.equal(badge("kernel_state.tools"), "◇");
  for (const h of ["ram_store.tools", "sqlite_store.tools", "credential_store.tools", "user_home_store.tools", "user_store.tools"]) assert.equal(badge(h), "▣", h);
  assert.equal(badge("exec_bridge.tools"), "⌨");
  assert.equal(badge("canvas.ts"), "◻");
  assert.equal(badge("file_bridge.tools"), "◇");
});

test("side: a .ts handler is the page's", () => {
  assert.equal(side("canvas.ts"), "page");
  assert.equal(side("file_bridge.tools"), "host");
});
