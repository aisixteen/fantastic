import { test } from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import { file, run } from "./view_harness.ts";
import type { Harness, Obj } from "./view_harness.ts";

// THE LINE SUMMARY IN THE VIEWS (#709): a chat's latest line (`line_summary {text, ts}`) shows
// as the tree row's state line and in the chat head while it is recent; otherwise the call state
// shows as without it. Each view reads the line once on open (its agent's `reflect`) and then
// follows the events: the chat's own `line_summary_changed {text, ts}`, and the sidebar agent's
// `line_summary_changed {agent, text, ts}`.

const SIDEBAR = "src/tui/src/kern/sidebar_view.html";
const CHAT = "src/lib/rust/crates/bundles/fantastic-ai-core/src/chat_view.html";
const now = (): number => Date.now() / 1000;
const plain = <T>(x: T): T => JSON.parse(JSON.stringify(x)) as T;

interface Pure {
  recentLine(line: unknown, now: number): string;
  withLine(lines: Obj, agent: string, text: string, ts: number): Obj;
  heardOf(ev: unknown): string;
  treeRows(root: Obj, chats: Record<string, boolean>, folded: Obj, status: Obj, asks: Obj, files?: Obj, opts?: Obj): Obj[];
}
const pure = ((): Pure => {
  const src = file(SIDEBAR);
  const from = src.indexOf("// pure\n");
  const to = src.indexOf("// end pure");
  assert.ok(from >= 0 && to > from, "the view marks its pure block");
  const ctx = vm.createContext({});
  vm.runInContext(`${src.slice(from, to)}\nthis.api = { recentLine, withLine, heardOf, treeRows };`, ctx);
  return (ctx as unknown as { api: Pure }).api;
})();

const tree = { id: "kernel", children: [{ id: "chat1", handler_module: "ai_chat.tools", children: [] }, { id: "tool1", children: [] }] };

test("a line is shown while recent, then the row falls back to its call state", () => {
  const t = 1000;
  assert.equal(pure.recentLine({ text: " reading\n the spec ", ts: t }, t + 5), "reading the spec");
  assert.equal(pure.recentLine({ text: "reading", ts: t }, t + 600), "reading", "ten minutes is still recent");
  assert.equal(pure.recentLine({ text: "reading", ts: t }, t + 601), "", "older: no line");
  assert.equal(pure.recentLine(undefined, t), "");
  assert.equal(pure.recentLine({ text: "x" }, t), "", "a line without its time is not shown");
  const rows = plain(pure.treeRows(tree, { chat1: true }, {}, { chat1: "thinking" }, {}, undefined, { lines: { chat1: { text: "reading", ts: t }, tool1: { text: "no", ts: t } }, now: t + 1 }));
  assert.deepEqual(rows.map((r) => [r["id"], r["line"]]), [["kernel", ""], ["chat1", "reading"], ["tool1", ""]], "only a chat's row carries a line");
  const later = plain(pure.treeRows(tree, { chat1: true }, {}, { chat1: "thinking" }, {}, undefined, { lines: { chat1: { text: "reading", ts: t } }, now: t + 3600 }));
  assert.equal(later[1]!["line"], "");
});

test("the sidebar's event names a line; each chat keeps its latest", () => {
  assert.equal(pure.heardOf({ type: "line_summary_changed", agent: "chat1", text: "reading", ts: 3 }), "line");
  assert.equal(pure.heardOf({ type: "line_summary_changed", agent: "chat1", text: "reading" }), "", "no time: no line");
  assert.equal(pure.heardOf({ type: "line_summary_changed", text: "reading", ts: 3 }), "", "no agent: not the sidebar's");
  const lines = plain(pure.withLine({ chat1: { text: "a", ts: 1 }, chat2: { text: "b", ts: 2 } }, "chat1", "c", 3));
  assert.deepEqual(lines, { chat2: { text: "b", ts: 2 }, chat1: { text: "c", ts: 3 } });
});

