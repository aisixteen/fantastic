import { test } from "node:test";
import assert from "node:assert/strict";
import { Kernel } from "../src/kernel/kernel.ts";
import { Agent } from "../src/kernel/agent.ts";
import { WsBridge } from "../src/transport/bridge.ts";
import type { WebSocketLike } from "../src/transport/bridge.ts";
import { encodeFrame, decodeFrame } from "../src/transport/frame.ts";
import type { Envelope } from "../src/transport/frame.ts";

// ─── frame codec ──────────────────────────────────────────────────────────

test("frame codec round-trips a text frame", () => {
  const env = { type: "call", target: "fs_loader", payload: { type: "reflect" }, id: "1" };
  const enc = encodeFrame(env);
  assert.equal(enc.binary, false);
  assert.deepEqual(decodeFrame(enc.data), env);
});

test("frame codec round-trips a binary frame (4-byte BE head + body)", () => {
  const bytes = new Uint8Array([0, 1, 2, 250, 255]);
  const enc = encodeFrame({
    type: "emit",
    target: "pty1",
    payload: { type: "paste_image", image: bytes },
  });
  assert.equal(enc.binary, true);
  const buf = enc.data as ArrayBuffer;
  // header length is a big-endian uint32 prefix
  const headLen = new DataView(buf).getUint32(0, false);
  assert.ok(headLen > 0 && headLen < buf.byteLength);
  const decoded = decodeFrame(buf) as {
    payload: { image: Uint8Array; type: string };
  };
  assert.equal(decoded.payload.type, "paste_image");
  assert.ok(decoded.payload.image instanceof Uint8Array);
  assert.deepEqual([...decoded.payload.image], [0, 1, 2, 250, 255]);
});

// ─── fake socket harness ────────────────────────────────────────────────────

class FakeSocket implements WebSocketLike {
  binaryType = "blob";
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  readonly sent: Envelope[] = [];
  closed = false;
  readonly url: string;

  constructor(url: string) {
    this.url = url;
    // open on a later tick — after Conn assigns onopen and awaits ready
    setTimeout(() => this.onopen?.(undefined), 0);
  }
  send(data: string | ArrayBuffer): void {
    this.sent.push(decodeFrame(data));
  }
  close(): void {
    this.closed = true;
  }
  /** server→client push */
  deliver(env: Envelope): void {
    const f = encodeFrame(env);
    this.onmessage?.({ data: f.data });
  }
  /** simulate the socket dropping */
  drop(): void {
    this.onclose?.(undefined);
  }
  lastCall(): Envelope | undefined {
    return [...this.sent].reverse().find((f) => f["type"] === "call");
  }
}

function fakeFactory() {
  const sockets: FakeSocket[] = [];
  return {
    factory: (url: string) => {
      const s = new FakeSocket(url);
      sockets.push(s);
      return s;
    },
    forEndpoint(ep: string): FakeSocket[] {
      return sockets.filter((s) => s.url.endsWith(`/${ep}/ws`));
    },
  };
}

async function until(fn: () => boolean, ms = 1000): Promise<void> {
  const start = Date.now();
  while (!fn()) {
    if (Date.now() - start > ms) throw new Error("until: timeout");
    await new Promise((r) => setTimeout(r, 1));
  }
}

function kernelWithRoot(): Kernel {
  const k = new Kernel();
  k.setRoot(new Agent({ id: "fs_loader" }));
  return k;
}

// ─── WsBridge ───────────────────────────────────────────────────────────────

test("forward sends a call on the control socket and resolves the reply by id", async () => {
  const fk = fakeFactory();
  const k = kernelWithRoot();
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: "fs_loader",
    makeSocket: fk.factory,
  });

  const p = bridge.forward("file_x", { type: "list", path: "." });
  await until(() => (fk.forEndpoint("fs_loader")[0]?.lastCall() ?? undefined) !== undefined);
  const sock = fk.forEndpoint("fs_loader")[0]!;
  const call = sock.lastCall()!;
  assert.equal(call["target"], "file_x");
  assert.deepEqual(call["payload"], { type: "list", path: "." });

  sock.deliver({ type: "reply", id: call["id"], data: { entries: ["a", "b"] } });
  assert.deepEqual(await p, { entries: ["a", "b"] });
  bridge.close();
});

test("an error frame rejects the matching call", async () => {
  const fk = fakeFactory();
  const k = kernelWithRoot();
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: "fs_loader",
    makeSocket: fk.factory,
  });
  const p = bridge.forward("fs_loader", { type: "nope" });
  await until(() => (fk.forEndpoint("fs_loader")[0]?.lastCall() ?? undefined) !== undefined);
  const sock = fk.forEndpoint("fs_loader")[0]!;
  sock.deliver({ type: "error", id: sock.lastCall()!["id"], error: "boom" });
  await assert.rejects(p, /boom/);
  bridge.close();
});

test("watchRemote dials /<src>/ws; its events route to emit(src) only", async () => {
  const fk = fakeFactory();
  const k = kernelWithRoot();
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: "fs_loader",
    makeSocket: fk.factory,
  });

  const pty1: Envelope[] = [];
  const pty2: Envelope[] = [];
  k.onInbox("pty1", (pl) => pty1.push(pl));
  k.onInbox("pty2", (pl) => pty2.push(pl));
  // a local view-agent watches each backend
  k.register(new Agent({ id: "view1", parentId: "fs_loader" }));
  k.register(new Agent({ id: "view2", parentId: "fs_loader" }));
  k.onInbox("view1", (pl) => pty1.push(pl));
  k.onInbox("view2", (pl) => pty2.push(pl));
  k.watch("pty1", "view1"); // remote → bridge.watchRemote("pty1")
  k.watch("pty2", "view2");

  await until(() => fk.forEndpoint("pty1").length > 0 && fk.forEndpoint("pty2").length > 0);
  fk.forEndpoint("pty1")[0]!.deliver({
    type: "event",
    payload: { type: "output", text: "hello" },
  });

  await until(() => pty1.length > 0);
  // pty1's stream reached pty1 watchers; pty2 untouched (per-endpoint isolation)
  assert.ok(pty1.some((p) => p["text"] === "hello"));
  assert.equal(pty2.length, 0);
  bridge.close();
});

test("a pending call rejects when its socket drops", async () => {
  const fk = fakeFactory();
  const k = kernelWithRoot();
  const bridge = new WsBridge(k, {
    origin: "ws://host",
    controlEndpoint: "fs_loader",
    makeSocket: fk.factory,
    maxReconnectDelay: 5,
  });
  const p = bridge.forward("fs_loader", { type: "slow" });
  await until(() => (fk.forEndpoint("fs_loader")[0]?.lastCall() ?? undefined) !== undefined);
  fk.forEndpoint("fs_loader")[0]!.drop();
  await assert.rejects(p, /disconnected/);
  bridge.close();
});
