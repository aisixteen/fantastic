// LISTENER ISOLATION — every place the kernel and its bridge call a list of
// callbacks: one callback throwing must not stop the next. Local state
// subscribers, inbox listeners (and the watcher fan-out behind them), a socket's
// connect/disconnect listeners, the host state-stream handlers, and a task's
// cancel hooks.
//
// Also pinned: the kernel calls its state subscribers and inbox listeners from a
// SNAPSHOT of the list (python `publish_state` iterates `tuple(...)`): one that
// unsubscribes another, or subscribes a new one, mid-dispatch changes the NEXT
// dispatch, not the current one.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { WsBridge } from "../src/transport/bridge.ts";
import type { WebSocketLike } from "../src/transport/bridge.ts";
import { encodeFrame } from "../src/transport/frame.ts";
import type { Payload } from "../src/kernel/json.ts";
import type { StateEvent } from "../src/kernel/state.ts";

const ROOT = "kernel_state";
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const boom = (): never => {
  throw new Error("listener blew up");
};

function kernel(): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: ROOT }));
  return k;
}

test("a throwing state subscriber does not stop the next; the list is a snapshot per event", () => {
  const k = kernel();
  const seen: string[] = [];
  let offC: () => void = () => undefined;
  let added = false;
  k.addStateSubscriber(boom);
  k.addStateSubscriber((e) => {
    seen.push(`a:${e.kind}:${e.agent_id}`);
    offC(); // removes C — C still gets THIS event
    if (!added) {
      added = true;
      k.addStateSubscriber((e2) => seen.push(`d:${e2.kind}:${e2.agent_id}`)); // D starts with the NEXT one
    }
  });
  offC = k.addStateSubscriber((e) => seen.push(`c:${e.kind}:${e.agent_id}`));
  k.register(new Agent({ id: "x", parentId: ROOT }));
  k.updateMeta("x", { n: 1 });
  assert.deepEqual(seen, ["a:added:x", "c:added:x", "a:updated:x", "d:updated:x"]);
});

test("every state event is stamped and carries the event's own fields", () => {
  const k = kernel();
  const events: StateEvent[] = [];
  k.addStateSubscriber((e) => events.push(e));
  k.register(new Agent({ id: "x", parentId: ROOT, meta: { display_name: "Ex" } }));
  k.updateMeta("x", { n: 1, m: 2 });
  k.remove("x");
  assert.deepEqual(
    events.map(({ ts, ...rest }) => {
      assert.equal(typeof ts, "number");
      return rest;
    }),
    [
      { agent_id: "x", kind: "added", name: "Ex", parent_id: ROOT },
      { agent_id: "x", kind: "updated", changed: ["n", "m"] },
      { agent_id: "x", kind: "removed", name: "Ex" },
    ],
  );
});

test("a throwing inbox listener does not stop the next listener, nor the fan-out to watchers", () => {
  const k = kernel();
  k.register(new Agent({ id: "src", parentId: ROOT }));
  const direct: Payload[] = [];
  const watcher: Payload[] = [];
  k.onInbox("src", boom);
  k.onInbox("src", (p) => direct.push(p));
  k.onInbox("w", boom);
  k.onInbox("w", (p) => watcher.push(p));
  k.watch("src", "w");
  k.emit("src", { type: "tick", n: 1 });
  assert.deepEqual(direct, [{ type: "tick", n: 1 }]);
  assert.deepEqual(watcher, [{ type: "tick", n: 1 }]);
});

test("onInbox: its unsubscribe removes that listener only, twice is harmless, and the list is a snapshot per delivery", () => {
  const k = kernel();
  k.register(new Agent({ id: "src", parentId: ROOT }));
  const seen: string[] = [];
  let offB: () => void = () => undefined;
  const offA = k.onInbox("src", (p) => {
    seen.push(`a${String(p["n"])}`);
    offB(); // B still gets THIS payload
  });
  offB = k.onInbox("src", (p) => seen.push(`b${String(p["n"])}`));
  k.onInbox("src", (p) => seen.push(`c${String(p["n"])}`));
  k.emit("src", { type: "tick", n: 1 });
  k.emit("src", { type: "tick", n: 2 });
  offA();
  offA();
  k.emit("src", { type: "tick", n: 3 });
  assert.deepEqual(seen, ["a1", "b1", "c1", "a2", "c2", "c3"]);
});

// ── the bridge's listeners ─────────────────────────────────────────────

class FakeSocket implements WebSocketLike {
  binaryType = "blob";
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  constructor() {
    setTimeout(() => this.onopen?.(undefined), 0);
  }
  send(): void {}
  close(): void {}
}

