// The browser AI agent's MEMORY STORE: the conversation lives in the store its
// record names (lazy boot, write-through head refreshed from the store, ids
// minted at the append, turns = rows with a chat role), reply bodies are never
// persisted (the origin line), and context is shared by GRANT (fork_from with
// the owner verified, third-owner stubs, request_share, watch/unwatch).
// Mirrors the store/grant parts of python ai_core/tests/test_memory_features.py;
// the fold/compaction parts have no ts counterpart (the ts backend has no
// compaction subsystem).

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Payload } from "../src/kernel/json.ts";
import { registerRamStore } from "../src/bundles/memory/ram_store.ts";
import { makeAIBackend, type AIRecord, type Message, type Provider } from "../src/bundles/ai/ai_backend.ts";
import { loadHistory, renderShared, saveHistory } from "../src/bundles/ai/memory.ts";

const ROOT = "kernel_state";
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Per-agent scripted turns; records every message list a provider was handed. */
interface Script {
  turns: Map<string, string[]>;
  seen: Map<string, Message[][]>;
}

function world(opts: { aiMeta?: Payload; storeCallable?: Payload } = {}): { k: Kernel; s: Script } {
  const k = new Kernel();
  const s: Script = { turns: new Map(), seen: new Map() };
  registerRamStore(k);
  k.registerBundle(
    "ai.ts",
    makeAIBackend((id) => k.get(id)!.meta as AIRecord, {
      makeProvider: (id): Provider => ({
        async *chat(messages) {
          const seen = s.seen.get(id) ?? [];
          seen.push(messages.map((m) => ({ ...m })));
          s.seen.set(id, seen);
          const list = s.turns.get(id) ?? [];
          yield list.length > 0 ? (list.shift() as string) : "done.";
        },
      }),
    }),
  );
  k.registerBundle("tool.tools", (_id, p) => {
    if (p["type"] === "big") return { text: "x".repeat(6000) };
    if (p["type"] === "sourced") return { text: "y".repeat(6000), origin: "notes.md lines 1–200 v3f2a — read {path, range}" };
    return { ok: true };
  });
  k.setRoot(new Agent({ id: ROOT, meta: { guid: "g1" } }));
  k.register(
    new Agent({
      id: "memory",
      parentId: ROOT,
      handlerModule: "ram_store.ts",
      meta: {
        operator: true,
        admins: ["kernel"],
        callable_by: opts.storeCallable ?? { alice: "*", bob: "*", carol: "*" },
      },
    }),
  );
  k.register(new Agent({ id: "tool", parentId: ROOT, handlerModule: "tool.tools", meta: { callable_by: { alice: "*" } } }));
  for (const [id, peer] of [["alice", "bob"], ["bob", "alice"]] as const) {
    k.register(
      new Agent({
        id,
        parentId: ROOT,
        handlerModule: "ai.ts",
        meta: {
          memory_store_id: "memory",
          can_call: { memory: "*", [peer]: "*", tool: "*" },
          callable_by: { [peer]: "*" },
          ...(opts.aiMeta ?? {}),
        },
      }),
    );
  }
  return { k, s };
}

const call = (id: string, payload: object) =>
  `<tool_call>${JSON.stringify({ name: "send", arguments: { target_id: id, payload } })}</tool_call>`;

async function send(k: Kernel, id: string, text: string): Promise<Payload> {
  return (await k.send([{ target: id, payload: { type: "send", text } }]))[0]!.result as Payload;
}

async function storeRows(k: Kernel, owner: string): Promise<Payload[]> {
  const r = (await k.send([{ target: "memory", payload: { type: "read", owner } }]))[0]!.result as Payload;
  return r["rows"] as Payload[];
}

async function until(fn: () => Promise<boolean> | boolean, ms = 2000): Promise<void> {
  const t0 = Date.now();
  while (!(await fn())) {
    if (Date.now() - t0 > ms) throw new Error("condition never held");
    await sleep(5);
  }
}

// ── no store, no turn ──────────────────────────────────────────────────

test("an agent with no memory_store_id fails loud at boot and at its first turn", async () => {
  const { k } = world({ aiMeta: { memory_store_id: null } });
  const boot = (await k.send([{ target: "alice", payload: { type: "boot" } }]))[0]!.result as Payload;
  assert.equal(boot["reason"], "missing_arg");
  assert.equal(boot["arg"], "memory_store_id");
  const out = await send(k, "alice", "hi");
  assert.equal(out["reason"], "missing_arg");
  assert.match(String(out["error"]), /memory_store_id/);
});

