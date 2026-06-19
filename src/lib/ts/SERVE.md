# Serving a view package — the weak-binding recipe

**The host must not know this package exists.** Client↔server is a *weak
binding* (same rule as `ws_bridge`: addressed by URL + path only, no shared
types). The Python `web` bundle is a generic static host; it serves whatever
`file_bridge` agents an operator (or an LLM following this recipe) wires
up. Nothing in `python/` references `ts/`. That decoupling is the point — and it
means **the same recipe serves any view package** (this TS kernel, a different
framework's `dist/`, a plain HTML app): point a `file_bridge` agent at its build output
and let it serve a caller-supplied mount page from there too.

One duck-typed alias in `python/bundled_agents/web/host/src/web/app.py` does all the work:

| alias | route | what answers it |
|---|---|---|
| `read{path}` | `GET /{id}/file/{path}` | a `file_bridge` agent → static file server rooted at a dir |

## Distribution — the sovereign artifact

The canonical production artifact is **`ts/dist/js_kernel.zip`** — a single
self-describing zip that contains the full kernel in ONE inlined bundle
(`bundle.min.js` + its `.map`) and the `readme.md` that describes how to revive
it. Build it with:

```bash
cd ts && sh scripts/pack.sh      # → ts/dist/js_kernel.zip
```

The zip's only pinned dependency is the **vendored esbuild Go binary** (checked
into `ts/src/vendor/` — see `ts/readme.md` and `ts/tools/esbuild/README.md`).
There is **no npm step** and no import map, because every vendor (`three`,
`@xterm/*`, `xterm.css`) is inlined by esbuild into `bundle.min.js`. Operators
pull the zip on demand and serve `bundle.min.js` through a generic `file_bridge` agent
— the mount page is a single `<script type="module">` tag, no import map, no
`<link rel="stylesheet">` (the xterm CSS is injected at runtime by a shim inside
the bundle). See [`ts/readme.md`](readme.md) for the full revive recipe and
integrity-check instructions.

## Dev path (scattered ESM modules)

```bash
# 0. build the scattered ESM modules (one pinned dep for this path: typescript)
cd ts && npm install && npm run build        # → ts/dist/*.js  (ES modules)

# 1. a GENERIC file_bridge agent rooted at the build output. The bridge is
#    SEALED by default (open with ingress_rule=allow_all) and its root is
#    CLAMPED to the running dir — copy the dist INTO the kernel's workdir
#    and root it relatively. /ts_dist/file/<path> then serves every dist
#    module; relative ESM imports (./kernel/kernel.js, ./transport/bridge.js,
#    …) resolve under it.
cp -R /PATH/TO/ts/dist ./ts_dist_src
fantastic kernel_state create_agent handler_module=file_bridge.tools id=ts_dist root=ts_dist_src ingress_rule=allow_all
```

The build emits **no** mount page — only the ESM modules and vendored bytes
(`ts/dist/*.js`, `ts/dist/vendor/`). The caller provides the mount HTML (a tiny
`<script type="module">` host page). The repo ships demo fixtures
(`ts/dist/_test_canvas.html`, `ts/dist/_dawee_canvas.html`) you can serve as-is
or copy from. A minimal no-canvas mount page is just:

```html
<!doctype html><html><head><meta charset="utf-8"><title>fantastic · ts</title></head>
<body><script type="module" src="/ts_dist/file/hello.js"></script></body></html>
```

Serve that page over `/ts_dist/file/<your-page>.html`. It loads `hello.js`,
which `new WsBridge(...)`s to `/kernel_state/ws`, reflects the kernel, and renders
the live tree. The host never learned what `hello.js` is; it's just bytes served
by the `file_bridge` agent.

### The canvas mount page (with vendored three/xterm)

`main.js` is the canvas bootstrap; `hello.js` is the no-canvas starter — both
emitted by the same build and served by the same `file_bridge` agent (the build emits
the modules only; you supply the mount page). The canvas boots against the
host's **`web_loader` alias** (a `web/kernel_state` the operator created — see the
root readme ["The frontend is decoupled"](../README.md#the-frontend-is-decoupled)):
its JS-side `proxy_loader` hydrates the canvas's OWN member tree from the host's
`.fantastic/web/` store and persists changes back through that alias — **no
per-page `upstream`/session config**. The page needs only (a) an **import map**
binding the bare `three`/`@xterm/*` specifiers to the vendored files, and (b) the
xterm stylesheet. All vendored bytes serve from `…/file/vendor/` (hermetic, no
CDN — see `src/vendor/VENDOR.md`). `html_agent` iframe panels need NO served
bridge: the canvas injects a `fantastic` connector into each panel's srcdoc that
reaches the JS kernel via `postMessage` only (the kernel does the local-vs-host
routing — frontend code never dials the host). The repo ships such a page as a
fixture (`ts/dist/_test_canvas.html`); a minimal one is:

```html
<!doctype html><html><head><meta charset="utf-8"><title>fantastic · canvas</title>
<link rel="stylesheet" href="/ts_dist/file/vendor/xterm.css">
<script type="importmap">{ "imports": {
  "three": "/ts_dist/file/vendor/three.module.js",
  "@xterm/xterm": "/ts_dist/file/vendor/xterm.js",
  "@xterm/addon-fit": "/ts_dist/file/vendor/addon-fit.js"
}}</script>
</head><body>
<script type="module" src="/ts_dist/file/main.js"></script>
</body></html>
```

Prereq: the operator created the `web_loader` store (`fantastic <web>
create_agent handler_module=kernel_state.tools root=.fantastic/web watch=false
alias=web_loader`). Heavy vendor loads lazily — `three` only when a `gl_view`
mounts, xterm only when a terminal opens — so the canvas shell stays light.

## Same trick, other views

Nothing above is TS-specific. To serve view package *X*:

1. build it to some `X/dist`,
2. `file_bridge` agent rooted at `X/dist` → all of `X`'s assets are served,
3. provide a mount page (a tiny `<script type="module">` host page) and open it
   over `/<file_id>/file/<path>`; it loads `X`'s entry from the same `/file/`
   root.

The host stays a dumb static server; each view package is self-contained and
swappable. Weak binding is the beauty.

---

*Part of **Aisixteen Fantastic** — licensed **AGPL-3.0-or-later** ([`../LICENSE`](../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg. 7,238,635) are trademarks of AISixteen; the license covers the code only, not the marks — forks must rename. See the [root README](../README.md#license--brand).*