test("a throwing connect/disconnect listener does not stop the next", async () => {
  const socks: FakeSocket[] = [];
  const k = kernel();
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: ROOT,
    makeSocket: () => {
      const s = new FakeSocket();
      socks.push(s);
      return s;
    },
  });
  const seen: string[] = [];
  k.onLifecycle("pty1", boom);
  k.onLifecycle("pty1", (s) => seen.push(s));
  while (seen.length === 0) await sleep(1);
  socks[0]!.onclose?.(undefined);
  assert.deepEqual(seen, ["connected", "disconnected"]);
  bridge.close();
});

test("a throwing state-stream handler does not stop the next", async () => {
  const socks: FakeSocket[] = [];
  const k = kernel();
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: ROOT,
    makeSocket: () => {
      const s = new FakeSocket();
      socks.push(s);
      return s;
    },
  });
  const seen: Record<string, unknown>[] = [];
  k.subscribeState(boom);
  k.subscribeState((f) => seen.push(f));
  await sleep(5);
  socks[0]!.onmessage?.({ data: encodeFrame({ type: "state_event", kind: "added", agent_id: "x" }).data });
  assert.deepEqual(seen, [{ type: "state_event", kind: "added", agent_id: "x" }]);
  bridge.close();
});

// ── cancel hooks ───────────────────────────────────────────────────────

test("a throwing cancel hook does not stop the rest of the subtree: every task is killed and settles cancelled", async () => {
  const k = kernel();
  const released: string[] = [];
  const running = new Set<string>();
  k.registerBundle("holder.ts", async (id, payload, kernel) => {
    if (payload["type"] !== "hold") return null;
    const row = kernel.currentTaskRow!;
    row.onCancel.unshift(boom); // BEFORE the kernel's own hook on this row
    row.onCancel.push(() => released.push(id));
    running.add(id);
    if (id === "parent") {
      const [child] = await kernel.send([{ target: "child", payload: { type: "hold" } }]);
      return { child: child!.status };
    }
    return new Promise<never>(() => undefined);
  });
  k.register(new Agent({ id: "parent", parentId: ROOT, handlerModule: "holder.ts" }));
  k.register(new Agent({ id: "child", parentId: "parent", handlerModule: "holder.ts" }));
  const held = k.send([{ target: "parent", payload: { type: "hold" } }]);
  while (running.size < 2) await sleep(1);
  const parentPid = k.tasks.ids().find((t) => k.tasks.get(t)!.agent === "parent")!;
  const childPid = k.tasks.ids().find((t) => k.tasks.get(t)!.agent === "child")!;
  const out = (await k.send([{ target: "kernel", payload: { type: "cancel", task_id: parentPid } }]))[0]!;
  assert.deepEqual(out.result, { cancelled: true, killed: [childPid, parentPid] });
  const [settled] = await held;
  assert.equal(settled!.status, "cancelled");
  assert.deepEqual(settled!.result, { cancelled: true });
  assert.deepEqual(released, ["child", "parent"], "a hook after a throwing one did not run");
  assert.equal(k.tasks.size(), 0);
});

test("cancel is idempotent: an unknown id kills nothing, and a row already killed is not killed (nor its hooks run) twice", async () => {
  const k = kernel();
  const hooks: string[] = [];
  const running = new Set<string>();
  k.registerBundle("holder.ts", async (id, payload, kernel) => {
    if (payload["type"] !== "hold") return null;
    kernel.currentTaskRow!.onCancel.push(() => hooks.push(id));
    running.add(id);
    if (id === "parent") await kernel.send([{ target: "child", payload: { type: "hold" } }]);
    return new Promise<never>(() => undefined);
  });
  k.register(new Agent({ id: "parent", parentId: ROOT, handlerModule: "holder.ts" }));
  k.register(new Agent({ id: "child", parentId: "parent", handlerModule: "holder.ts" }));
  const held = k.send([{ target: "parent", payload: { type: "hold" } }]);
  while (running.size < 2) await sleep(1);
  const parentPid = k.tasks.ids().find((t) => k.tasks.get(t)!.agent === "parent")!;
  const childPid = k.tasks.ids().find((t) => k.tasks.get(t)!.agent === "child")!;
  assert.deepEqual(k.tasks.cancel(987654), []);
  // the child first, then — in the same tick, its row still in the table — the parent
  assert.deepEqual(k.tasks.cancel(childPid), [childPid]);
  assert.deepEqual(k.tasks.cancel(parentPid), [parentPid]);
  assert.deepEqual(k.tasks.cancel(parentPid), [], "a killed row is not killed again");
  assert.deepEqual(hooks, ["child", "parent"]);
  assert.equal((await held)[0]!.status, "cancelled");
});
