import { test } from "node:test";
import assert from "node:assert/strict";
import { clocksReadNothing, file, ownCopiesReadNothing, run } from "./view_harness.ts";
import type { Obj } from "./view_harness.ts";

test("the AI chat view follows every call of its agent, whoever made it", async () => {
  let current: Obj | null = null;
  const h = run(file("src/lib/rust/crates/bundles/fantastic-ai-core/src/chat_view.html"), { agent: "chat1" }, (target, p) => {
    if (target === "user") return { inquiries: [] };
    if (p["type"] === "reflect") return { id: "chat1", display_name: "Chat", provider_id: "prov" };
    if (p["type"] === "history") return { rows: [] };
    if (p["type"] === "status") return { current };
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

  // every phase that ends a call (or comes outside one) ends it like `done`: Stop goes, the rows are read
  for (const end of ["failed", "stalled", "paused", "idle"]) {
    current = { phase: "thinking", is_mine: false };
    await h.publish("chat1", { type: "status", phase: "thinking", client_id: "cli", source: "chat1", detail: { send_id: "s2" } });
    assert.equal(stop["hidden"], false, `${end}: the call runs`);
    current = null;
    const at = h.sends.length;
    await h.publish("chat1", { type: "status", phase: end, client_id: "cli", source: "chat1", detail: { send_id: "s2" } });
    const read = h.since(at).map((s) => String(s.p["type"]));
    assert.ok(read.includes("history"), `${end}: the rows are read again: ${read}`);
    assert.equal(stop["hidden"], true, `${end}: nothing runs: no Stop`);
  }

  // a question it asked the person: read on the event, not at a pass's end
  mark = h.sends.length;
  await h.publish("chat1", { type: "inquiry_opened", re: 9, asker: "chat1", holder: "user" });
  assert.deepEqual(h.since(mark).map((s) => `${s.target} ${String(s.p["type"])}`), ["user inquiries"]);
});

test("the AI chat view never reads again on a copy of a call — its own reads, another view's status read", async () => {
  const h = run(file("src/lib/rust/crates/bundles/fantastic-ai-core/src/chat_view.html"), { agent: "chat1" }, (target, p) => {
    if (target === "user") return { inquiries: [] };
    if (p["type"] === "reflect") return { id: "chat1", display_name: "Chat", provider_id: "prov" };
    if (p["type"] === "history") return { rows: [] };
    if (p["type"] === "status") return { current: null };
    return {};
  });
  await h.settle();
  await ownCopiesReadNothing(h);
  const stop = h.byId.get("stop")!;
  const state = h.byId.get("state")!;
  const shown = state["textContent"];
  assert.equal(stop["hidden"], true);
  // two views on one chat: the other one's status read reaches this view as a copy
  for (const copy of [{ type: "status", client_id: "_ws_other" }, { type: "status" }, { type: "history", client_id: "_ws_other" }, { type: "reflect" }]) {
    const mark = h.sends.length;
    await h.publish("chat1", copy);
    assert.deepEqual(h.since(mark).map((s) => String(s.p["type"])), [], `a copy read again: ${JSON.stringify(copy)}`);
    assert.equal(stop["hidden"], true, `a copy is no running call: ${JSON.stringify(copy)}`);
    assert.equal(state["textContent"], shown, `a copy sets no phase: ${JSON.stringify(copy)}`);
  }
});
