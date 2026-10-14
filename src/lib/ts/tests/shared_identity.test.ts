// THE TABS OF ONE IDENTITY ARE ONE PRINCIPAL: every tab of
// every browser hydrates the one tree and says `hello` with ITS guid — there is
// no per-tab guid. The host holds each tab's connection as one more live route
// of that guid: its call to `<guid>|x` goes to every tab and settles with the
// first reply (the host's peer table — python `tests/kernel/test_peer_addressing.py`,
// `web_ws/tests/test_peer_calls.py`). Here, the page side: each tab says hello
// with the tree guid, answers the call the host makes on ITS socket, and a
// closed tab leaves the other answering.
//
// The socket is the only stand-in; the kernel and the bridge are the real ones.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { WsBridge } from "../src/transport/bridge.ts";
import type { WebSocketLike } from "../src/transport/bridge.ts";
import { encodeFrame, decodeFrame } from "../src/transport/frame.ts";
import type { Envelope } from "../src/transport/frame.ts";

const ROOT = "kernel_state";
const TREE = "tree-g";

class FakeSocket implements WebSocketLike {
  binaryType = "blob";
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  readonly sent: Envelope[] = [];
  readonly url: string;
  constructor(url: string) {
    this.url = url;
    setTimeout(() => this.onopen?.(undefined), 0);
  }
  send(data: string | ArrayBuffer): void {
    this.sent.push(decodeFrame(data));
  }
  close(): void {}
  deliver(env: Envelope): void {
    this.onmessage?.({ data: encodeFrame(env).data });
  }
  frames(type: string): Envelope[] {
    return this.sent.filter((f) => f["type"] === type);
  }
  reply(id: string): Envelope | undefined {
    return this.sent.find((f) => f["type"] === "reply" && f["id"] === id);
  }
}

async function until(fn: () => boolean, ms = 3000): Promise<void> {
  const start = Date.now();
  while (!fn()) {
    if (Date.now() - start > ms) throw new Error("until: timeout");
    await new Promise((r) => setTimeout(r, 1));
  }
}

/** One TAB: a kernel hydrated from the one tree (root guid `TREE`) with a local
 *  agent `x` that answers with the tab's name, and its own bridge. */
function tab(name: string) {
  const socks: FakeSocket[] = [];
  const k = new Kernel();
  k.setRoot(new Agent({ id: ROOT, meta: { guid: TREE } }));
  k.register(
    new Agent({
      id: "x",
      parentId: ROOT,
      handler: (_id, payload) => ({ tab: name, type: payload["type"] ?? null }),
    }),
  );
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: ROOT,
    makeSocket: (url) => {
      const s = new FakeSocket(url);
      socks.push(s);
      return s;
    },
  });
  bridge.announce();
  const sock = (): FakeSocket => socks[0]!;
  return { k, bridge, sock };
}

test("two tabs of one tree say hello with the tree guid — no per-tab guid", async () => {
  const a = tab("a");
  const b = tab("b");
  await until(() => (a.sock()?.frames("hello").length ?? 0) > 0 && (b.sock()?.frames("hello").length ?? 0) > 0);
  assert.deepEqual(a.sock().frames("hello"), [{ type: "hello", guid: TREE }]);
  assert.deepEqual(b.sock().frames("hello"), [{ type: "hello", guid: TREE }]);
  // a guid change on the tree re-announces with the tree's new guid
  a.k.updateMeta(ROOT, { guid: "tree-g2" });
  await until(() => a.sock().frames("hello").length === 2);
  assert.deepEqual(a.sock().frames("hello")[1], { type: "hello", guid: "tree-g2" });
  a.bridge.close();
  b.bridge.close();
});

test("the host's call reaches each tab on its own socket; a closed tab leaves the other answering", async () => {
  const a = tab("a");
  const b = tab("b");
  await until(() => (a.sock()?.frames("hello").length ?? 0) > 0 && (b.sock()?.frames("hello").length ?? 0) > 0);
  // the host's fan-out: one call frame per live route of the guid
  const call = { type: "call", id: "h1", target: "x", payload: { type: "ping" } };
  a.sock().deliver(call);
  b.sock().deliver(call);
  await until(() => a.sock().reply("h1") !== undefined && b.sock().reply("h1") !== undefined);
  assert.deepEqual(a.sock().reply("h1")!["data"], { tab: "a", type: "ping" });
  assert.deepEqual(b.sock().reply("h1")!["data"], { tab: "b", type: "ping" });
  // tab a closes; the identity's other tab still answers
  a.bridge.close();
  b.sock().deliver({ ...call, id: "h2" });
  await until(() => b.sock().reply("h2") !== undefined);
  assert.deepEqual(b.sock().reply("h2")!["data"], { tab: "b", type: "ping" });
  b.bridge.close();
});

test("the tree guid is the tab's only own form: local resolution, a host call addressed by it", async () => {
  const { k, bridge, sock } = tab("a");
  assert.equal(k.ownQualified(`${TREE}|x`), "x");
  assert.equal(k.ownQualified("tab-a|x"), null);
  await until(() => (sock()?.frames("hello").length ?? 0) > 0);
  // the host addresses the agent by the tree guid; the call names no caller
  sock().deliver({ type: "call", id: "h1", target: `${TREE}|x`, payload: { type: "ping" } });
  await until(() => sock().reply("h1") !== undefined);
  assert.deepEqual(sock().reply("h1")!["data"], { tab: "a", type: "ping" });
  // a per-tab qualifier names nothing here
  sock().deliver({ type: "call", id: "h2", target: "tab-a|x", payload: { type: "ping" } });
  await until(() => sock().reply("h2") !== undefined);
  assert.equal((sock().reply("h2")!["data"] as Record<string, unknown>)["reason"], "unknown_agent");
  bridge.close();
});
