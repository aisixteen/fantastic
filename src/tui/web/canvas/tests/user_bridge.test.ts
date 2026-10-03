import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { UserBridge } from "../src/user_bridge.ts";

/** The lib bridge the UserBridge wraps: it records what would go on the wire. */
function fakeInner(): { inner: never; sent: Array<{ target: string; payload: Record<string, unknown> }>; watched: string[] } {
  const sent: Array<{ target: string; payload: Record<string, unknown> }> = [];
  const watched: string[] = [];
  const inner = {
    onLifecycle: () => () => {},
    forward: async (target: string, payload: Record<string, unknown>) => {
      sent.push({ target, payload });
      return { ok: true };
    },
    forwardBinary: async (target: string, payload: Record<string, unknown>) => {
      sent.push({ target, payload });
      return { ok: true };
    },
    watchRemote: (src: string) => watched.push(src),
    unwatchRemote: () => {},
    subscribeState: () => () => {},
    close: () => {},
  };
  return { inner: inner as never, sent, watched };
}

test("a host call goes to `user` as forward, with the token when one is set", async () => {
  const { inner, sent } = fakeInner();
  const b = new UserBridge(new Kernel(), inner, { token: () => "T" });
  await b.forward("setup", { type: "store_key", secret: "k" } as Payload);
  assert.equal(sent[0]!.target, "user");
  assert.deepEqual(sent[0]!.payload, { type: "forward", target: "setup", payload: { type: "store_key", secret: "k" }, token: "T" });
  // no token: none sent
  const n = fakeInner();
  await new UserBridge(new Kernel(), n.inner, { token: () => null }).forward("x", { type: "v" } as Payload);
  assert.equal(n.sent[0]!.payload["token"], undefined);
});

test("a call to `user` itself carries the token on its own payload", async () => {
  const { inner, sent } = fakeInner();
  await new UserBridge(new Kernel(), inner, { token: () => "T" }).forward("user", { type: "reflect" } as Payload);
  assert.deepEqual(sent[0]!.payload, { type: "reflect", token: "T" });
});

test("a watch rides the leg's own frames, not `user`", () => {
  const { inner, sent, watched } = fakeInner();
  const b = new UserBridge(new Kernel(), inner, { token: () => null });
  b.watchRemote("exec_1");
  assert.deepEqual(watched, ["exec_1"]);
  assert.equal(sent.length, 0);
});
