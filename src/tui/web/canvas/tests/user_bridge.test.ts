import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { UserBridge, canon, pageVerbs, rerooted, writesPage } from "../src/user_bridge.ts";

/** The lib bridge the UserBridge wraps: it records what would go on the wire. */
function fakeInner(): { inner: never; sent: Array<{ target: string; payload: Record<string, unknown>; secret: unknown }> } {
  const sent: Array<{ target: string; payload: Record<string, unknown>; secret: unknown }> = [];
  const inner = {
    onLifecycle: () => () => {},
    forward: async (target: string, payload: Record<string, unknown>, _c?: string, secret?: unknown) => {
      sent.push({ target, payload, secret });
      return { ok: true };
    },
    forwardBinary: async (target: string, payload: Record<string, unknown>, _c?: string, secret?: unknown) => {
      sent.push({ target, payload, secret });
      return { ok: true };
    },
    close: () => {},
  };
  return { inner: inner as never, sent };
}

test("a host call goes to the gateway as forward, its marks one payload down, the token marked", async () => {
  const k = new Kernel();
  const { inner, sent } = fakeInner();
  const b = new UserBridge(k, inner, { loader: "web_loader", token: () => "T", tab: "tab1" });
  await b.forward("setup", { type: "store_key", secret: "k" } as Payload, undefined, ["payload.secret"]);
  assert.equal(sent[0]!.target, "users");
  assert.deepEqual(sent[0]!.payload, { type: "forward", target: "setup", payload: { type: "store_key", secret: "k" }, tab: "tab1", token: "T" });
  assert.deepEqual(sent[0]!.secret, ["payload.payload.secret", "payload.token"]);
  assert.deepEqual(rerooted([["payload", "a.b"], "result.x"], false), [["payload", "payload", "a.b"], "result.x"]);
  // no token: none sent, none marked
  const n = fakeInner();
  await new UserBridge(new Kernel(), n.inner, { loader: "web_loader", token: () => null, tab: "t" }).forward("x", { type: "v" } as Payload);
  assert.equal(n.sent[0]!.payload["token"], undefined);
  assert.deepEqual(n.sent[0]!.secret, []);
});

test("another tab's record change is applied here, and its echo is never sent back", async () => {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  const { inner, sent } = fakeInner();
  const b = new UserBridge(k, inner, { loader: "web_loader", token: () => null, tab: "me" });
  const rec = { id: "h1", parent_id: "canvas", handler_module: "html_agent.ts", x: 5, html: "<p>x</p>" };
  b.dispatch({ type: "record_changed", id: "h1", record: rec, origin: "other" });
  assert.equal(k.get("h1")?.meta["x"], 5, "added");
  b.dispatch({ type: "record_changed", id: "h1", record: { ...rec, x: 9 }, origin: "other" });
  assert.equal(k.get("h1")?.meta["x"], 9, "updated");
  // this tab's loader persists what it just applied: an echo, not sent
  const r = await b.forward("web_loader", { type: "persist_record", record: { html: "<p>x</p>", x: 9, id: "h1", handler_module: "html_agent.ts", parent_id: "canvas" } } as unknown as Payload);
  assert.deepEqual(r, { ok: true, echo: true });
  assert.equal(sent.length, 0);
  // this tab's OWN change comes back with its origin: skipped
  b.dispatch({ type: "record_changed", id: "h1", record: { ...rec, x: 1 }, origin: "me" });
  assert.equal(k.get("h1")?.meta["x"], 9);
  b.dispatch({ type: "record_changed", id: "h1", removed: true, origin: "other" });
  assert.equal(k.get("h1"), undefined, "removed");
  assert.equal(canon({ b: 1, a: { d: 2, c: 3 } }), canon({ a: { c: 3, d: 2 }, b: 1 }));
});

test("a watched agent's event reaches its local listeners", () => {
  const k = new Kernel();
  const { inner } = fakeInner();
  const b = new UserBridge(k, inner, { loader: "web_loader", token: () => null, tab: "me" });
  const got: unknown[] = [];
  k.onInbox("exec_1", (p) => got.push(p));
  b.dispatch({ type: "event", src: "exec_1", payload: { type: "output", n: 1 } });
  assert.deepEqual(got, [{ type: "output", n: 1 }]);
});

test("page grants: an entry covers the agent and what is under it; reflect alone does not write", () => {
  const cc = { "G|cv1": ["reflect"], "G|cv2": ["*"], "*|top": ["place"] };
  assert.deepEqual(pageVerbs(cc, "G", "c1", ["cv1", "canvas"]), ["reflect"]);
  assert.equal(writesPage(pageVerbs(cc, "G", "c1", ["cv1", "canvas"])), false);
  assert.equal(writesPage(pageVerbs(cc, "G", "c2", ["cv2", "canvas"])), true);
  assert.equal(writesPage(pageVerbs(cc, "G", "x", ["top"])), true, "the any-tree guid");
  assert.deepEqual(pageVerbs(cc, "G", "z", ["canvas"]), []);
  assert.equal(writesPage(pageVerbs({ "*|*": ["*"] }, "G", "anything", [])), true);
});
