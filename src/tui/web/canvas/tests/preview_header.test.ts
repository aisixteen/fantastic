import { test } from "node:test";
import assert from "node:assert/strict";
import { AgentIndex } from "../src/bundles/files/agent_index.ts";
import { agentChain, crumbs, kindOf, lineCount, metaOf, noViewHandler, sizeText, trail } from "../src/bundles/preview/header_model.ts";

test("crumbs: the path's segments, the root named project, nothing for no path", () => {
  assert.deepEqual(crumbs("deploy/api/api.yaml"), ["deploy", "api", "api.yaml"]);
  assert.deepEqual(crumbs(""), ["project"]);
  assert.deepEqual(crumbs(null), []);
});

test("kindOf: folder, file, agent", () => {
  assert.equal(kindOf("dir", false), "folder");
  assert.equal(kindOf("text", false), "file");
  assert.equal(kindOf("text", true), "agent");
});

test("sizeText and lineCount", () => {
  assert.equal(sizeText(1), "1 byte");
  assert.equal(sizeText(612), "612 bytes");
  assert.equal(sizeText(2048), "2.0 KiB");
  assert.equal(sizeText(-1), "unknown size");
  assert.equal(lineCount(""), 0);
  assert.equal(lineCount("a"), 1);
  assert.equal(lineCount("a\nb\n"), 2);
  assert.equal(lineCount("a\n\nb"), 3);
});

test("metaOf: what is known, in order", () => {
  assert.equal(metaOf({ name: "api.yaml", size: 612, text: "a\nb\n" }), "2 lines · 612 bytes · yaml");
  assert.equal(metaOf({ name: "Makefile", size: 10 }), "10 bytes");
  assert.equal(metaOf({ name: "x.png", size: -1 }), "png");
});

const idx = (): AgentIndex => {
  const ix = new AgentIndex();
  ix.set("agents/lead", { id: "lead", handler: "ai_chat.tools", displayName: "lead" });
  ix.set("agents/lead/agents/ops", { id: "ops", handler: "ai_chat.tools", displayName: "cluster ops", parentId: "lead" });
  ix.set("agents/lead/agents/ops/agents/k", { id: "k-rollout", handler: "ai_chat.tools", displayName: "", parentId: "ops" });
  return ix;
};

test("agentChain: the parent_id chain, outermost first, as far as the index knows it", () => {
  assert.deepEqual(agentChain("k-rollout", idx()), ["lead", "cluster ops", "k-rollout"]);
  assert.deepEqual(agentChain("ghost", idx()), ["ghost"], "an agent the index does not hold shows its id");
  const loop = new AgentIndex();
  loop.set("a", { id: "a", handler: "", displayName: "", parentId: "b" });
  loop.set("b", { id: "b", handler: "", displayName: "", parentId: "a" });
  assert.deepEqual(agentChain("a", loop), ["b", "a"], "a cycle ends");
});

test("trail: the agent chain, then the path below the agent's folder", () => {
  assert.deepEqual(trail("agents/lead/agents/ops/agents/k/deploy/api.yaml", idx()), ["lead", "cluster ops", "k-rollout", "deploy", "api.yaml"]);
  assert.deepEqual(trail("src/a.ts", idx()), ["src", "a.ts"], "outside an agent folder: the plain path");
  assert.deepEqual(trail(null, idx()), []);
});

test("noViewHandler: only the refusal of an agent with no view", () => {
  assert.equal(noViewHandler("no view for task.tools"), "task.tools");
  assert.equal(noViewHandler("no agent x"), null);
});
