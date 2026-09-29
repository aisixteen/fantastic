# Serving a view package — the weak-binding recipe

**The host must not know this package exists.** Client↔server is a *weak
binding* (same rule as `ws_bridge`: addressed by URL + path only, no shared
types). The Python `web` bundle is a generic static host; it serves whatever
`file_bridge` agents an operator (or an LLM following this recipe) wires
up. Nothing in `python/` references `ts/`. That decoupling is the point — and it
means **the same recipe serves any view package** (this TS kernel, a different
framework's `dist/`, a plain HTML app): point a `file_bridge` agent at its build output
and let it serve a caller-supplied mount page from there too.

One duck-typed route in `python/bundled_agents/web/host/src/web/app.py` does all the work:

| route | what answers it |
|---|---|
| `GET /{id}/file/{path}` | any agent answering `read_stream` — a `file_bridge` agent is a static file server over what its `allow` list exposes |

## The page lives in the APP, not here

This library has no page. An app packs its own — the canvas is
`src/tui/web/canvas/` — with the library's vendored esbuild:

```bash
sh src/tui/web/canvas/scripts/pack.sh   # → src/tui/web/canvas/dist/canvas.zip
# any app:  sh src/lib/ts/scripts/pack.sh <entry.ts> <out.zip> <readme.md> [esbuild flags …]
```

The zip is `readme.md` (the app's revive guide) + ONE inlined `bundle.min.js`
(the app, this library, the app's vendors) + its map. There is **no npm step**
and no import map: esbuild inlines everything, so the mount page is a single
`<script type="module">` tag served by a generic `file_bridge` agent. The canvas
app's `readme.md` is the full revive recipe (inspect the zip without unpacking,
pull `bundle.min.js`, open a `file_bridge` leg on it, provide the one-tag page).

An app's page boots this library with `boot` (`src/index.ts`): it dials the
host's loader by id (e.g. `web_loader` — a `kernel_state` the operator created at
`.fantastic/web`), hydrates the app's OWN tree from there, seeds an empty
namespace the way the app decides, and persists every change back through that
loader. The host never learns what the page is; it just serves bytes.

## Dev path (scattered ESM modules)

`npm run build` (the one pinned dev dep, `typescript`) emits this library as ES
modules into `dist/`. A page that imports those modules instead of a packed
bundle is served the same way — a `file_bridge` agent rooted at a copy of the
build output inside the kernel's running dir (the bridge is SEALED by default and
its root CLAMPED to the running dir):

```bash
cp -R /PATH/TO/src/lib/ts/dist ./ts_dist_src
fantastic kernel_state create_agent handler_module=file_bridge.tools id=ts_dist \
  root=ts_dist_src allow='["."]' ingress_rule=allow_all_ingress
# → /ts_dist/file/index.js is the public entry; relative imports resolve under it
```

The build emits no page — the app provides one.

## Same trick, other views

Nothing above is TS-specific. To serve view package *X*:

1. build it to some `X/dist`,
2. `file_bridge` agent rooted at `X/dist` → all of `X`'s assets are served,
3. provide a mount page (a tiny `<script type="module">` host page) and open it
   over `/<file_id>/file/<path>`; it loads `X`'s entry from the same `/file/`
   root.

The host stays a dumb static server; each view package is self-contained and
swappable. Weak binding is the beauty.

## The native sibling — `cli_loader`

`web_loader` is one **sub-kernel** anchor. The `fantastic` CLI writes another of the
*same shape* — `cli_loader = kernel_state @ .fantastic/cli` (a literal sibling of
`.fantastic/web`) — but realizes it differently: the CLI is in-proc with disk, so it
**native-writes** the anchor + namespace directly (no `proxy_loader` over WS, no
served bytes), because the WS RPC surface exists only for a sandboxed *browser*. Same
config shape, different realization: `web` is the **passive/served** sub-kernel,
`cli` the **initiator** one. Sub-kernels are a generic construct (web is not a special
case) — the full model + roles live in the CLI contract
[`src/cli/design/25_subkernels.md`](../../cli/design/25_subkernels.md).

---

*Part of **Aisixteen Fantastic** — licensed **AGPL-3.0-or-later** ([`../LICENSE`](../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg. 7,238,635) are trademarks of AISixteen; the license covers the code only, not the marks — forks must rename. See the [root README](../README.md#license--brand).*
