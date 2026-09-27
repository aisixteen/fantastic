# Fantastic ts kernel — `src/lib/ts`

The **ts kernel library**: a real Fantastic kernel — the same `send`/`reflect`/
`watch` protocol as the host runtimes — that runs in a browser (or under node) as
a **peer** of the host. It is the BASE ONLY: no page, no views, nothing mounted.
An **app** boots it, registers its own bundles, decides what an empty tree is
seeded with, and renders what it wants. The canvas is such an app — it lives in
`src/tui/web/canvas/` (its page, its views, its vendored three/xterm, its packed
zip and revive guide).

## What's in it

| path | what |
|---|---|
| `src/index.ts` | **the public entry** — everything an app imports, and `boot` |
| `src/kernel/` | the pure kernel core: agents, the two-sided call gate, the array-only batch send, the execution ledger, `ps`/`cancel`, reflect, the lifecycle (`kernel_start`/`kernel_stop`). No DOM — `check:pure` compiles it without one |
| `src/transport/` | the WS bridge to the host (`WsBridge`) and its frame codec |
| `src/bundles/loader/proxy_loader.ts` | persists this kernel's tree THROUGH the host's loader, and hydrates it back |
| `src/bundles/memory/ram_store.ts` | the memory store (`kernel_start` needs the root to name one) |
| `src/bundles/ai/` | the AI backend — an LLM agent that runs in this kernel (raw `<call>` tag grammar, memory-store conversation, compaction) |
| `src/bundles/host.ts` | `Host` — a view's call/watch surface over the kernel |
| `tools/esbuild/` | the vendored, sha256-pinned esbuild that `scripts/pack.sh` packs an app's page with |

## The public API (`src/index.ts`)

`Kernel`, `Agent`, `WsBridge`, `ProxyLoader`, `Host`, `registerRamStore`,
`registerAIBackend` / `makeAIBackend` / `OpenAIProvider`, `str`, the types an app
needs (`Json`, `Payload`, `Handler`, `AgentRecord`, `Bridge`, `Item`, `Outcome`,
`AIRecord`, …) and:

```ts
boot({ origin, loader, register, seed?, makeSocket? }) → Promise<{ kernel, loader }>
```

1. creates the `Kernel` and registers the library's own bundles (the memory store;
   the AI backend only if the app registers it — `registerAIBackend`),
2. calls `register(kernel)` — the app registers ITS bundles and readmes
   (`kernel.registerBundle`, `kernel.setBundleReadme`) before anything loads,
3. dials the host with a `WsBridge` whose control endpoint is `loader`, and names
   `loader` as the root's `persistence.provider`,
4. `loadTree()` from that loader → `kernel.load(records)` when there are any; on an
   EMPTY namespace it calls the app's `seed(kernel)` if given (an empty tree
   stays empty otherwise — the library seeds nothing),
5. `loader.start()` — every local change persists back through the host,
6. returns the handles. Mounting is the app's.

```ts
import { Agent, boot } from "../../../lib/ts/src/index.ts";
const { kernel } = await boot({
  origin: "ws://127.0.0.1:8888",
  loader: "web_loader",
  register: (k) => k.registerBundle("my_view.ts", () => null),
  seed: (k) => k.setRoot(new Agent({ id: "app", handlerModule: "my_view.ts" })),
});
```

An app imports from `src/index.ts`, never from the modules behind it (the canvas
keeps that to one file, its `src/lib.ts`).

## Packing an app's page

The library has no page, so `scripts/pack.sh` has no default: the app names its
entry, its output zip and its readme (the revive guide that travels inside the
zip), plus any esbuild flags its page needs:

```sh
sh src/lib/ts/scripts/pack.sh <entry.ts> <out.zip> <readme.md> [esbuild flags …]
```

It verifies the vendored esbuild against `tools/esbuild/SHA256SUMS`, bundles the
entry (and everything it imports — this library included) into ONE
`bundle.min.js` + map, stamps the bundle's sha256 into the readme copy, and zips
the three. No npm, no node. The canvas: `sh src/tui/web/canvas/scripts/pack.sh`
→ `src/tui/web/canvas/dist/canvas.zip`.

## Tests

```sh
node --test "tests/**/*.test.ts"                     # no build step — node strips the types
npx --no-install tsc --noEmit -p tsconfig.json       # typecheck
npx --no-install tsc --noEmit -p src/kernel/tsconfig.json   # check:pure — the core has no DOM
```

The contract corpus (`contract/agentic`, `contract/conformance`) is replayed here
by `tests/contract_corpus.test.ts` and `tests/c21_conformance.test.ts`.

## Host contract (weak binding)

- Every agent answers `{type:"reflect"}` — the universal discovery verb.
- This kernel federates to a host **loader** alias over the WS bridge and persists
  every local change back through it. No per-page session id.
- Direction is **this kernel → host**: the host never references it. Same rule as
  `ws_bridge` — addressed by URL + id only, no shared types.

---

*Part of **Aisixteen Fantastic** — licensed **AGPL-3.0-or-later**
([`../LICENSE`](../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg.
7,238,635) are trademarks of AISixteen; the license covers the code only, not the
marks — forks must rename. See the [root README](../README.md#license--brand).*
