// The library's PUBLIC ENTRY (src/index.ts): `boot` creates the kernel,
// registers the library's own bundles, hands the kernel to the app's
// `register`, dials the host with the loader as the control endpoint, hydrates
// the tree from that loader and starts persisting through it. It seeds and
// mounts NOTHING — an empty tree stays empty for the app to decide.

import { test } from "node:test";
import assert from "node:assert/strict";
import { Agent, boot } from "../src/index.ts";
import type { Json, Payload, WebSocketLike } from "../src/index.ts";
import { decodeFrame, encodeFrame } from "../src/transport/frame.ts";
import type { Envelope } from "../src/transport/frame.ts";

/** A host that answers the loader's verbs on the control socket. */
class HostSocket implements WebSocketLike {
  binaryType = "blob";
  onopen: ((ev: unknown) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  readonly url: string;
  readonly calls: Envelope[] = [];
  private readonly records: Payload[];

  constructor(url: string, records: Payload[]) {
    this.url = url;
    this.records = records;
    setTimeout(() => this.onopen?.(undefined), 0);
  }
  send(data: string | ArrayBuffer): void {
    const env = decodeFrame(data);
    if (env["type"] !== "call") return;
    this.calls.push(env);
    const p = env["payload"] as Payload;
    let reply: Json = { ok: true };
    if (p["type"] === "load_tree") reply = { version: 1, records: this.records as Json };
    setTimeout(() => this.onmessage?.({ data: encodeFrame({ type: "reply", id: env["id"], data: reply }).data }), 0);
  }
  close(): void {}
}

function host(records: Payload[] = []): { sockets: HostSocket[]; make: (url: string) => HostSocket } {
  const sockets: HostSocket[] = [];
  return {
    sockets,
    make: (url) => {
      const s = new HostSocket(url, records);
      sockets.push(s);
      return s;
    },
  };
}

test("boot: an EMPTY tree is the app's to seed — and the seed, root included, persists", async () => {
  const h = host();
  const order: string[] = [];
  const { kernel, loader } = await boot({
    origin: "ws://host",
    loader: "web_loader",
    makeSocket: h.make,
    register: (k) => {
      order.push(`register (ram_store already: ${k.availableBundles().some((b) => b.handler_module === "ram_store.ts")})`);
      k.registerBundle("app_root.ts", () => null);
    },
    seed: (k) => {
      order.push("seed");
      k.setRoot(new Agent({ id: "app", handlerModule: "app_root.ts" }));
    },
  });
  assert.deepEqual(order, ["register (ram_store already: true)", "seed"]);
  assert.equal(kernel.rootId, "app");
  assert.equal(kernel.persistenceProvider, "web_loader");
  assert.equal(h.sockets[0]!.url, "ws://host/web_loader/ws", "the loader is the control endpoint");
  assert.equal((h.sockets[0]!.calls[0]!["payload"] as Payload)["type"], "load_tree");
  await loader.flush();
  const persisted = h.sockets[0]!.calls.filter((c) => (c["payload"] as Payload)["type"] === "persist_record");
  assert.equal(persisted.length, 1);
  assert.equal(((persisted[0]!["payload"] as Payload)["record"] as Payload)["id"], "app");
  await loader.stop();
});

test("boot: a persisted tree is hydrated, and the app's bundles run its records", async () => {
  const h = host([
    { id: "web_loader", handler_module: "kernel_state.tools" }, // the host anchor
    { id: "app", handler_module: "app_root.ts", parent_id: "web_loader" },
    { id: "note", handler_module: "note.ts", parent_id: "app", text: "hi" },
    { id: "mem", handler_module: "ram_store.ts", parent_id: "app", operator: true },
  ]);
  const { kernel, loader } = await boot({
    origin: "ws://host",
    loader: "web_loader",
    makeSocket: h.make,
    register: (k) => {
      k.registerBundle("app_root.ts", () => null);
      k.registerBundle("note.ts", (id, _p, kk) => ({ text: String(kk.get(id)?.meta["text"] ?? "") }));
    },
  });
  assert.equal(kernel.rootId, "app", "the anchor is dropped and the JS root re-rooted");
  assert.deepEqual([...kernel.agents.keys()].sort(), ["app", "mem", "note"]);
  const [r] = await kernel.send([{ target: "note", payload: { type: "read" } }]);
  assert.deepEqual(r!.result, { text: "hi" });
  assert.equal(kernel.get("mem")?.handler !== null, true, "the library's memory store runs its record");
  await loader.stop();
});

test("boot: without a `seed`, an empty tree stays empty — the library seeds nothing", async () => {
  const h = host();
  let seeded = false;
  const { kernel, loader } = await boot({ origin: "ws://host", loader: "web_loader", makeSocket: h.make, register: () => undefined });
  assert.equal(kernel.rootId, null);
  assert.equal(kernel.agents.size, 0);
  const h2 = host([{ id: "web_loader" }, { id: "app", parent_id: "web_loader" }]);
  const b2 = await boot({ origin: "ws://host", loader: "web_loader", makeSocket: h2.make, register: () => undefined, seed: () => { seeded = true; } });
  assert.equal(seeded, false, "a non-empty tree is never seeded");
  await loader.stop();
  await b2.loader.stop();
});
