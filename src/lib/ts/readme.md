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
| `src/bundles/host.ts` | `Host` — a view's call/watch surface over the kernel |
| `tools/esbuild/` | the vendored, sha256-pinned esbuild that `scripts/pack.sh` packs an app's page with |

## The public API (`src/index.ts`)

`Kernel`, `Agent`, `WsBridge`, `ProxyLoader`, `Host`, `registerRamStore`, `str`,
the types an app needs (`Json`, `Payload`, `Handler`, `AgentRecord`, `Bridge`,
`Item`, `Outcome`, …) and:

```ts
boot({ origin, loader, register, seed?, makeSocket? }) → Promise<{ kernel, loader }>
```

1. creates the `Kernel` and registers the library's own bundle (the memory store),
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

**Deleting a page agent.** Tree CRUD verbs (`create_agent`/`update_agent`/
`delete_agent`) belong to the host root, so a page deletes its own agents with
`kernel.remove(id)` — and `delete_lock` is enforced there, as the host kernels'
`delete_agent` enforces it: if the agent OR any descendant carries a truthy
`delete_lock`, nothing is removed and the answer is
`{error, reason:"locked", blocked_by, id, hint}`. Clear it with
`kernel.updateMeta(id, {delete_lock: false})`, then remove again. Success is
`{deleted: true, id}`; an unknown id is `unknown_agent` (with `deleted: false`);
the ROOT is never removed (`bad_arg`, `deleted: false`) — it answers the kernel
verbs, and its removal would forget this kernel's whole namespace on the host.

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
- This kernel federates to a host **loader** (by id) over the WS bridge and persists
  every local change back through it. No per-page session id.
- The host never references it by TYPE — addressed by URL + id only, no shared
  types, the same rule as `ws_bridge`. It can CALL it once the page has said hello
  (see below): by guid and agent id, learned through reflect.

## Called from the host (cross-kernel addressing)

The host can call this kernel's agents directly: it addresses one as
`<this kernel's guid>|<agent id>` with an ordinary `send` (and learns what is here
by `reflect` to `<guid>|kernel`). The bridge's control socket makes that possible:
it says `{type:"hello", guid}` on every (re)connect and again when the root first
mints the guid, and answers each inbound `{type:"call", id, target, payload,
caller, secret?}` with `{type:"reply", id, data}` — the call's outcome, a refusal included.

Such a call runs under a FOREIGN origin (`caller`, the host agent's
`<host guid>|<agent>`): the target's `callable_by` is the whole check here — name
the caller, or `<host guid>|*` for every agent of that host — and only this
kernel's own agents are reachable. A caller claiming this kernel's own guid is
refused. A memory store here keys what such a caller writes by its qualified
address, verbatim. Locally, `<this kernel's guid>|<agent id>` names the local
agent too (`<guid>|kernel` the root).

**The tabs of one identity are one principal** (owner, 2026-10-03). Every tab of
every browser hydrates the same tree and so says hello with the SAME guid — there is
no per-tab guid. The host holds each such connection as one more live route of that
guid: a host call to `<guid>|<agent>` reaches every tab and settles with the first
reply (the others are dropped); a closed tab leaves the guid reachable through the
rest.

**Secret marks cross the bridge.** A batch item's `secret` marks
(`src/kernel/secret.ts`) — dotted strings (`payload.<field…>` / `result[.<field…>]`)
and/or arrays of steps (`["payload", "a.b"]`: a string step a literal key, an int
step a list index) — ride its call frame as `secret`, in both directions: a
host-bound send puts them on the frame it sends as given (only when the item
marked some), and a host's `call` frame is dispatched here with its `secret`
verbatim — so whichever kernel receives the call records `<secret>` (call/settle
rows, ledger, memory store, inbox and watcher mirrors) exactly as the caller does,
and a malformed mark is the receiving kernel's own `bad_arg` refusal, before
dispatch. The reply carries the real result back.

**A JOIN of a secret-marked execution** (audit F1, 2026-10-03): the REAL outcome
goes to the execution's ORIGINAL caller only — the identity that opened it (its
`caller`: the sending agent; the operator, null; a caller on another kernel by its
`<guid>|<agent>`). Any other joiner — operator included — gets it AS SHOWN
(`shownOutcome`, what the `ledger` reads back): the gate re-checks the joiner on
the original VERB (a foreign joiner on the target's `callable_by`, as at launch),
but a handler that answers by WHO asks never runs for a joiner. An unmarked
execution joins unchanged.

**A handler reads its call's marks** as `kernel.currentSecret` on the view it is
handed (python `kernel.current_secret()`): the marks as the caller gave them —
a local item's, or a host call frame's — frozen, `[]` when none were marked.
Stamped per dispatch like `currentCaller`, so the handler's own sends never
inherit them. A bundle that must only be called WITH a mark refuses an unmarked
call by it.

**A view marks its calls** through `Host`: `call(target, payload, secret?)` makes
the marks the item's `secret` (masked in this kernel's records, on the frame for a
host target); `callHost(target, payload, secret?)` and
`callBinary(target, payload, secret?)` — and `Kernel.callHost` /
`Kernel.forwardBinary` under them — bypass the batch, so this kernel keeps no
record of them, and put the marks on the call frame. Each validates the marks as
a batch item's: a malformed one is `bad_arg`, before the egress gate and before a
byte leaves.

---

*Part of **Aisixteen Fantastic** — licensed **AGPL-3.0-or-later**
([`../LICENSE`](../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg.
7,238,635) are trademarks of AISixteen; the license covers the code only, not the
marks — forks must rename. See the [root README](../README.md#license--brand).*
