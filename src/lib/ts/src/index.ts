// THE PUBLIC ENTRY of the ts kernel library — everything an app needs, and the
// one way to boot it.
//
// The library is the BASE ONLY: the kernel, the WS bridge to the host, the
// loader that persists this kernel's tree through the host, the memory store,
// the AI backend. It has no page, no views, no seeding and nothing to mount — an
// APP (the canvas, a test harness, any other page) registers its own bundles,
// decides what an empty tree is seeded with, and mounts what it renders:
//
//     import { boot, Agent } from "<path>/src/lib/ts/src/index.ts";
//     const { kernel, loader } = await boot({
//       origin: "ws://127.0.0.1:8888",
//       loader: "web_loader",
//       register: (k) => { k.registerBundle("my_view.ts", () => null); },
//       seed: (k) => k.setRoot(new Agent({ id: "app", handlerModule: "my_view.ts" })),
//     });
//
// An app imports from HERE, not from the modules behind it.

import { Kernel } from "./kernel/kernel.ts";
import { WsBridge } from "./transport/bridge.ts";
import type { SocketFactory } from "./transport/bridge.ts";
import { ProxyLoader } from "./bundles/loader/proxy_loader.ts";
import { registerRamStore } from "./bundles/memory/ram_store.ts";

export { Kernel } from "./kernel/kernel.ts";
export type { Bridge } from "./kernel/kernel.ts";
export { Agent } from "./kernel/agent.ts";
export type { AgentInit, Handler } from "./kernel/agent.ts";
export { str } from "./kernel/json.ts";
export type { Json, Payload } from "./kernel/json.ts";
export type { AgentRecord, StateEvent } from "./kernel/state.ts";
export type { Item, Outcome, Status } from "./kernel/outcome.ts";
export { WsBridge } from "./transport/bridge.ts";
export type { BridgeOptions, SocketFactory, WebSocketLike } from "./transport/bridge.ts";
export { ProxyLoader } from "./bundles/loader/proxy_loader.ts";
export type { ProxyLoaderOptions } from "./bundles/loader/proxy_loader.ts";
export { Host } from "./bundles/host.ts";
export { registerRamStore, RAM_STORE_README } from "./bundles/memory/ram_store.ts";
export { registerAIBackend, makeAIBackend, OpenAIProvider, AI_README, SEND_TIMEOUT } from "./bundles/ai/ai_backend.ts";
export type { AIRecord, AIBackendOptions } from "./bundles/ai/ai_backend.ts";

export interface BootOptions {
  /** The host's WS origin, e.g. `ws://127.0.0.1:8888` — never guessed. */
  origin: string;
  /** The host-side loader this kernel's tree persists THROUGH (a `kernel_state`
   *  the operator created, e.g. alias `web_loader`). It is also the bridge's
   *  control endpoint. */
  loader: string;
  /** The app's turn: register its bundles (`kernel.registerBundle`) and their
   *  readmes (`kernel.setBundleReadme`) BEFORE the tree is loaded, so a
   *  persisted record of one of them runs here instead of weak-loading. */
  register: (kernel: Kernel) => void;
  /** The app's decision for an EMPTY tree (a fresh host namespace): called only
   *  when the loader held no records, BEFORE the loader starts watching — so
   *  what it seeds is persisted by the loader's initial write, the root
   *  included (a root publishes no `added` event of its own). Absent, an empty
   *  tree stays empty. */
  seed?: (kernel: Kernel) => void;
  /** The WebSocket factory — absent, the platform's `WebSocket`. */
  makeSocket?: SocketFactory;
}

export interface Booted {
  kernel: Kernel;
  loader: ProxyLoader;
}

/**
 * Boot the kernel as a PEER of the host: create it, register the library's own
 * bundles (the memory store — the page never starts the kernel itself; an
 * unstarted kernel keeps its execution rows in RAM, and `kernel_start` is the
 * operator's act), let the app register its own, dial the host with the loader
 * as the control endpoint, hydrate this kernel's OWN tree from that loader, and
 * start persisting every local change back through it.
 *
 * What an EMPTY tree is seeded with is the app's decision (`seed`), and so is
 * what gets mounted — the library seeds and mounts nothing.
 */
export async function boot(opts: BootOptions): Promise<Booted> {
  const kernel = new Kernel();
  registerRamStore(kernel);
  opts.register(kernel);
  new WsBridge(kernel, {
    origin: opts.origin,
    controlEndpoint: opts.loader,
    ...(opts.makeSocket !== undefined ? { makeSocket: opts.makeSocket } : {}),
  });
  // The ONE auto-added agent of this runtime: its loader, mirroring the host root
  // loader. Everything else is the app's, explicitly.
  const loader = new ProxyLoader(kernel, opts.loader);
  // `persistence:{provider}` on the root reflect: records persist THROUGH it.
  kernel.persistenceProvider = opts.loader;
  const records = await loader.loadTree();
  if (records.length > 0) kernel.load(records);
  else opts.seed?.(kernel);
  // Watch the local tree → persist / forget over the bridge; the first write is
  // the whole current tree (a fresh seed included).
  loader.start();
  return { kernel, loader };
}
