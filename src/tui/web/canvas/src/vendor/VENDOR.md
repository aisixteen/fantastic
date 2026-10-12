# Vendored dependencies — hermetic, security-assessed

**No CDN, no npm at runtime.** Every byte here was pulled from the canonical
npm registry tarball, which was verified against the registry's signed
`dist.integrity` (sha512) before extraction. `three` was additionally
cross-checked byte-identical across jsdelivr + unpkg + jsdelivr's published SRI.
All MIT-licensed. Re-verify any file with `shasum -a 256 <file>` against the
table below; re-pull with the npm tarball + integrity check (see scripts/ or
this file's git history).

| file | package@version | from tarball | bytes | sha256 | tarball integrity (npm-signed) | license |
|---|---|---|---|---|---|---|
| `three.module.js` | three@0.160.0 | `build/three.module.js` | 1272972 | `76dea8151bc9352aef3528b4262e249b2604f62543828328db978d060d61a495` | `sha512-DLU8lc0zNIPkM7rH5/e1Ks1Z8tWCGRq6g8mPowdDJpw1CFBJMU7UoJjC6PefXW7z//SSl0b2+GCw14LB+uDhng==` | MIT |
| `xterm.js` | @xterm/xterm@6.0.0 | `lib/xterm.js` | 488663 | `14903579ff54664cd72f8e8699e6961a6272c21863ec1c3b118cdc8af5d4a972` | `sha512-TQwDdQGtwwDt+2cgKDLn0IRaSxYu1tSUjgKarSDkUM0ZNiSRXFpjxEsvc/Zgc5kq5omJ+V0a8/kIM2WD3sMOYg==` | MIT |
| `xterm.css` | @xterm/xterm@6.0.0 | `css/xterm.css` | 7112 | `854a7c0fb70e8b1a083c16797ab827299fb18744f5ad34f227b48337e33293c6` | `sha512-TQwDdQGtwwDt+2cgKDLn0IRaSxYu1tSUjgKarSDkUM0ZNiSRXFpjxEsvc/Zgc5kq5omJ+V0a8/kIM2WD3sMOYg==` | MIT |
| `addon-webgl.js` | @xterm/addon-webgl@0.19.0 **+ fantastic.patch** (REBUILT, see below) | xterm.js tag 6.0.0 `addons/addon-webgl/src` | 127013 | `47bad10d3a91df205678958bd7dabbed72680dbe7e7f0c2a6844597a795f2789` | `sha512-b3fMOsyLVuCeNJWxolACEUE…` (0.19.0 tarball; its `src/` = the tag's) | MIT |
| `addon-fit.js` | @xterm/addon-fit@0.11.0 | `lib/addon-fit.js` | 1521 | `ba3ea256ce0620a0992a197d6c9baea64823fc93d8da07a9e366ca9943c18527` | `sha512-jYcgT6xtVYhnhgxh3QgYDnnNMYTcf8ElbxxFzX0IZo+vabQqSPAjC3c1wJrKB5E19VwQei89QCiZZP86DCPF7g==` | MIT |

## Roles

- `three.module.js` — ESM — import * as THREE
- `xterm.js` — UMD — sets window.Terminal
- `xterm.css` — stylesheet
- `addon-fit.js` — UMD — sets window.FitAddon
- `addon-webgl.js` — ESM — `WebglAddon`: the terminals' renderer, through ONE shared WebGL context (patched)

## Provenance

- three@0.160.0: `https://registry.npmjs.org/three/-/three-0.160.0.tgz`
- @xterm/xterm@6.0.0: `https://registry.npmjs.org/@xterm/xterm/-/xterm-6.0.0.tgz`
- @xterm/addon-fit@0.11.0: `https://registry.npmjs.org/@xterm/addon-fit/-/addon-fit-0.11.0.tgz`
- @xterm/addon-webgl@0.19.0 — NOT the tarball's bundle: rebuilt from xterm.js tag 6.0.0
  (`f447274f`, whose `addons/addon-webgl/src` is the 0.19.0 tarball's `src/`) with
  `../../vendor-src/addon-webgl/fantastic.patch`, by `vendor-src/addon-webgl/build.sh`
  (the library's vendored esbuild). The patch (owner 2026-10-09, the E65 load test):
  ONE shared WebGL2 context for every terminal, each copying its frame into its own 2D
  canvas (a page keeps only ~16 WebGL contexts); the atlas page-layout VERSION every
  sharing renderer compares (xterm.js master #6055 — a cleared/merged shared atlas left
  sibling terminals drawing stale glyphs; 6.0.0's one-owner flag never reset).
