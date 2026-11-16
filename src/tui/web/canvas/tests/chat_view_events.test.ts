import { test } from "node:test";
import assert from "node:assert/strict";
import { clocksReadNothing, file, run } from "./view_harness.ts";
import type { Obj } from "./view_harness.ts";

test("the AI chat view follows every call of its agent, whoever made it", async () => {
  let current: Obj | null = null;
  const h = run(file("src/lib/rust/crates/bundles/fantastic-ai-core/src/chat_view.html"), { agent: "chat1" }, (target, p) => {
    if (target === "user") return { inquiries: [] };
    if (p["type"] === "reflect") return { id: "chat1", display_name: "Chat", provider_id: "prov" };
    if (p["type"] === "history") return { rows: [] };
    if (p["type"] === "status") return { current, others_pending: 0, mine_pending: [] };
    return {};
  });
  await h.settle();
  assert.ok(h.watches.has("chat1"), "the view watches its agent");
  assert.deepEqual(h.intervals.map((t) => t.ms), [30000], "one clock, for the question's age");
  await clocksReadNothing(h);
  const stop = h.byId.get("stop")!;
  assert.equal(stop["hidden"], true, "nothing runs: no Stop");

  // the terminal sends: the call is not this view's, it still shows Stop while it runs
  current = { phase: "thinking", is_mine: false };
  await h.publish("chat1", { type: "status", phase: "thinking", client_id: "cli", source: "chat1", detail: { send_id: "s1" } });
  assert.equal(stop["hidden"], false, "another client's call runs: Stop shows");
  assert.equal(h.byId.get("state")!["textContent"] !== "", true, "the phase shows");

  // it ends: the rows it wrote are drawn from the history
  current = null;
  let mark = h.sends.length;
  await h.publish("chat1", { type: "status", phase: "done", client_id: "cli", source: "chat1", detail: { send_id: "s1" } });
  const after = h.since(mark).map((s) => String(s.p["type"]));
  assert.ok(after.includes("status"), `the state is read again: ${after}`);
  assert.ok(after.includes("history"), `the rows are read again: ${after}`);
  assert.equal(stop["hidden"], true, "nothing runs: no Stop");

  // a question it asked the person: read on the event, not at a turn's end
  mark = h.sends.length;
  await h.publish("chat1", { type: "inquiry_opened", re: 9, asker: "chat1", holder: "user" });
  assert.deepEqual(h.since(mark).map((s) => `${s.target} ${String(s.p["type"])}`), ["user inquiries"]);
});
