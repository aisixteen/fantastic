import { test } from "node:test";
import assert from "node:assert/strict";
import { FakeEl, installFakeDom } from "./fake_dom.ts";
import { credentialsView } from "../src/bundles/credentials_view/credentials_view.ts";
import { storeView } from "../src/bundles/store_view/store_view.ts";
import { schedulerView } from "../src/bundles/scheduler_view/scheduler_view.ts";
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
    onLifecycle: (_src: string, fn: (s: "connected" | "disconnected") => void) => ((life = fn), () => void (life = null)),
  };
  let life: ((s: "connected" | "disconnected") => void) | null = null;
  const mount = new FakeEl("div");
  const ctx = { kernel, mount, selfId: "v", backend: "agent", close: () => {} } as unknown as ViewContext;
  return { ctx, mount, calls, fire: (p: Dict) => inbox?.(p), live: () => inbox !== null, up: (s: "connected" | "disconnected") => life?.(s) };
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
    p["type"] === "dump" ? { tables: { rows: [{ owner: "alice", row: { content: "SENTINEL-BODY" } }], fold_refs: [] } }
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

test("read_view_does_not_read_again_for_the_echo_of_its_own_reads", async () => {
  const r = rig((p) => (p["type"] === "dump" ? { tables: { rows: [], fold_refs: [] } } : { matches: [] }));
  await storeView.mount(r.ctx);
  const before = r.calls.length;
  r.fire({ type: "dump" }); // the kernel mirrors the view's own call back to its watcher
  await flush();
  assert.equal(r.calls.length, before, "an echo of its own read reads nothing");
  r.fire({ type: "appended", table: "rows", id: 1 });
  await flush();
  assert.equal(r.calls.length, before + 1, "an event of the agent reads it again");
});

/** Each read view against its own agent: copies of calls (its own reads, another client's
 *  writes) read nothing; each change event the agent publishes reads it again. */
const READ_VIEWS = [
  {
    name: "store",
    view: storeView,
    reply: (p: Dict): Dict => (p["type"] === "dump" ? { tables: { rows: [], fold_refs: [] } } : { matches: [] }),
    copies: [{ type: "dump" }, { type: "search", query: "x" }, { type: "append", rows: [{ text: "a" }] }, { type: "prune", ids: [1] }, { type: "inject", tables: {} }, { type: "read" }, { type: "put_bytes" }],
    events: [{ type: "appended", table: "rows", id: 1 }, { type: "pruned", n: 1 }, { type: "injected", rows: 2 }, { type: "bytes_put", ref: "p1" }],
  },
  {
    name: "scheduler",
    view: schedulerView,
    reply: (p: Dict): Dict => (p["type"] === "list" ? { schedules: [] } : { history: [] }),
    copies: [{ type: "list" }, { type: "history", limit: 20 }, { type: "schedule", target: "t", payload: { type: "reflect" }, interval_seconds: 5 }, { type: "tick_now", schedule_id: "sch_1" }, { type: "unschedule", schedule_id: "sch_1" }, { type: "pause" }],
    events: [{ type: "schedule_fired", schedule_id: "sch_1", result: {}, error: null }, { type: "scheduled", schedule_id: "sch_1" }, { type: "unscheduled", schedule_id: "sch_1" }, { type: "paused", schedule_id: "sch_1" }, { type: "resumed", schedule_id: "sch_1" }],
  },
  {
    name: "credentials",
    view: credentialsView,
    reply: (p: Dict): Dict => (p["type"] === "list" ? { refs: [] } : { callable_by: {} }),
    copies: [{ type: "list" }, { type: "reflect" }, { type: "set", ref: "a", fields: { k: "v" } }, { type: "forget", ref: "a" }, { type: "get", ref: "a" }],
    events: [{ type: "stored", ref: "a" }, { type: "forgotten", ref: "a" }],
  },
];

for (const c of READ_VIEWS) {
  test(`${c.name} view: a copy of a call reads nothing; a published change reads again`, async () => {
    const r = rig(c.reply);
    await c.view.mount(r.ctx);
    for (const copy of c.copies) {
      const before = r.calls.length;
      r.fire(copy);
      await flush();
      assert.equal(r.calls.length, before, `a copy read again: ${JSON.stringify(copy)}`);
    }
    for (const ev of c.events) {
      const before = r.calls.length;
      r.fire(ev);
      await flush();
      assert.ok(r.calls.length > before, `${String(ev.type)}: not read again`);
    }
  });

  test(`${c.name} view: its watch coming up (a late dial, a reconnect) reads it again`, async () => {
    const r = rig(c.reply);
    await c.view.mount(r.ctx);
    let before = r.calls.length;
    r.up("disconnected");
    await flush();
    assert.equal(r.calls.length, before, "a drop reads nothing");
    r.up("connected");
    await flush();
    assert.ok(r.calls.length > before, "the watch up: read again");
    before = r.calls.length;
    r.up("connected");
    await flush();
    assert.ok(r.calls.length > before, "a reconnect: read again");
  });
}

test("read view: only the newest of overlapping reads draws", async () => {
  let n = 0;
  const gates: Array<() => void> = [];
  const r = rig((p) => (p["type"] === "dump" ? { tables: { rows: Array.from({ length: n }, () => ({ owner: "alice", row: {} })), fold_refs: [] } } : {}));
  const slow = r.ctx.kernel as unknown as { send: (items: Array<{ target: string; payload: Dict }>) => Promise<unknown> };
  const fast = slow.send;
  await storeView.mount(r.ctx);
  // the first read hangs until released; the second answers at once with more rows
  slow.send = (items) => new Promise((res) => gates.push(() => void fast(items).then(res)));
  n = 1;
  r.fire({ type: "appended", table: "rows", id: 1 });
  slow.send = fast;
  n = 2;
  r.fire({ type: "appended", table: "rows", id: 2 });
  await flush();
  assert.match(r.mount.textContent, /alice {2}2 rows/);
  n = 1;
  gates.forEach((g) => g());
  await flush();
  await flush();
  assert.match(r.mount.textContent, /alice {2}2 rows/, "the stale read did not draw over the newer one");
});
