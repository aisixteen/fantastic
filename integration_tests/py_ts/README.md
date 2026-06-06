# Python ↔ TS integration tests

Cross-runtime tests that pair a **real python `fantastic` host** with the **TS
frontend kernel in a real browser**, over the `kernel_bridge`. They're the
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
  (web + web_ws + optional web_loader / python_runtime / scheduler / LLM agents);
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
and `FANTASTIC_HEAD=off`, seeding one-shots run inside the image, and the frontend
`dist/` is bind-mounted in so the `ts_dist` file agent serves it. Everything is
host/browser → container (no container↔container), so it works over `-p`. The
workdir lives under `py_ts/tmp/` (a VM-mounted path) since the OS tmpdir may not
be mounted in the podman/docker VM.

Prereqs: `cd ../../python && uv sync` (the `fantastic` venv), `cd ../../ts &&
npm run build` (the `dist/` the browser loads), and system Chrome for the
`*.browser.itest.ts`. `bundle_revive.browser.itest.ts` additionally needs the
sovereign artifact: `cd ../../ts && sh scripts/pack.sh` (→
`ts/dist/js_kernel.zip`). A test **skips** (never fails) when its prerequisite
— the venv, a built `dist/`, the zip, Chrome, or `ANTHROPIC_KEY` for the
live-LLM cases — is missing; nothing hangs or silently "passes".

## What's covered

- `persistence.browser.itest.ts` — save → **daemon restart** → rehydrate, full
  stack (seeded *and* runtime-persisted records survive a real re-spawn).
- `bridge.itest.ts`, `two_tree.itest.ts` — the `WsBridge` against a live host;
  the genuine two-runtime persist→rehydrate round-trip the in-process tests cover
  only in halves.
- `html_agent.browser.itest.ts`, `two_tree.browser.itest.ts` — panels render +
  hydrate in a browser against the host.
- `canvas_terminal.browser.itest.ts` — the Part-3 discover-and-pair lifecycle: a
  canvas dblclick **discovers** a PTY-capable `terminal_backend` from the host
  catalog (no hardcoded handler), pairs a `terminal_view` 1:1, then closing the
  frame **cascades** removal of both — the frontend view *and* the host backend it
  owns — over the bridge.
- `scheduler_ai_html.browser.itest.ts` — the deep cross-kernel cascade
  (scheduler ⇄ python ⇄ AI ⇄ JS panels); `A` + `G` are deterministic, `B–J` need
  `ANTHROPIC_KEY`.
- `llm_e2e.browser.itest.ts` — a live-LLM browser round-trip.
- `bundle_revive.browser.itest.ts` — revives the sovereign artifact
  (`ts/dist/js_kernel.zip`) in a real browser: serves the single
  `bundle.min.js` through a `file` agent (no import map, no external
  stylesheet) and proves `three` + xterm + xterm.css are inlined, and
  that bridge + terminal pairing work through the rolled-up bundle.

---

*Part of **Aisixteen Fantastic** — licensed **AGPL-3.0-or-later** ([`../../LICENSE`](../../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg. 7,238,635) are trademarks of AISixteen; the license covers the code only, not the marks — forks must rename. See the [root README](../../README.md#license--brand).*
