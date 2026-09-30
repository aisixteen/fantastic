import { test } from "node:test";
import assert from "node:assert/strict";
import { line } from "../src/bundles/stream_view/stream_view.ts";

test("a send is sender → target verb · summary; other frames are skipped", () => {
  assert.deepEqual(
    line({ type: "state_event", agent_id: "memory", kind: "send", sender: "canvas_ws", summary: "{…}", payload: { type: "read" } }),
    ["canvas_ws", "memory", "read", "{…}"],
  );
  assert.deepEqual(line({ type: "state_event", agent_id: "ai_1", kind: "send", sender: null, payload: { type: "status" } }), ["kernel", "ai_1", "status", ""]);
  assert.equal(line({ type: "state_event", agent_id: "memory", kind: "drain" }), null);
  assert.equal(line({ type: "state_snapshot", agents: [] }), null);
});