test("boot is LAZY — it never calls the store", async () => {
  const { k } = world({ storeCallable: {} }); // the store refuses everyone
  const boot = (await k.send([{ target: "alice", payload: { type: "boot" } }]))[0]!;
  assert.equal(boot.status, "completed");
  assert.equal(boot.result, null);
  // the first turn is where a refusing store surfaces: fail fast
  const out = await send(k, "alice", "hi");
  assert.equal(out["reason"], "memory_store");
});

// ── the conversation is rows in the store ──────────────────────────────

test("a turn's rows land in the store in id order; the next turn reads them back", async () => {
  const { k, s } = world();
  s.turns.set("alice", [call("tool", { type: "ping" }), "pong received"]);
  const out = await send(k, "alice", "check the tool");
  assert.equal(out["response"], "pong received");
  const rows = await storeRows(k, "alice");
  assert.deepEqual(rows.map((r) => r["role"]), ["user", "assistant", "user", "assistant"]);
  const ids = rows.map((r) => r["id"] as number);
  assert.deepEqual(ids, [...ids].sort((a, b) => a - b), "append order is id order");
  assert.equal(rows[0]!["content"], "check the tool");
  assert.equal(rows[1]!["parent"], rows[0]!["id"], "the answer names what it answers");
  assert.equal(rows[2]!["parent"], rows[1]!["id"], "a reply names the turn that made the call");
  assert.match(String(rows[2]!["content"]), /"ok":true/, "a small reply stays inline");

  s.turns.set("alice", ["second"]);
  await send(k, "alice", "again");
  const second = s.seen.get("alice")!.at(-1)!;
  assert.equal(second[0]!.role, "system");
  assert.deepEqual(second.slice(1, 3).map((m) => m.content), ["check the tool", rows[1]!["content"]]);
  // `history` is python's DIGEST resolution: {id, parent, kind, role, head, bytes}
  const hist = (await k.send([{ target: "alice", payload: { type: "history" } }]))[0]!.result as Payload;
  assert.equal(hist["total"], 6);
  const digests = hist["rows"] as Payload[];
  assert.deepEqual(digests.map((d) => d["role"]), ["user", "assistant", "user", "assistant", "user", "assistant"]);
  assert.equal(digests[0]!["head"], "check the tool");
  assert.ok(digests.every((d) => typeof d["bytes"] === "number" && "parent" in d));
});

test("a row another writer appends reaches the next head read — merged once", async () => {
  const { k } = world();
  await saveHistory("alice", k, [{ role: "user", content: "first" }]);
  assert.deepEqual((await loadHistory("alice", k)).map((r) => r["content"]), ["first"]);
  const rid = k.mintId();
  const out = (await k.send([
    { target: "memory", payload: { type: "append", owner: "alice", rows: [{ id: rid, role: "user", content: "planted" }] } },
  ]))[0]!.result as Payload;
  assert.deepEqual(out["ids"], [rid]);
  assert.deepEqual((await loadHistory("alice", k)).map((r) => r["content"]), ["first", "planted"]);
  assert.equal((await loadHistory("alice", k)).length, 2);
});

test("a note the agent appends itself is memory, not a turn", async () => {
  const { k } = world();
  await saveHistory("alice", k, [{ role: "user", content: "hi" }]);
  await loadHistory("alice", k); // the cache is warm
  const out = (await k.scopedFor("alice", null).send([
    { target: "memory", payload: { type: "append", rows: [{ kind: "note", content: "vault code 4711" }] } },
  ]))[0]!.result as Payload;
  assert.equal(out["appended"], 1);
  assert.deepEqual((await loadHistory("alice", k)).map((r) => r["content"]), ["hi"]);
  const behind = await loadHistory("alice", k, { behind: true });
  assert.ok(behind.every((r) => r["role"]));
  assert.equal((await storeRows(k, "alice")).length, 2, "the note stays in the store");
});

// ── bodies are never persisted ─────────────────────────────────────────

