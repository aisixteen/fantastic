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

Prereqs: `cd ../../python && uv sync` (the `fantastic` venv), `cd ../../ts &&
npm run build` (the `dist/` the browser loads), and system Chrome for the
`*.browser.itest.ts`. A test **skips** (never fails) when its prerequisite — the
venv, a built `dist/`, Chrome, or `ANTHROPIC_KEY` for the live-LLM cases — is
missing; nothing hangs or silently "passes".

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

---

*Part of **Aisixteen Fantastic** — open core, licensed **Apache-2.0** ([`../../LICENSE`](../../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg. 7,238,635) are trademarks of AISixteen; the license covers the code only, not the marks — forks must rename. See the [root README](../../README.md#license--brand).*
