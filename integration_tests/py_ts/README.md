# Python ↔ TS integration tests

Cross-runtime tests that pair a **real python `fantastic` host** with the **TS
frontend kernel in a real browser**, over the `ws_bridge`. They're the
node-driven sibling of the pytest bridge suite in [`../`](../README.md): same
intent — exercise the interop surface *between* kernels end-to-end — but a
different driver (`node --test` + a headless Chrome over CDP), because the TS
kernel only runs in a browser.

Why here and not in `ts/tests/`? Those are **pure-TS unit** tests (in-process,
no host, no browser). These boot a python subprocess *and* a browser — they're
**python↔ts**, so they live with the other cross-runtime suites under
`integration_tests/`, named by the runtime pair (`py_ts`).

## Files

- `*.itest.ts` — the integration tests (boot a host via `_host.ts`, drive a
  browser via `_chrome.ts`).
- `_host.ts` — boots/tears-down a real `fantastic` daemon in a throwaway tmp dir
  (web + web_ws + optional web_loader / exec_bridge / scheduler / LLM agents);
  exposes `bootHost`, `teardownHost`, `restartHost`, `DIST_DIR`, `dotenvKey`.
- `_chrome.ts` — minimal CDP browser driver; the `*.browser.itest.ts` skip
  cleanly when system Chrome is absent.
- `e2e/` — the **heavy, rarely-run** emergence + live-LLM layer (real tokens).
  See [`e2e/README.md`](e2e/README.md).

## Running

```bash
cd integration_tests/py_ts
npm run test:integration                                    # all *.itest.ts
node --test --test-force-exit persistence.browser.itest.ts  # one file
node --test --test-force-exit --test-name-pattern="^A:" \
  scheduler_ai_html.browser.itest.ts                        # one case
```

### Target: local binaries (default) or the container

Like the pytest suite, the SAME tests run against either the local venv binary
or the universal container image, via `FANTASTIC_TARGET` (the `_host.ts` harness
routes seeding + the daemon through the chosen target — no test changes):

```bash
node --test --test-force-exit bridge.itest.ts               # FANTASTIC_TARGET=local (default)
FANTASTIC_TARGET=container node --test --test-force-exit two_tree.browser.itest.ts
FANTASTIC_IMAGE=fantastic:latest FANTASTIC_TARGET=container npm run test:integration
```

Container target: needs the image built (`sh container/build.sh`, **host arch**)
and podman/docker; the daemon runs the python runtime with `-p 127.0.0.1:port:port`
(its `/` is the dynamic agent index), seeding one-shots run inside the image, and the canvas
page is extracted into the bind-mounted workdir (file_bridge clamps roots to the
running dir) so the `canvas_page` file_bridge agent serves it. Everything is
host/browser → container (no container↔container), so it works over `-p`. The
workdir lives under `py_ts/tmp/` (a VM-mounted path) since the OS tmpdir may not
be mounted in the podman/docker VM.

Prereqs: `cd ../../src/lib/python && uv sync` (the `fantastic` venv) and system
Chrome for the `*.browser.itest.ts`. The page the browser loads is the CANVAS
APP (`src/tui/web/canvas`): `bootHost({serveDist})` packs it
(`scripts/pack.sh` → `dist/canvas.zip`, the vendored esbuild, no npm), extracts
its `bundle.min.js` into the workdir beside a one-tag page, and serves it at
`host.pageUrl`. A test **skips** (never fails) when its prerequisite — the venv,
Chrome, or a reachable LLM backend for the live cases — is missing; nothing
hangs or silently "passes".

Every page agent SENDS AS ITSELF, so a fixture record carries its own
`can_call` (the host agents and verbs it calls or watches), and the harness
grants the socket leg onto each host agent it seeds, both halves.

## What's covered

- `persistence.browser.itest.ts` — save → **daemon restart** → rehydrate, full
  stack (seeded *and* runtime-persisted records survive a real re-spawn).
- `bridge.itest.ts`, `two_tree.itest.ts` — the `WsBridge` against a live host;
  the genuine two-runtime persist→rehydrate round-trip the in-process tests cover
  only in halves.
- `html_agent.browser.itest.ts`, `two_tree.browser.itest.ts` — panels render +
  hydrate in a browser against the host.
- `canvas_terminal.browser.itest.ts` — the canvas pair: a dblclick spawns the
  backend the terminal view DECLARES it fronts (`exec_bridge.tools`) as a
  child of the socket leg carrying `view_id` + `canvas_id`, binds the view to it
  (and grants exactly it), then closing the frame removes both.
- `scheduler_ai_html.browser.itest.ts` — the deep cross-kernel cascade
  (scheduler ⇄ python ⇄ AI ⇄ JS panels); `A` + `G` are deterministic, `B–J` are
  live (PAID — real tokens on the active backend).
- `llm_e2e.browser.itest.ts` — a live-LLM browser round-trip (PAID).
- `bundle_revive.browser.itest.ts` — revives the canvas artifact
  (`src/tui/web/canvas/dist/canvas.zip`) as its readme says: the stamped sha256
  matches the served bundle, the single `bundle.min.js` runs with no import map
  and no external stylesheet (`three` + xterm + xterm.css inlined), and the
  bridge + the terminal pair work through it.

---

*Part of **Aisixteen Fantastic** — licensed **AGPL-3.0-or-later** ([`../../LICENSE`](../../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg. 7,238,635) are trademarks of AISixteen; the license covers the code only, not the marks — forks must rename. See the [root README](../../README.md#license--brand).*
