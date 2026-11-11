import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

// THE CHAT VIEW'S PURE DATA SHAPING (history rows -> what the transcript shows). The view
// (`fantastic-ai-core/src/chat_view.html`) keeps it in one `// pure` script block: it is read
// here and run in a bare vm context, no DOM.

const HTML = fileURLToPath(new URL("../../../../../src/lib/rust/crates/bundles/fantastic-ai-core/src/chat_view.html", import.meta.url));
const src = readFileSync(HTML, "utf8");
const from = src.indexOf("// pure\n");
const to = src.indexOf("// end pure");
assert.ok(from >= 0 && to > from, "the view marks its pure block");

type Row = Record<string, unknown>;
interface Pure {
  parseFrom(t: string): { sender: string; text: string };
  shapeRows(rows: Row[], thoughts?: Map<number, { text: string; cut: number }>): Row[];
  historyTail(call: (p: Row) => Promise<Row | null>, keep?: number): Promise<{ rows: Row[]; bytes: number; lastId: number }>;
  keptThoughts(call: (p: Row) => Promise<Row | null>, size?: number): Promise<Map<number, { text: string; cut: number }>>;
  groupTitle(g: Row): { n: string; verbs: string };
}
const ctx = vm.createContext({});
vm.runInContext(`${src.slice(from, to)}\nthis.api = { parseFrom, shapeRows, historyTail, keptThoughts, groupTitle };`, ctx);
const raw = (ctx as unknown as { api: Pure }).api;
// the vm's objects are of another realm: compare them as plain JSON
const plain = <T>(x: T): T => JSON.parse(JSON.stringify(x)) as T;
const P: Pure = {
  parseFrom: (t) => plain(raw.parseFrom(t)),
  shapeRows: (r, t) => plain(raw.shapeRows(r, t)),
  historyTail: async (c, k) => plain(await raw.historyTail(c, k)),
  keptThoughts: async (c, n) => new Map(Object.entries(plain(Object.fromEntries(await raw.keptThoughts(c, n)))).map(([k, v]) => [Number(k), v])),
  groupTitle: (g) => plain(raw.groupTitle(g)),
};

const call = (verb: string, target = "files"): string => `<call name="c"><target_id>${target}</target_id><type>${verb}</type></call>`;
const reply = (verb: string, target = "files"): string => `[reply to 7 · ${target} ${verb}]\n{"ok":true}`;

test("consecutive tool rows fold into ONE group row: N tool calls · verbs", () => {
  const rows: Row[] = [
    { id: 1, role: "user", head: "hello" },
    { id: 2, role: "assistant", head: "on it" },
    { id: 3, role: "assistant", head: call("list") },
    { id: 4, role: "user", head: reply("list") },
    { id: 5, role: "assistant", head: call("read") },
    { id: 6, role: "user", head: reply("read") },
    { id: 7, role: "assistant", head: call("list") },
    { id: 8, role: "assistant", head: "done" },
  ];
  const out = P.shapeRows(rows);
  assert.deepEqual(out.map((x) => x["kind"]), ["msg", "msg", "tools", "msg"]);
  const g = out[2] as unknown as { calls: { verb: string; target: string; args: string; reply: string | null }[] };
  assert.equal(g.calls.length, 3);
  assert.equal(g.calls[0]!.reply?.startsWith("[reply to 7"), true, "a reply is filed under its call");
  assert.equal(g.calls[2]!.reply, null, "a call with no reply yet");
  assert.deepEqual(P.groupTitle(g as never), { n: "3 tool calls", verbs: "list, read" });
});

test("a tool group split by a message is two groups", () => {
  const out = P.shapeRows([
    { id: 1, role: "assistant", head: call("list") },
    { id: 2, role: "user", head: reply("list") },
    { id: 3, role: "assistant", head: "text between" },
    { id: 4, role: "assistant", head: call("read") },
  ]);
  assert.deepEqual(out.map((x) => x["kind"]), ["tools", "msg", "tools"]);
  assert.deepEqual(P.groupTitle(out[2] as never), { n: "1 tool call", verbs: "read" });
});

