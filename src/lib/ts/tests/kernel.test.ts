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

test("'kernel' alias resolves to the root agent", async () => {
  const k = withRoot();
  const viaAlias = (await k.send("kernel", { type: "reflect" })) as Payload;
  const viaId = (await k.send("kernel_state", { type: "reflect" })) as Payload;
  assert.equal(viaAlias["id"], "kernel_state");
  assert.deepEqual(viaAlias, viaId);
});

test("a local view-agent dispatches its domain verb to its handler", async () => {
  const k = withRoot();
  k.registerBundle("echo.js", (_id, p) => ({ echoed: p["text"] ?? null }));
  k.register(
    new Agent({ id: "e1", parentId: "kernel_state", handlerModule: "echo.js" }),
  );
  const r = (await k.send("e1", { type: "say", text: "hi" })) as Payload;
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
  const r = (await k.send("v", { type: "reflect", tree: "none" })) as Payload;
  assert.equal(r["sentence"], "I am a view.");
  assert.equal(r["hijacked"], undefined);
});

test("bare local agent answers reflect but errors on a domain verb", async () => {
  const k = withRoot();
  k.register(new Agent({ id: "bare", parentId: "kernel_state" }));
  const refl = (await k.send("bare", { type: "reflect" })) as Payload;
  assert.equal(refl["id"], "bare");
  const err = (await k.send("bare", { type: "frobnicate" })) as Payload;
  assert.match(String(err["error"]), /bare/);
});

test("unknown target with no bridge errors; with a bridge it forwards", async () => {
  const noBridge = withRoot();
  const e = (await noBridge.send("ghost", { type: "reflect" })) as Payload;
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
  const r = (await k.send("host_agent", { type: "ls" })) as Payload;
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
