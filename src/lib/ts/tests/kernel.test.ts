import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import type { Bridge } from "../src/kernel/kernel.ts";
import type { Json, Payload } from "../src/kernel/json.ts";

function withRoot(): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "kernel_state" }));
  return k;
}

test("the reserved 'kernel' address resolves to the root agent", async () => {
  const k = withRoot();
  const viaAlias = ((await k.send([{ target: "kernel", payload: { type: "reflect" } }]))[0]!.result) as Payload;
  const viaId = ((await k.send([{ target: "kernel_state", payload: { type: "reflect" } }]))[0]!.result) as Payload;
  assert.equal(viaAlias["id"], "kernel_state");
  assert.deepEqual(viaAlias, viaId);
});

test("a local view-agent dispatches its domain verb to its handler", async () => {
  const k = withRoot();
  k.registerBundle("echo.js", (_id, p) => ({ echoed: p["text"] ?? null }));
  k.register(
    new Agent({ id: "e1", parentId: "kernel_state", handlerModule: "echo.js" }),
  );
  const r = ((await k.send([{ target: "e1", payload: { type: "say", text: "hi" } }]))[0]!.result) as Payload;
  assert.deepEqual(r, { echoed: "hi" });
});

test("reflect is kernel-native even for handler-bearing agents", async () => {
  const k = withRoot();
  // a handler that would hijack reflect if reflect weren't native:
  k.registerBundle("evil.js", () => ({ hijacked: true }));
  k.register(
    new Agent({
      id: "v",
      parentId: "kernel_state",
      handlerModule: "evil.js",
      sentence: "I am a view.",
    }),
  );
  const r = ((await k.send([{ target: "v", payload: { type: "reflect", tree: "none" } }]))[0]!.result) as Payload;
  assert.equal(r["sentence"], "I am a view.");
  assert.equal(r["hijacked"], undefined);
});

test("bare local agent answers reflect but errors on a domain verb", async () => {
  const k = withRoot();
  k.register(new Agent({ id: "bare", parentId: "kernel_state" }));
  const refl = ((await k.send([{ target: "bare", payload: { type: "reflect" } }]))[0]!.result) as Payload;
  assert.equal(refl["id"], "bare");
  const err = ((await k.send([{ target: "bare", payload: { type: "frobnicate" } }]))[0]!.result) as Payload;
  // python `_dispatch`'s refusal, verbatim (no hint)
  assert.deepEqual(err, {
    error: "agent 'bare' has no handler_module; cannot answer verb 'frobnicate'",
    reason: "unhandled",
    verb: "frobnicate",
  });
});

test("unknown target with no bridge errors; with a bridge it forwards", async () => {
  const noBridge = withRoot();
  const e = ((await noBridge.send([{ target: "ghost", payload: { type: "reflect" } }]))[0]!.result) as Payload;
  assert.match(String(e["error"]), /no agent 'ghost'/);

  const k = withRoot();
  const calls: Array<[string, Payload]> = [];
  const fake: Bridge = {
    forward: (target, payload) => {
      calls.push([target, payload]);
      return Promise.resolve({ from: "host", target } as Json);
    },
    forwardBinary: () => Promise.resolve(null),
    emitRemote: () => {},
    watchRemote: () => {},
    unwatchRemote: () => {},
    onLifecycle: () => () => {},
    subscribeState: () => () => {},
  };
  k.bridge = fake;
  const r = ((await k.send([{ target: "host_agent", payload: { type: "ls" } }]))[0]!.result) as Payload;
  assert.deepEqual(r, { from: "host", target: "host_agent" });
  assert.deepEqual(calls, [["host_agent", { type: "ls" }]]);
});

test("emit fans out to inbox listeners and watchers", async () => {
  const k = withRoot();
  k.register(new Agent({ id: "src", parentId: "kernel_state" }));
  const seenDirect: Payload[] = [];
  const seenWatcher: Payload[] = [];
  k.onInbox("src", (p) => seenDirect.push(p));
  k.onInbox("watcher", (p) => seenWatcher.push(p));
  k.watch("src", "watcher");

  k.emit("src", { type: "event", data: 1 });
  assert.deepEqual(seenDirect, [{ type: "event", data: 1 }]);
  assert.deepEqual(seenWatcher, [{ type: "event", data: 1 }]);

  k.unwatch("src", "watcher");
  k.emit("src", { type: "event", data: 2 });
  assert.equal(seenWatcher.length, 1, "unwatched listener stops receiving");
  assert.equal(seenDirect.length, 2);
});