test("a reply over the inline cap is held in RAM and digested to its origin line; grep reaches it", async () => {
  // window 2400, no reserve → budget 2400 → inline cap 300 tokens
  const { k, s } = world({ aiMeta: { context_window: 2400, output_reserve: 0 } });
  s.turns.set("alice", [call("tool", { type: "big", path: "a.txt" }), call("tool", { type: "sourced" }), "read both"]);
  await send(k, "alice", "read the files");
  const rows = await storeRows(k, "alice");
  const replies = rows.filter((r) => r["role"] === "user" && String(r["content"]).startsWith("<tool_response>"));
  assert.equal(replies.length, 2);
  assert.ok(!String(replies[0]!["content"]).includes("xxxxxxxx"), "the body was persisted");
  assert.match(String(replies[0]!["content"]), /NOT inlined, held in memory only until this agent restarts/);
  const pid = (replies[0]!["reply"] as Payload)["pid"] as number;
  assert.deepEqual(replies[0]!["reply"], { pid, target: "tool", verb: "big" }, "the row names the call it answered");
  assert.match(String(replies[0]!["content"]), new RegExp(`"ids":\\[${pid}\\]`), "the grep hint names the pid");
  assert.match(String(replies[0]!["content"]), /origin: tool big \{"path":"a\.txt"\} — re-send it; the output may differ/);
  assert.match(String(replies[1]!["content"]), /origin: notes\.md lines 1–200 v3f2a — read \{path, range\}/,
    "the replying agent's own origin wins");
  // the pass after the call saw the DIGEST — the view is rebuilt from the store
  const pass2 = s.seen.get("alice")![1]!;
  assert.ok(!pass2.some((m) => m.content.includes("x".repeat(6000))));
  assert.ok(pass2.some((m) => m.content.includes("NOT inlined")));
  // the body is held this incarnation: grep scoped to the pid finds a line in it
  const g = (await k.send([{ target: "alice", payload: { type: "grep", query: "xxxx", ids: [pid] } }]))[0]!.result as Payload;
  assert.ok((g["total"] as number) >= 1, JSON.stringify(g));
  assert.equal((g["matches"] as Payload[])[0]!["id"], pid);
});

// ── context by grant ───────────────────────────────────────────────────

async function share(k: Kernel, owner: string, withId: string, scope: Payload): Promise<Payload> {
  return (await k.scopedFor(owner, null).send([
    { target: "memory", payload: { type: "share", with: withId, scope } },
  ]))[0]!.result as Payload;
}

