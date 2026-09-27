// THE TURN: one `send` is one provider pass; what it called is routed after the
// lock is released and comes back as a NEW turn. Mirrors python ai_core tests
// `test_done_and_nudge.py` (DONE ends a chain, silence is nudged, `await` waits
// or is refused), `test_client_id_contract.py` (the safe alphabet; a send killed
// while queued leaves no phantom), `test_row_ids.py` (every row has an address)
// and `test_think_side_calls.py` (side calls strip reasoning) — plus the cancel
// close-out a UI drives through `ps` → `cancel` (there is no `interrupt` verb).

import { test } from "node:test";
import assert from "node:assert/strict";
import type { Payload } from "../src/kernel/json.ts";
import type { ProjectionCtx } from "../src/bundles/ai/context.ts";
import { DEFAULT_CLIENT_ID, rowIdsForIncoming, safeClient } from "../src/bundles/ai/turn.ts";
import { REPLY_TAG, PID_TAG, replyTag } from "../src/bundles/ai/replies.ts";
import { aiMem, loadHistory } from "../src/bundles/ai/memory.ts";
import { makeSummarizer } from "../src/bundles/ai/seam.ts";
import type { Provider } from "../src/bundles/ai/seam.ts";
import { compose } from "../src/bundles/ai/verbs.ts";
import { summarizeSpan } from "../src/bundles/ai/strategies/fold.ts";
import { renderCall } from "../src/bundles/ai/tool_parse.ts";
import { DONE, Mind, backendWorld, settle, until, World } from "./ai_world.ts";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

function reflectKernel(): string {
  return renderCall("send", { target_id: "kernel", payload: { type: "reflect" }, reply_to: "self" });
}

async function send(k: ReturnType<typeof backendWorld>, text: string, extra: Payload = {}): Promise<Payload> {
  return (await k.send([{ target: "ai", payload: { type: "send", text, ...extra } }]))[0]!.result as Payload;
}

// ── the end-of-chain contract ────────────────────────────────────────────────

test("silent chain turns are nudged until a call or done", async () => {
  const mind = new Mind([
    reflectKernel(), // turn 1: a real call → chain
    "thinking out loud, no envelope", // turn 2: SILENT → nudged
    "still just prose", // turn 3: STILL silent → nudged again
    "ok, finishing", // turn 4: silent → nudged again
    DONE, // turn 5: done → the chain ends
  ]);
  const k = backendWorld(mind);
  await send(k, "go");
  await settle(k);
  assert.equal(mind.calls, 5, JSON.stringify(mind.incoming));
  for (const i of [2, 3, 4]) assert.ok(mind.incoming[i]!.startsWith("[no call was dispatched]"), mind.incoming[i]);
  await sleep(50);
  assert.equal(mind.calls, 5, "done ended it; nothing reminds after done");
});

test("a done call ends the chain without a nudge", async () => {
  const mind = new Mind([reflectKernel(), "finished. " + DONE]);
  const k = backendWorld(mind);
  await send(k, "go");
  await settle(k);
  assert.equal(mind.calls, 2, JSON.stringify(mind.incoming));
  assert.ok(!mind.incoming.some((m) => m.startsWith("[no call was dispatched]")));
});

test("a first turn with no call is nudged too", async () => {
  const mind = new Mind(["just an answer, no calls", DONE]);
  const k = backendWorld(mind);
  await send(k, "hi");
  await settle(k);
  assert.equal(mind.calls, 2, JSON.stringify(mind.incoming));
  assert.ok(mind.incoming[1]!.includes("[no call was dispatched]"));
});

test("await with nothing in flight is refused and told what to do", async () => {
  const mind = new Mind(['waiting <call name="await"></call>', DONE]);
  const k = backendWorld(mind);
  await send(k, "hi");
  await settle(k);
  assert.equal(mind.calls, 2, JSON.stringify(mind.incoming));
  assert.ok(mind.incoming[1]!.includes("[await refused]"));
  assert.ok(mind.incoming[1]!.includes("nothing of yours is in flight"));
});

test("await with a message queued stops quietly and the queue wakes it", async () => {
  const mind = new Mind(['waiting <call name="await"></call>', DONE], 100);
  const k = backendWorld(mind);
  const first = send(k, "one");
  await sleep(20);
  const second = send(k, "two");
  await Promise.all([first, second]);
  await settle(k);
  assert.equal(mind.calls, 2, JSON.stringify(mind.incoming));
  assert.ok(!mind.incoming.some((m) => m.includes("[await refused]")));
  assert.ok(!mind.incoming.some((m) => m.includes("[no call was dispatched]")));
  assert.ok(mind.incoming[mind.incoming.length - 1]!.endsWith("two"));
});

