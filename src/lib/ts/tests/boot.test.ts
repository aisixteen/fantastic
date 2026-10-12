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
  readonly frames: Envelope[] = [];
  private readonly records: Payload[];
  /** `<loader root>/bundles/*` as the host loader's `read_bundles` hands them. */
  readonly folders: Payload[];

  constructor(url: string, records: Payload[], folders: Payload[]) {
    this.url = url;
    this.records = records;
    this.folders = folders;
    setTimeout(() => this.onopen?.(undefined), 0);
  }
  send(data: string | ArrayBuffer): void {
    const env = decodeFrame(data);
    this.frames.push(env);
    if (env["type"] !== "call") return;
    this.calls.push(env);
    const p = env["payload"] as Payload;
    let reply: Json = { ok: true };
    if (p["type"] === "load_tree") reply = { version: 1, records: this.records as Json };
    if (p["type"] === "read_bundles") {
      const name = p["name"];
      const rows = this.folders.filter((f) => name === undefined || f["name"] === name);
      reply =
        name !== undefined && rows.length === 0
          ? { error: `read_bundles: no folder web/bundles/${String(name)}`, reason: "not_found" }
          : { bundles: rows as Json };
    }
    setTimeout(() => this.onmessage?.({ data: encodeFrame({ type: "reply", id: env["id"], data: reply }).data }), 0);
  }
  close(): void {}
}

function host(
  records: Payload[] = [],
  folders: Payload[] = [],
): { sockets: HostSocket[]; make: (url: string) => HostSocket } {
  const sockets: HostSocket[] = [];
  return {
    sockets,
    make: (url) => {
      const s = new HostSocket(url, records, folders);
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
  assert.deepEqual(
    h.sockets[0]!.calls.slice(0, 2).map((c) => (c["payload"] as Payload)["type"]),
    ["read_bundles", "load_tree"],
    "the folders load BEFORE the tree, through the loader",
  );
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
    { id: "mem", handler_module: "ram_store.ts", parent_id: "app" },
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


test("boot: a HYDRATED root that already has a guid says hello — the page stays addressable after a restart", async () => {
  // what a reopened canvas loads: its root WITH the guid minted on an earlier run
  const h = host([
    { id: "web_loader", handler_module: "kernel_state.tools" },
    { id: "app", handler_module: "app_root.ts", parent_id: "web_loader", guid: "p9" },
  ]);
  const { kernel } = await boot({
    origin: "ws://host",
    loader: "web_loader",
    makeSocket: h.make,
    register: (k) => k.registerBundle("app_root.ts", () => null),
  });
  assert.equal(kernel.guid, "p9");
  const deadline = Date.now() + 1000;
  const said = () => h.sockets.flatMap((s) => s.frames).filter((f) => f["type"] === "hello");
  while (said().length === 0 && Date.now() < deadline) await new Promise((r) => setTimeout(r, 5));
  assert.deepEqual(said()[0], { type: "hello", guid: "p9" });
});
