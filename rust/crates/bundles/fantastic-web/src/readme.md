# web — axum HTTP host

Serves rendering only: `/` (tree), `/<id>/` (render_html), `/<id>/file/<path>`, transport.js, favicon. Verb-invocation surfaces are sub-agents (web_ws, web_rest) that mount routes via the duck-typed `get_routes` verb. `port` field on the record is where it binds.

## Bundled web assets

The kernel ships pinned, vendored copies of shared third-party JS / CSS
deps so WebView surfaces don't depend on public CDNs at runtime. **No
network round-trip on first paint, full offline operation, no iPad
WebKit hangs from cold CDN stalls.**

| URL | content | version | size |
|---|---|---|---|
| `/transport.js` | Kernel transport runtime (`transport.js`) | in-tree | small |
| `/favicon.ico`, `/favicon.png`, `/_assets/favicon.png` | Browser-tab icon | in-tree | 602 KB |
| `/_assets/three.module.js` | Three.js | v0.160.0 | ~654 KB |
| `/_assets/xterm.min.js` | xterm.js | v6.0.0 | ~490 KB |
| `/_assets/xterm.min.css` | xterm.js default stylesheet | v6.0.0 | ~4 KB |
| `/_assets/xterm-addon-fit.min.js` | xterm.js fit addon | v0.11.0 | ~2 KB |

All third-party assets are served with `Cache-Control: public, max-age=31536000, immutable` — version-pinned per kernel build, the browser holds them indefinitely after the first fetch.

### Policy

**Consumer surfaces inside this kernel** (canvas, terminal) **and external apps embedding the kernel** (e.g. the Fantastic iOS app's brain WebView) should LOAD shared web deps from these URLs rather than vendoring their own copies. The kernel is the single source of truth for shared web deps; that's what enables offline operation + version coherence across surfaces.

To add a new shared web asset:

1. Drop the minified file in `src/assets/<name>`.
2. Add `pub const NAME: &str = include_str!("assets/<name>");` near the other asset constants.
3. Add an axum `GET /_assets/<name>` route and a handler that returns the constant with the correct `Content-Type` + the shared `ASSET_CACHE_CONTROL` header.
4. Update this README's asset table.
5. Add the third-party license attribution to `rust/THIRD_PARTY_LICENSES.md`.

### Version updates

To bump Three.js or xterm:

1. Replace the file in `src/assets/`.
2. Bump the version line in `rust/THIRD_PARTY_LICENSES.md`.
3. Bump the version cell in this README's table.
4. Verify both runnable canvas + terminal HTML files don't need source-level adjustments (rare for these libraries).

License attribution for all bundled third-party files lives in `rust/THIRD_PARTY_LICENSES.md`.