test("watching a REMOTE src asks the bridge once; unwatch releases", async () => {
  const k = withRoot();
  const watched: string[] = [];
  const unwatched: string[] = [];
  k.bridge = {
    forward: () => Promise.resolve(null),
    forwardBinary: () => Promise.resolve(null),
    emitRemote: () => {},
    watchRemote: (s) => watched.push(s),
    unwatchRemote: (s) => unwatched.push(s),
    onLifecycle: () => () => {},
    subscribeState: () => () => {},
  };
  k.watch("remote_pty", "a");
  k.watch("remote_pty", "b"); // second watcher: no extra bridge call
  assert.deepEqual(watched, ["remote_pty"]);
  k.unwatch("remote_pty", "a");
  assert.deepEqual(unwatched, []); // still one watcher left
  k.unwatch("remote_pty", "b");
  assert.deepEqual(unwatched, ["remote_pty"]);
});

// ── python parity (divergence fix 2026-10-01) ─────────────────────────────

test("a SEND is mirrored to the target's own inbox and to its watchers", async () => {
  // python `_dispatch` → `_fanout(kind="send")`: a watcher sees the raw inbound
  // payloads, which is exactly why `watch` is gated like a call.
  const k = withRoot();
  k.registerBundle("echo.js", () => ({ ok: true }));
  k.register(new Agent({ id: "src", parentId: "kernel_state", handlerModule: "echo.js" }));
  const own: Payload[] = [];
  const mirrored: Payload[] = [];
  k.onInbox("src", (p) => own.push(p));
  k.onInbox("w", (p) => mirrored.push(p));
  k.watch("src", "w");
  await k.send([{ target: "src", payload: { type: "say", text: "hi" } }]);
  assert.deepEqual(own, [{ type: "say", text: "hi" }]);
  assert.deepEqual(mirrored, [{ type: "say", text: "hi" }]);
});

test("watch is gated as the agent RUNNING the code, not the reader it names", () => {
  // python `Agent.watch` gates `_current_sender` → src: naming a reader that holds
  // a grant does not borrow it.
  const k = withRoot();
  k.register(new Agent({ id: "src", parentId: "kernel_state" }));
  k.register(new Agent({ id: "trusted", parentId: "kernel_state", meta: { can_call: { src: ["watch"] } } }));
  k.register(new Agent({ id: "nosy", parentId: "kernel_state" }));
  k.get("src")!.meta["callable_by"] = { trusted: ["watch"], nosy: [] };
  const asNosy = k.scopedFor("nosy", null);
  const d = asNosy.watch("src", "trusted");
  assert.equal((d as Payload | null)?.["error"], "nosy may not call 'watch' on src");
  const asTrusted = k.scopedFor("trusted", null);
  assert.equal(asTrusted.watch("src", "trusted"), null);
});

test("record(): parent_id comes LAST from the tree; meta cannot shadow it; the root has none", () => {
  const k = withRoot();
  k.register(new Agent({ id: "a", parentId: "kernel_state", handlerModule: "x.js", meta: { x: 1 } }));
  assert.deepEqual(Object.keys(k.get("a")!.record()), ["id", "handler_module", "x", "can_call", "callable_by", "parent_id"]);
  const rec = k.updateMeta("a", { parent_id: "elsewhere" });
  assert.equal(rec!["parent_id"], "kernel_state");
  k.updateMeta("kernel_state", { parent_id: "elsewhere" });
  assert.equal("parent_id" in k.get("kernel_state")!.record(), false);
});

test("local state events stamp `ts` in SECONDS (python `time.time()`)", () => {
  const k = withRoot();
  const ts: number[] = [];
  k.addStateSubscriber((e) => ts.push(e["ts"] as number));
  k.register(new Agent({ id: "a", parentId: "kernel_state" }));
  const now = Date.now() / 1000;
  assert.equal(ts.length, 1);
  assert.ok(Math.abs((ts[0] as number) - now) < 5, `not seconds: ${ts[0]}`);
});

test("the full tree carries python's posture keys: `allow`, not `file_bridge_id`", async () => {
  const k = withRoot();
  k.register(new Agent({ id: "fb", parentId: "kernel_state", meta: { allow: ["a.txt"], file_bridge_id: "x" } }));
  const r = (await k.send([{ target: "kernel", payload: { type: "reflect", detail: "full" } }]))[0]!.result as Payload;
  const node = ((r["tree"] as Payload)["children"] as Payload[])[0]!;
  assert.deepEqual(node["allow"], ["a.txt"]);
  assert.equal("file_bridge_id" in node, false);
});

test("execution rows keep python's key order: `ts` last", async () => {
  const k = withRoot();
  k.registerBundle("echo.js", () => null);
  k.register(new Agent({ id: "e", parentId: "kernel_state", handlerModule: "echo.js" }));
  await k.send([{ target: "e", payload: { type: "go" } }]);
  // `{**row, id, agent}` then `setdefault("ts")`; a call row's `outcome` is
  // stamped onto the live row at settle, after it — as in python.
  const [call, settle] = [...k.store.rows.values()];
  assert.deepEqual(Object.keys(call!).slice(-3), ["agent", "ts", "outcome"]);
  assert.deepEqual(Object.keys(settle!), ["kind", "parent", "settled", "outcome", "attempts", "id", "agent", "ts"]);
});
