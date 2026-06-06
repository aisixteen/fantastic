# Swift ↔ TS browser e2e (scaffold — not yet wired)

The cross-runtime proof that the **Swift** host serves the `ts/` frontend
in a real browser — the same story `py_ts/` proves for Python.

**Status: GENERATED scaffold, not run.** The pytest integration layer
already proves the decoupling on the swift binary without a browser:

- `../decoupling/test_decoupling_bundle_catalog.py` — the swift catalog no longer
  registers the 7 view bundles.
- `../decoupling/test_serve_frontend.py` — the swift binary serves a static `dist/`
  generically through a `file` agent (`GET /<id>/file/<path>`).

The browser layer (headless Chrome loading the swift-served frontend +
asserting panels hydrate) reuses the existing `py_ts/` harness verbatim —
duplicating its ~300-line `_host.ts` + `_chrome.ts` here would be dead
weight. To wire it:

1. Copy `py_ts/_host.ts` → `swift_ts/_host.ts` and change two things:
   - `FANTASTIC` → the swift binary (`swift/.build/debug/fantastic` — see
     `../conftest.py:swift_binary`).
   - the seed root id `fs_loader` → **`core`** (swift's root id; python
     uses `fs_loader`). This is the only non-trivial port.
2. Copy `py_ts/_chrome.ts` → `swift_ts/_chrome.ts` (unchanged).
3. Add `serve_dist.browser.itest.ts`: boot the swift host with a `file`
   agent (`id=ts_dist root=<repo>/ts/dist`) under `web`, open
   `/ts_dist/file/<mount>.html` in Chrome, assert the canvas mounts.
4. Copy `py_ts/package.json` (`"type":"module"`).

Prereqs to run: `cd swift && swift build`, `cd ts && npm run build`,
`cd ts && sh scripts/pack.sh` (→ `ts/dist/js_kernel.zip`), system Chrome.
Skips (never fails) when any is absent.

---

*Part of **Aisixteen Fantastic** — licensed **AGPL-3.0-or-later** ([`../../LICENSE`](../../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg. 7,238,635) are trademarks of AISixteen; the license covers the code only, not the marks — forks must rename. See the [root README](../../README.md#license--brand).*