test("fork_from reads the owner's rows under the grant and kickstarts; unshare fails the next read", async () => {
  const { k, s } = world();
  await saveHistory("alice", k, [{ role: "user", content: "the vault code is 4711" }]);
  const g = await share(k, "alice", "bob", { head_until: 1_000_000_000 });
  s.turns.set("bob", ["on it"]);
  const out = (await k.send([
    { target: "bob", payload: { type: "fork_from", owner: "alice", grant: g["grant"] as number, message: "what is the code?" } },
  ]))[0]!.result as Payload;
  assert.equal(out["forked"], true);
  assert.equal(out["rows"], 1);
  const hist = await loadHistory("bob", k);
  const fork = hist.find((r) => r["kind"] === "fork")!;
  assert.match(String(fork["content"]), /\[shared from alice · read-only/);
  assert.match(String(fork["content"]), /4711/, "the owner's row, read back under the grant");
  // the KICKSTART arrives as bob's own turn at once
  await until(async () => (await storeRows(k, "bob")).some((r) => r["role"] === "assistant"));
  const kick = (await storeRows(k, "bob")).find((r) => r["role"] === "user" && r["kind"] !== "fork")!;
  assert.match(String(kick["content"]), /you are a new agent, not alice/);
  assert.ok(String(kick["content"]).endsWith("what is the code?"));
  // the model saw alice's rows attributed, not as its own turns
  const seen = s.seen.get("bob")![0]!;
  assert.ok(seen.some((m) => m.content.includes("(user · row") && m.content.includes("4711")));
  // the owner revokes — the reader's next read fails fast
  await k.scopedFor("alice", null).send([{ target: "memory", payload: { type: "unshare", grant: g["grant"] as number } }]);
  await assert.rejects(loadHistory("bob", k), /memory store/);
  const next = await send(k, "bob", "go on");
  assert.equal(next["reason"], "memory_store");
});

test("fork_from refuses an owner the grant does not name, and an unknown grant", async () => {
  const { k } = world();
  await saveHistory("alice", k, [{ role: "user", content: "alice's row" }]);
  const g = await share(k, "alice", "bob", { head_until: 1_000_000_000 });
  const wrong = (await k.send([
    { target: "bob", payload: { type: "fork_from", owner: "mallory", grant: g["grant"] as number, message: "go" } },
  ]))[0]!.result as Payload;
  assert.equal(wrong["reason"], "bad_arg");
  assert.match(String(wrong["error"]), /given by alice/);
  const none = (await k.send([
    { target: "bob", payload: { type: "fork_from", owner: "alice", grant: 99_999, message: "go" } },
  ]))[0]!.result as Payload;
  assert.equal(none["reason"], "not_found");
});

test("request_share reaches the OWNER's model as a turn", async () => {
  const { k, s } = world();
  s.turns.set("alice", ["I will think about it"]);
  const out = (await k.scopedFor("bob", null).send([
    { target: "alice", payload: { type: "request_share", row: 42, why: "need it" } },
  ]))[0]!.result as Payload;
  assert.deepEqual(out, { requested: true, owner: "alice", row: 42 });
  await until(async () => (await storeRows(k, "alice")).some((r) => r["role"] === "assistant"));
  const turn = (await storeRows(k, "alice")).find((r) => r["role"] === "user")!;
  assert.match(String(turn["content"]), /^\[request_share from bob · row 42\] need it/);
  assert.match(String(turn["content"]), /"type":"share","with":"bob","scope":\{"row":42\}/);
});

test("a shared scope shows a third owner's row as a stub naming ITS row id", () => {
  const text = renderShared("alice", 2, [
    { id: 1, role: "user", content: "mine" },
    { id: 2, role: "user", kind: "fork", owner: "carol", as_of: 57, content: "[fork of carol]" },
  ]);
  assert.match(text, /\(user · row 1\) mine/);
  assert.match(text, /\[row 57 · owner carol · not shared with you\]/);
});

// ── the row event ──────────────────────────────────────────────────────

test("watch: a digest per row; bodies only under a live grant; unwatch stops", async () => {
  const { k, s } = world();
  const got: Payload[] = [];
  const off = k.onInbox("bob", (e) => { if (e["type"] === "row") got.push(e); });
  const asBob = k.scopedFor("bob", null);
  // an agent watches for ITSELF
  const forOther = (await asBob.send([{ target: "alice", payload: { type: "watch", watcher: "carol" } }]))[0]!.result as Payload;
  assert.equal(forOther["reason"], "unauthorized");
  const w = (await asBob.send([{ target: "alice", payload: { type: "watch", body: true } }]))[0]!.result as Payload;
  assert.deepEqual(w, { watching: true, watcher: "bob", body: true });
  await saveHistory("alice", k, [{ role: "user", content: "secret 1" }]);
  assert.equal(got.length, 1);
  assert.equal((got[0]!["row"] as Payload)["role"], "user");
  assert.equal("content" in (got[0]!["row"] as Payload), false, "no grant, no body");
  // the MODEL shares through its own store call — the grants cache refreshes
  s.turns.set("alice", [call("memory", { type: "share", with: "bob", scope: { head_until: 1_000_000_000 }, mode: "live" }), "shared"]);
  await send(k, "alice", "share your thread with bob");
  const withBody = got.filter((e) => "content" in (e["row"] as Payload));
  assert.ok(withBody.length > 0, "a live grant covering the row carries its body");
  const n = got.length;
  await asBob.send([{ target: "alice", payload: { type: "unwatch" } }]);
  await saveHistory("alice", k, [{ role: "user", content: "after" }]);
  assert.equal(got.length, n, "unwatch stops the events");
  off();
  // no reader, no watch
  const operatorWatch = (await k.send([{ target: "alice", payload: { type: "watch", watcher: "nobody" } }]))[0]!.result as Payload;
  assert.equal(operatorWatch["reason"], "no_consumer");
});

test("a store that refuses mid-turn fails the turn fast", async () => {
  const { k, s } = world();
  s.turns.set("alice", ["hello"]);
  await send(k, "alice", "first"); // warm
  k.updateMeta("memory", { callable_by: {} }); // the store now refuses alice
  const out = await send(k, "alice", "second");
  assert.equal(out["reason"], "memory_store");
  assert.match(String(out["error"]), /memory store 'memory'/);
  // what the store held is untouched
  assert.equal((await storeRows(k, "alice")).length, 2);
});
