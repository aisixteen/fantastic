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
  shapeRows(rows: Row[], thoughts?: Map<number, { text: string; cut: number }>, texts?: Map<number, string>): Row[];
  historyTail(call: (p: Row) => Promise<Row | null>, keep?: number): Promise<{ rows: Row[]; bytes: number; lastId: number; earlier: number; tokens: number }>;
  stateOf(phase: string, reason: string, asks: number): string;
  inquiryAge(ts: unknown, nowMs: number): string;
  metaOf(sender: string, name: string): string;
  codeSpans(text: string): { text: string; code: boolean }[];
  tokensOf(rows: Row[]): number;
  fmtTokens(n: number): string;
  readHead(call: (p: Row) => Promise<Row | null>, size?: number): Promise<Row[]>;
  textsOf(rows: Row[]): Map<number, string>;
  keptThoughts(call: (p: Row) => Promise<Row | null>, size?: number): Promise<Map<number, { text: string; cut: number }>>;
  groupTitle(g: Row): { n: string; verbs: string };
}
const ctx = vm.createContext({});
vm.runInContext(`${src.slice(from, to)}\nthis.api = { parseFrom, shapeRows, historyTail, keptThoughts, groupTitle, stateOf, inquiryAge, metaOf, codeSpans, tokensOf, fmtTokens, readHead, textsOf };`, ctx);
const raw = (ctx as unknown as { api: Pure }).api;
// the vm's objects are of another realm: compare them as plain JSON
const plain = <T>(x: T): T => JSON.parse(JSON.stringify(x)) as T;
const P: Pure = {
  parseFrom: (t) => plain(raw.parseFrom(t)),
  shapeRows: (r, t, x) => plain(raw.shapeRows(r, t, x)),
  historyTail: async (c, k) => plain(await raw.historyTail(c, k)),
  keptThoughts: async (c, n) => new Map(Object.entries(plain(Object.fromEntries(await raw.keptThoughts(c, n)))).map(([k, v]) => [Number(k), v])),
  groupTitle: (g) => plain(raw.groupTitle(g)),
  stateOf: (a, b, c) => raw.stateOf(a, b, c),
  inquiryAge: (a, b) => raw.inquiryAge(a, b),
  metaOf: (a, b) => raw.metaOf(a, b),
  codeSpans: (x) => plain(raw.codeSpans(x)),
  tokensOf: (r) => raw.tokensOf(r),
  fmtTokens: (n) => raw.fmtTokens(n),
  readHead: async (c, n) => plain(await raw.readHead(c, n)),
  textsOf: (r) => new Map(Object.entries(plain(Object.fromEntries(raw.textsOf(r)))).map(([k, v]) => [Number(k), v])),
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
  assert.equal(t.earlier, 300, "the 300 older rows are counted, for the Earlier button");
  assert.deepEqual(await P.historyTail(async () => null), { rows: [], bytes: 0, lastId: 0, earlier: 0, tokens: 0 });
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

test("the agent's state: asks you beats working; a failure reason is failed; ok is done", () => {
  assert.equal(P.stateOf("thinking", "", 1), "asks");
  for (const ph of ["queued", "thinking", "streaming", "tool_calling"]) assert.equal(P.stateOf(ph, "", 0), "working");
  assert.equal(P.stateOf("idle", "ok", 0), "done");
  assert.equal(P.stateOf("idle", "", 0), "idle");
  for (const why of ["error", "dead_turn", "gave_up"]) assert.equal(P.stateOf("idle", why, 0), "failed", why);
  for (const why of ["awaiting", "nothing_to_await"]) assert.equal(P.stateOf("idle", why, 0), "idle", why);
});

test("the inquiry's age: minutes from its ts in seconds, just now under a minute", () => {
  const t0 = 1_700_000_000;
  assert.equal(P.inquiryAge(t0, (t0 + 20) * 1000), "just now");
  assert.equal(P.inquiryAge(t0, (t0 + 12 * 60 + 5) * 1000), "12 min");
  assert.equal(P.inquiryAge(t0, (t0 + 3 * 3600) * 1000), "3 h");
  assert.equal(P.inquiryAge(undefined, 1), "");
  assert.equal(P.inquiryAge(0, 1), "");
});

test("a message from another agent gets a `sender to <name>` meta line; the person's own has none", () => {
  assert.equal(P.metaOf("planner", "lead"), "planner to lead");
  assert.equal(P.metaOf("", "lead"), "");
});

test("`code` spans split out of a text, the text stays whole", () => {
  assert.deepEqual(P.codeSpans("run `ls -la` then `pwd` ok"), [
    { text: "run ", code: false }, { text: "ls -la", code: true }, { text: " then ", code: false }, { text: "pwd", code: true }, { text: " ok", code: false },
  ]);
  assert.deepEqual(P.codeSpans("no code, one ` tick"), [{ text: "no code, one ` tick", code: false }]);
  assert.deepEqual(P.codeSpans("`a\nb`"), [{ text: "`a\nb`", code: false }], "a span never crosses a line");
  assert.deepEqual(P.codeSpans(""), []);
});

test("tokens of a run: the usage the rows keep, summed; shown as N tok / Nk tok", () => {
  const rows: Row[] = [
    { id: 1, role: "user" },
    { id: 2, role: "assistant", attributes: { usage: { input: 1000, output: 200, cache_read: 300 } } },
    { id: 3, role: "assistant", attributes: { usage: { input: 10, output: 5, cache_write: 5 } } },
    { id: 4, role: "assistant", attributes: { model: "m" } },
  ];
  assert.equal(P.tokensOf(rows), 1520);
  assert.equal(P.tokensOf([]), 0);
  assert.deepEqual([P.fmtTokens(0), P.fmtTokens(999), P.fmtTokens(1520), P.fmtTokens(96_400), P.fmtTokens(250_000)], ["0 tok", "999 tok", "1.5k tok", "96.4k tok", "250k tok"]);
});

test("a message longer than the digest's 120 is shown whole from the loaded head; a short one keeps its head", () => {
  const long = "x".repeat(200);
  const rows: Row[] = [
    { id: 1, role: "user", head: "short", bytes: 5 },
    { id: 2, role: "assistant", head: long.slice(0, 120), bytes: 200 },
    { id: 3, role: "assistant", head: long.slice(0, 120), bytes: 200 },
  ];
  const texts = new Map([[1, "SHOULD NOT SHOW"], [2, long]]);
  const out = P.shapeRows(rows, undefined, texts);
  assert.deepEqual(out.map((x) => x["text"]), ["short", long, long.slice(0, 120)], "id 3 is not loaded: its cut head stays");
});

test("readHead pages the rows verb; textsOf keeps the string contents by id", async () => {
  const head = [{ id: 1, content: "a" }, { id: 2, content: { x: 1 } }, { id: 3, content: "c" }];
  const call = async (p: Row): Promise<Row | null> => {
    const at = p["after"] === undefined ? 0 : head.findIndex((r) => r.id === p["after"]) + 1;
    const page = head.slice(at, at + 2);
    return { rows: page, next: at + 2 < head.length ? page[page.length - 1]!.id : null };
  };
  const rows = await P.readHead(call, 2);
  assert.equal(rows.length, 3);
  assert.deepEqual([...P.textsOf(rows)], [[1, "a"], [3, "c"]]);
});

test("the tool-call row title stays `N tool calls` with its verbs, for the mock's row", () => {
  const out = P.shapeRows([
    { id: 1, role: "assistant", head: call("list") }, { id: 2, role: "user", head: reply("list") },
    { id: 3, role: "assistant", head: call("read") }, { id: 4, role: "user", head: reply("read") },
  ]);
  assert.deepEqual(P.groupTitle(out[0] as never), { n: "2 tool calls", verbs: "list, read" });
});

test("one row with several calls is several entries of ONE run; a cut-off call head still counts one", () => {
  const two = call("list") + "\n" + call("read");
  const out = P.shapeRows([
    { id: 1, role: "assistant", head: two },
    { id: 2, role: "user", head: reply("list") },
    { id: 3, role: "user", head: reply("read") },
    { id: 4, role: "assistant", head: `<call name="c"><target_id>files</target_id><type>wri` },
  ]);
  assert.deepEqual(out.map((x) => x["kind"]), ["tools"]);
  const g = out[0] as unknown as { calls: { verb: string; reply: string | null }[] };
  assert.deepEqual(g.calls.map((c) => c.verb), ["list", "read", ""]);
  assert.equal(g.calls[0]!.reply?.includes("list"), true);
  assert.equal(g.calls[1]!.reply?.includes("read"), true);
  assert.deepEqual(P.groupTitle(g as never), { n: "3 tool calls", verbs: "list, read, files" });
});