test("a turn's calls come back as ONE new turn, linked to the turn that made them", async () => {
  const two =
    renderCall("send", { target_id: "tool", title: "first", payload: { type: "go", size: 3 } }) +
    renderCall("send", { target_id: "tool", payload: { type: "go", size: 4 } });
  const mind = new Mind([two, DONE]);
  const k = backendWorld(mind);
  const out = await send(k, "call the tool twice");
  assert.equal(out["client_id"], "cli");
  assert.equal(typeof out["message_id"], "number");
  await settle(k);
  assert.equal(mind.calls, 2);
  const delivered = mind.incoming[1]!;
  const blocks = delivered.split("\n\n");
  assert.equal(blocks.length, 2);
  assert.ok(blocks[0]!.startsWith("[reply to "), blocks[0]);
  assert.ok(blocks[0]!.includes(` · tool go · "first" · ↑${String(out["message_id"])} · 1 of 2 in batch ${String(out["message_id"])}]`), blocks[0]);
  assert.ok(blocks[1]!.includes("· 2 of 2 in batch"), blocks[1]);
  assert.ok(blocks[0]!.includes("<text>ttt</text>"), "the reply body is the tag notation, never JSON");
  const rows = await loadHistory("ai", k);
  const reply = rows.find((r) => String(r["content"]).startsWith("[reply to "))!;
  assert.equal(reply["parent"], out["message_id"], "the delivered turn names its parent from the ↑ on its tag");
});

test("a message from another agent opens with the header the kernel vouches for", async () => {
  const mind = new Mind([DONE]);
  const k = backendWorld(mind);
  k.updateMeta("tool", { can_call: { ai: "*" } });
  k.updateMeta("ai", { callable_by: { tool: "*" } });
  await k.scopedFor("tool", null).send([{ target: "ai", payload: { type: "send", text: "hello", to: ["ai", "x"], msg: 7, re: 3 } }]);
  await settle(k);
  assert.equal(mind.incoming[0], "[from tool · to ai, x · msg 7 · re 3]\nhello");
});

// ── cancel is `ps` → `cancel` on the kernel (no `interrupt` verb) ────────────

test("a turn cancelled through ps → cancel closes out for its client", async () => {
  let release!: () => void;
  const gate = new Promise<void>((r) => (release = r));
  const blocked: Provider = {
    async *chat(messages) {
      if (messages[0]?.role !== "system") return;
      await gate;
      yield DONE;
    },
  };
  const k = backendWorld(new Mind());
  aiMem(k).providers.set("ai", blocked);
  const events: Payload[] = [];
  k.onInbox("ai", (e) => {
    if (e["type"] === "status") events.push(e);
  });
  const running = k.send([{ target: "ai", payload: { type: "send", text: "long job", client_id: "tab1" } }]);
  await until(() => aiMem(k).current.has("ai"));
  const ps = (await k.send([{ target: "kernel", payload: { type: "ps" } }]))[0]!.result as Payload;
  const row = (ps["tasks"] as Payload[]).find((t) => t["agent"] === "ai" && t["verb"] === "send")!;
  assert.ok(row, "the in-flight send IS a kernel task");
  const c = (await k.send([{ target: "kernel", payload: { type: "cancel", task_id: row["id"] } }]))[0]!.result as Payload;
  assert.equal(c["cancelled"], true);
  const [out] = await running;
  assert.equal(out!.status, "cancelled");
  assert.equal(aiMem(k).current.has("ai"), false, "no phantom generation");
  const done = events.find((e) => e["phase"] === "done")!;
  assert.equal((done["detail"] as Payload)["reason"], "cancelled");
  release();
  await settle(k);
  assert.equal((await loadHistory("ai", k)).length, 0, "nothing the cancelled pass streamed is recorded");
});

// ── the client_id contract ───────────────────────────────────────────────────

test("the safe alphabet survives verbatim", () => {
  for (const cid of ["cli", "tab1", "cid__turn7", "a.b-c_d", "UPPER.lower-123_", "0", "...", "---", "___"]) {
    assert.equal(safeClient(cid), cid, cid);
  }
});

test("everything else becomes underscore, silently", () => {
  const cases: [string, string][] = [["cid#turn", "cid_turn"], ["a b", "a_b"], ["a/b", "a_b"], ["a\\b", "a_b"], ["a:b", "a_b"], ["a@b!c", "a_b_c"]];
  for (const [raw, want] of cases) assert.equal(safeClient(raw), want, raw);
});

test("unicode alphanumerics survive (the alphabet is unicode alnum)", () => {
  assert.equal(safeClient("тест"), "тест");
  assert.equal(safeClient("日本語7"), "日本語7");
  assert.equal(safeClient("naïve"), "naïve");
});

test("empty and whitespace fall back to the default", () => {
  assert.equal(safeClient(""), DEFAULT_CLIENT_ID);
  assert.equal(safeClient("   "), DEFAULT_CLIENT_ID);
  assert.equal(safeClient(null), DEFAULT_CLIENT_ID);
});

test("two distinct raw ids can collide after sanitization", () => {
  assert.equal(safeClient("a#b"), "a_b");
  assert.equal(safeClient("a/b"), "a_b");
});