function sidebar(lines: Obj): Harness {
  return run(file(SIDEBAR), { agent: "sidebar", cell: "view_sidebar" }, (target, p) => {
    if (target === "kernel" && p["handler_modules"]) return { tree: ["chat1"] };
    if (target === "kernel") return { id: "kernel", guid: "g1", peers: [], tree };
    if (target === "user") return { inquiries: [] };
    if (target === "sidebar") return { id: "sidebar", phases: { chat1: "thinking" }, line_summaries: lines, open: 0, files_root: "" };
    return {};
  }, { textDom: true });
}

const treeText = (h: Harness): string => h.document!.getElementById("tree").textContent;

test("the tree row's state line shows the chat's recent line, read once and then followed", async () => {
  const h = sidebar({ chat1: { text: "reading the spec", ts: now() } });
  await h.settle();
  assert.ok(treeText(h).includes("reading the spec"), treeText(h));
  assert.ok(!treeText(h).includes("thinking"), "the line takes the state line's place");
  const mark = h.sends.length;
  await h.publish("sidebar", { type: "line_summary_changed", agent: "chat1", text: "writing the tests", ts: now() });
  assert.ok(treeText(h).includes("writing the tests") && !treeText(h).includes("reading the spec"), treeText(h));
  assert.deepEqual(h.since(mark), [], "the event carries the line: nothing is read");
  // an old line: the call state, as without one
  await h.publish("sidebar", { type: "line_summary_changed", agent: "chat1", text: "long ago", ts: now() - 3600 });
  assert.ok(treeText(h).includes("thinking") && !treeText(h).includes("long ago"), treeText(h));
});

test("with no line the tree row shows the call state as before", async () => {
  const h = sidebar({});
  await h.settle();
  assert.ok(treeText(h).includes("thinking"), treeText(h));
});

// what the chat's `status` answers: a call of another client runs while `running` says so
const running = { phase: "" };
function chat(line: unknown): Harness {
  running.phase = "";
  return run(file(CHAT), { agent: "chat1" }, (target, p) => {
    if (target === "user") return { inquiries: [] };
    if (p["type"] === "reflect") return { id: "chat1", display_name: "Chat", provider_id: "prov", line_summary: line };
    if (p["type"] === "history") return { rows: [] };
    if (p["type"] === "status") return { current: running.phase ? { phase: running.phase, is_mine: false } : null, others_pending: 0, mine_pending: [] };
    return {};
  });
}

const head = (h: Harness): unknown => h.byId.get("state")!["textContent"];

test("the chat head shows the chat's recent line, read once on open and then followed", async () => {
  const h = chat({ text: "reading the spec", ts: now() });
  await h.settle();
  assert.equal(head(h), "reading the spec");
  assert.equal(h.byId.get("state")!["hidden"], false);
  const mark = h.sends.length;
  await h.publish("chat1", { type: "line_summary_changed", source: "chat1", text: "writing the tests", ts: now() });
  assert.equal(head(h), "writing the tests");
  assert.deepEqual(h.since(mark), [], "the event carries the line: nothing is read");
  // a call runs: the recent line still shows
  running.phase = "thinking";
  await h.publish("chat1", { type: "status", phase: "thinking", client_id: "cli", source: "chat1", detail: { send_id: "s1" } });
  assert.equal(head(h), "writing the tests");
  // another agent's line is not this chat's
  await h.publish("chat1", { type: "line_summary_changed", source: "chat2", text: "not mine", ts: now() });
  assert.equal(head(h), "writing the tests");
  // an old line: the call state, as without one
  await h.publish("chat1", { type: "line_summary_changed", source: "chat1", text: "long ago", ts: now() - 3600 });
  assert.equal(head(h), "thinking");
});

test("with no line the chat head shows the call state as before", async () => {
  const h = chat(null);
  await h.settle();
  assert.equal(h.byId.get("state")!["hidden"], true, "at rest: no pill");
  running.phase = "thinking";
  await h.publish("chat1", { type: "status", phase: "thinking", client_id: "cli", source: "chat1", detail: { send_id: "s1" } });
  assert.equal(head(h), "thinking");
});
