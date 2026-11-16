import { test } from "node:test";
import assert from "node:assert/strict";
import { clocksReadNothing, file, run } from "./view_harness.ts";

test("the web agent's request view re-reads on `request_served`, never on a timer", async () => {
  const h = run(file("src/lib/rust/crates/bundles/fantastic-web/src/request_view.html"), { agent: "web1" }, () => ({ request_allow: [], recent_requests: [] }));
  await h.settle();
  assert.ok(h.watches.has("web1"), "the view watches its agent");
  assert.deepEqual(h.intervals, [], "no timer");
  let mark = h.sends.length;
  await h.publish("web1", { type: "request_served", method: "GET", host: "h", status: 200 });
  assert.deepEqual(h.since(mark).map((s) => [s.target, s.p["type"]]), [["web1", "reflect"]]);
  mark = h.sends.length;
  await h.publish("web1", { type: "socket_open" });
  assert.deepEqual(h.since(mark), [], "an event that changes nothing shown reads nothing");
});