test("a send killed while QUEUED leaves no phantom queue entry", async () => {
  let release!: () => void;
  const gate = new Promise<void>((r) => (release = r));
  const blocked: Provider = {
    async *chat(messages) {
      if (messages[0]?.role !== "system") return;
      await gate;
      yield DONE;
    },
  };
  const k = backendWorld(new Mind());
  aiMem(k).providers.set("ai", blocked);
  const holder = k.send([{ target: "ai", payload: { type: "send", text: "first" } }]);
  await until(() => aiMem(k).current.has("ai"));
  // a second caller queues behind it and is killed by its OWN item deadline
  const [out] = await k.send([{ target: "ai", payload: { type: "send", text: "second", client_id: "tab2" }, timeout: 0.1 }]);
  assert.equal(out!.status, "timed_out", JSON.stringify(out));
  assert.deepEqual(aiMem(k).queue.get("ai") ?? [], [], "the row is GONE");
  const snap = (await k.send([{ target: "ai", payload: { type: "status" } }]))[0]!.result as Payload;
  assert.equal(snap["others_pending"], 0);
  assert.equal(snap["generating"], true, "the holder is still the in-flight turn");
  release();
  await holder;
  await settle(k);
});

// ── every row has an address ─────────────────────────────────────────────────

test("an external message is a root", () => {
  const w = new World();
  const [rid, parent] = rowIdsForIncoming("please survey the corpus", w.k);
  assert.equal(typeof rid, "number");
  assert.equal(parent, null);
  assert.equal(rowIdsForIncoming("again", w.k)[0], rid + 1, "the counter, in order");
});

test("a delivered reply turn names its parent from the tag", () => {
  const w = new World();
  const [rid, parent] = rowIdsForIncoming('[reply to 40 · notes read · "read the spec" · ↑17]\nok', w.k);
  assert.equal(parent, 17);
  assert.notEqual(rid, parent);
});

test("the reply tag carries every segment in order and still parses", () => {
  const tag = replyTag(40, "notes", "read", "read the spec", 17, 2, 3, "17");
  assert.equal(tag, '[reply to 40 · notes read · "read the spec" · ↑17 · 2 of 3 in batch 17]');
  const m = REPLY_TAG.exec(tag + "\nbody")!;
  assert.equal(m[1], "notes");
  assert.equal(m[2], "read");
  assert.equal(PID_TAG.exec(tag)![1], "40");
});

test("optional segments are omitted cleanly", () => {
  assert.equal(replyTag(1, "t", "v", null, null, 1, 1, "x"), "[reply to 1 · t v]");
  assert.equal(replyTag(1, "t", "v", null, 5, 1, 1, "5"), "[reply to 1 · t v · ↑5]");
});

test("the batch token is the assistant row's id", () => {
  const w = new World();
  const turn = w.k.mintId();
  const tag = replyTag(1, "t", "v", null, turn, 1, 2, String(turn));
  assert.ok(tag.includes(`in batch ${turn}`) && tag.includes(`↑${turn}`));
});

// ── side calls strip reasoning like the main turn ────────────────────────────

class Chunked implements Provider {
  readonly text: string;
  lastStopReason: string | null = null;
  constructor(text: string) {
    this.text = text;
  }
  async *chat(): AsyncIterable<string> {
    for (let i = 0; i < this.text.length; i += 7) yield this.text.slice(i, i + 7);
  }
}

test("the summarizer drops the think block", async () => {
  const s = makeSummarizer(new Chunked("<think>The user wants me to summarize.</think>\nRead 8 files; all pangram.\n\nNothing binds."));
  assert.equal(await s([{ role: "user", content: "x" }]), "Read 8 files; all pangram.\n\nNothing binds.");
});

test("compose drops the think block, even unterminated", async () => {
  assert.equal(await compose(new Chunked('<think>hm</think>{"expand": []}'), "p"), '{"expand": []}');
  assert.equal(await compose(new Chunked("<think>cut off mid-thought"), "p"), "");
});

function sideCtx(summarize: ProjectionCtx["summarize"]): ProjectionCtx {
  return { budget: 12000, recentN: 4, summarize, selfId: "ai", kernel: null, clientId: "c" };
}

test("a fold title is the first non-empty line after stripping", async () => {
  const rows = [{ role: "user", content: "go", id: "u:1", parent: null, title: "t" }];
  const out = await summarizeSpan(sideCtx(async () => '\n\n"Read 8 files; all pangram."\n\nAsked to read; found nothing unusual.'), rows, [0]);
  assert.deepEqual(out, ["Read 8 files; all pangram.", "Asked to read; found nothing unusual.", false]);
});

test("a summary that still opens a think block degrades to titles", async () => {
  const rows = [{ role: "user", content: "go", id: "u:1", parent: null, title: "t" }];
  const out = await summarizeSpan(sideCtx(async () => "<think>never closed"), rows, [0]);
  assert.equal(out![2], true);
  assert.ok(!out![0].includes("<think"));
});
