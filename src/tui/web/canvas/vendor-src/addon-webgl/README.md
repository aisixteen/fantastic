# addon-webgl — the shared-context terminal renderer

`@xterm/addon-webgl@0.19.0` (the addon xterm.js tag 6.0.0 ships) + `fantastic.patch`,
built into `../../src/vendor/addon-webgl.js` by `build.sh` (the library's vendored
esbuild; no npm). A **pure xterm addon** — same API as upstream (`new WebglAddon()`,
`onContextLoss`, `clearTextureAtlas`), no knowledge of the canvas. The canvas uses it
through `src/bundles/terminal_view/renderer.ts`, for every terminal (the default; no
switcher yet).

## What the patch changes (owner 2026-10-09, E65 / E66)

1. **One WebGL2 context for the page** (`SharedContext.ts`). Every terminal renders
   through it into an `OffscreenCanvas` sized to its frame, and the frame is handed to
   the terminal's own `bitmaprenderer` canvas (`transferToImageBitmap` — zero copy).
   A page keeps only ~16 WebGL contexts, so one context per terminal cannot serve N.
2. **Atlas pages are one GPU texture each**, shared by every renderer, uploaded once per
   page version; every renderer binds all pages before it draws (texture units are
   context state — binding only on upload drew with another terminal's pages, or the
   1×1 red placeholders).
3. **The atlas page-layout version** every sharing renderer compares (backport of
   xterm.js master #6055): a cleared / merged / evicted shared atlas reaches every
   sibling (6.0.0's one-owner flag reached only the clearer, and was never reset).
4. **Atlas pages sample NEAREST** (no mipmaps; owner 2026-10-12, E66): glyphs are drawn
   1:1, so each texel is drawn exactly — with LINEAR, a glyph re-rasterized at another
   atlas spot (a clear, a merge, a lost context) drew slightly different edges.
5. **A merge mid-frame rebuilds that frame** (owner 2026-10-12, E66): the page-layout
   version is checked again after the frame places its new glyphs — a merge while
   rasterizing them left the rows already drawn on stale texture coords until the
   terminal next drew (whole screens of wrong glyphs once its output stopped).
6. **Counters for probes** on `globalThis.__fantasticWebgl`: renders, uploads, page
   textures live / released, contexts made, and `loseContext()` / `restoreContext()`.

## Rebuild

    git clone --depth 1 --branch 6.0.0 https://github.com/xtermjs/xterm.js.git <dir>
    sh build.sh <dir>        # → src/vendor/addon-webgl.js; update its sha256 in VENDOR.md

## Its tests

- `src/tui/e2e/tests/canvas/E65_load` — N two-way terminals: fps, keystroke echo, WS,
  row store, disk, CPU / GPU / load.
- `src/tui/e2e/tests/canvas/E66_render_fidelity` — the glyph / colour sweep: no
  placeholder red in any frame, every terminal equal to its fresh redraw, a lost context.