test("a message starting [from <sender> …] shows the sender as its meta line and the rest as text", () => {
  assert.deepEqual(P.parseFrom("[from planner · msg 4]\nplease check"), { sender: "planner", text: "please check" });
  assert.deepEqual(P.parseFrom("plain words"), { sender: "", text: "plain words" });
  const out = P.shapeRows([{ id: 1, role: "user", head: "[from runner]\nhi there" }]);
  assert.deepEqual([out[0]!["sender"], out[0]!["text"], out[0]!["role"]], ["runner", "hi there", "user"]);
});

test("a thinking row folds: the thought comes before its answer", () => {
  const thoughts = new Map([[2, { text: "a deep thought", cut: 3 }]]);
  const out = P.shapeRows([{ id: 1, role: "user", head: "q" }, { id: 2, role: "assistant", head: "answer", has_thinking: true }], thoughts);
  assert.deepEqual(out.map((x) => x["kind"]), ["msg", "think", "msg"]);
  assert.deepEqual([out[1]!["text"], out[1]!["cut"]], ["a deep thought", 3]);
});

test("inline <think> and complete <call> blocks never reach the bubble", () => {
  const out = P.shapeRows([{ id: 1, role: "assistant", head: "<think>hm</think>The answer. " + call("list") }]);
  assert.equal(out[0]!["text"], "The answer.");
});

test("embed rows make name cards; an unembed takes the card out; reports repeat as one ×N row", () => {
  const out = P.shapeRows([
    { id: 1, kind: "embed", head: "panel_a" },
    { id: 2, kind: "embed", head: "panel_b" },
    { id: 3, kind: "unembed", head: "panel_a" },
    { id: 4, role: "user", head: "[malformed call — nothing ran] x" },
    { id: 5, role: "user", head: "[malformed call — nothing ran] x" },
  ]);
  assert.deepEqual(out.map((x) => x["kind"]), ["card", "report"]);
  assert.equal(out[0]!["agent"], "panel_b");
  assert.match(String(out[1]!["text"]), /×2$/);
});

test("historyTail keeps the newest 300 rows over the pages", async () => {
  const pages: Row[] = [
    { rows: Array.from({ length: 200 }, (_, i) => ({ id: i + 1, bytes: 10 })), next: 200 },
    { rows: Array.from({ length: 200 }, (_, i) => ({ id: i + 201, bytes: 10 })), next: 400 },
    { rows: Array.from({ length: 200 }, (_, i) => ({ id: i + 401, bytes: 10 })), next: null },
  ];
  let k = 0;
  const t = await P.historyTail(async () => pages[k++] ?? null, 300);
  assert.equal(t.rows.length, 300);
  assert.equal(t.rows[0]!["id"], 301);
  assert.equal(t.rows[299]!["id"], 600);
  assert.equal(t.lastId, 600);
  assert.equal(t.bytes, 6000);
  assert.deepEqual(await P.historyTail(async () => null), { rows: [], bytes: 0, lastId: 0 });
});

test("keptThoughts pages the rows verb from the head and stops on a null answer", async () => {
  const head = [{ id: 101 }, { id: 160, thinking: "deep", thinking_cut: 3 }, { id: 161 }, { id: 230, thinking: "other" }];
  const call = async (p: Row): Promise<Row | null> => {
    let rows = head;
    if (p["after"] !== undefined) {
      const at = head.findIndex((r) => r.id === p["after"]);
      rows = at < 0 ? [] : head.slice(at + 1);
    }
    const limit = p["limit"] as number;
    const page = rows.slice(0, limit);
    return { rows: page, next: rows.length > limit ? page[page.length - 1]!.id : null };
  };
  const got = await P.keptThoughts(call, 2);
  assert.deepEqual([...got.keys()], [160, 230]);
  assert.deepEqual(got.get(160), { text: "deep", cut: 3 });
  assert.equal((await P.keptThoughts(async () => null)).size, 0);
});
