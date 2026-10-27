import { test } from "node:test";
import assert from "node:assert/strict";
import { FakeEl, installFakeDom } from "./fake_dom.ts";
import { credentialsView } from "../src/bundles/credentials_view/credentials_view.ts";
import { storeView } from "../src/bundles/store_view/store_view.ts";
import { tasksView } from "../src/bundles/tasks_view/tasks_view.ts";
import type { ViewContext } from "../src/bundles/view.ts";

installFakeDom();

type Dict = Record<string, unknown>;
const flush = (): Promise<void> => new Promise((r) => setTimeout(r, 0));

/** A page kernel stand-in: answers calls from `reply`, records them, and lets the test fire an event. */
function rig(reply: (p: Dict) => Dict) {
  const calls: Dict[] = [];
  let inbox: ((p: Dict) => void) | null = null;
  const kernel = {
    send: async (items: Array<{ target: string; payload: Dict }>) => items.map((i) => (calls.push(i.payload), { result: reply(i.payload) })),
    watch: () => {},
    unwatch: () => {},
    onInbox: (_src: string, fn: (p: Dict) => void) => ((inbox = fn), () => void (inbox = null)),
  };
  const mount = new FakeEl("div");
  const ctx = { kernel, mount, selfId: "v", backend: "agent", close: () => {} } as unknown as ViewContext;
  return { ctx, mount, calls, fire: (p: Dict) => inbox?.(p), live: () => inbox !== null };
}

test("credentials_view_dom_has_no_secret_text", async () => {
  const r = rig((p) =>
    p["type"] === "list"
      ? { refs: [{ ref: "openai", host: "api.openai.com", fields: { api_key: "sk-SECRET-123" }, value: "sk-SECRET-123", len: 13, masked: "****" }] }
      : { callable_by: { $self: "*", "openai-key": ["set", "get", "forget"] } },
  );
  await credentialsView.mount(r.ctx);
  const t = r.mount.textContent;
  assert.match(t, /openai/);
  assert.match(t, /fields: api_key/);
  assert.match(t, /openai-key: set, get, forget/);
  for (const bad of ["SECRET", "sk-", "****"]) assert.ok(!t.includes(bad), bad);
  assert.ok(r.calls.every((c) => c["type"] === "list" || c["type"] === "reflect"), "never `get`");
});

test("store_view_search_sends_search_and_shows_hits", async () => {
  const r = rig((p) =>
    p["type"] === "dump" ? { rows: [{ owner: "alice", row: { content: "SENTINEL-BODY" } }], fold_refs: [] }
    : { matches: [{ id: 7, line: 2, text: "my own line" }], total: 1, truncated: false },
  );
  await storeView.mount(r.ctx);
  assert.match(r.mount.textContent, /alice {2}1 rows/);
  assert.ok(!r.mount.textContent.includes("SENTINEL-BODY"));
  const input = r.mount.querySelector("input")!;
  input.value = "own";
  input.dispatch("keydown", { key: "Enter" } as never);
  await flush();
  assert.deepEqual(r.calls.filter((c) => c["type"] === "search"), [{ type: "search", query: "own" }]);
  assert.match(r.mount.textContent, /7:2 {2}my own line/);
});

test("tasks_view_refreshes_on_watch_event", async () => {
  let tasks: Dict[] = [{ id: "a", status: "waiting", source: "gh:o/r#1", holder: null }];
  const r = rig(() => ({ tasks }));
  const h = await tasksView.mount(r.ctx);
  assert.match(r.mount.textContent, /a {2}waiting/);
  assert.ok(r.live(), "watching the agent");
  tasks = [{ id: "a", status: "running", source: "gh:o/r#1", holder: "w1" }];
  r.fire({ type: "updated" });
  await flush();
  assert.match(r.mount.textContent, /a {2}running {2}w1/);
  assert.ok(r.calls.every((c) => c["type"] === "list"));
  h.unmount();
  assert.ok(!r.live(), "unmount stops the watch");
});

test("tasks_view_click_expands_the_history", async () => {
  const r = rig(() => ({ tasks: [{ id: "a", status: "running", source: "s", holder: "w2", history: [{ from: "w1", to: "w2", by: "lead" }] }] }));
  await tasksView.mount(r.ctx);
  assert.ok(!r.mount.textContent.includes(" by "));
  r.mount.querySelector("[data-id='a']")!.dispatch("click");
  assert.match(r.mount.textContent, /w1 -> w2 by lead/);
});
