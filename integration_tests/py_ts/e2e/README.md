# Fantastic E2E — the heavy, rarely-run validation

This is the **most expensive** test layer in the repo. It is **NOT** part of
`pytest`, `npm test`, or the normal `node --test` runs — those stay fast and free.
This one:

- spends **real Anthropic tokens** (live `claude-opus-4-8` inference turns), and
- **spawns a fresh "builder" agent** that wires the system **from zero** using only
  `curl` + the system's own readmes (no source, no kernel tools),

so it takes **minutes, not seconds, and costs money**. Run it **rarely** — before a
release, or after touching the workflow substrate (the `python_runtime` connector,
the AI backends, the scheduler, `ai_view`, or any of the readmes those depend on).

## Why it exists — two things the cheap tests can't prove

The unit + browser integration tests prove the substrate **mechanically works**, but
they **hard-code the wiring** (the setup is `bootHost`, the tree is seeded by hand,
the `fantastic.send(...)`/`watch` calls are written by us). They answer *"given the
correct wiring, does it behave?"* — not *"can an LLM build it?"*. This layer closes
both gaps:

1. **Live integration** — the real scheduler → AI → panel chain, AI-drives-UI by
   tool-call, AI → python, AI → AI — against a live model, end-to-end in a browser.
2. **Emergence from zero** — a spawned, readme-only agent discovers the agents via
   `reflect` and **composes the whole workflow itself**, proving the
   self-description is sufficient for capability to *emerge* (the north-star).
3. **Pairing cardinality (Phase 2.5 — the pinnacle)** — a readme-only agent derives the
   *cardinality* of the four unit kinds from self-description alone, never told a number:
   **terminal_view ↔ terminal_backend = 1:1** (bound by `backend_id` to an exclusive
   PTY), **ai_view ↔ AI backend = 1:1** (bound by `backend_id` to shared compute), and
   **html_agent panel ↔ python_runtime = 1:N** (a content agent, no binding — its JS
   send/watches a many-jobs runtime by id). `pairing_verify.ts` asserts it structurally;
   a wrong derivation is the signal to strengthen the readme nudge and re-run.

## Prerequisites

- `cd python && uv sync` (the `fantastic` venv).
- `cd ts && npm run build` (the `ts/dist` frontend the browser loads).
- System **Chrome** (headless) — see `../_chrome.ts`.
- **`ANTHROPIC_KEY`** (or `ANTHROPIC_API_KEY`) in the repo `.env`.

If any are missing the runner reports it and skips the affected phase — it never
hangs or silently "passes".

## How to run

It is **agent-driven** — an operating agent (Claude Code) executes `RUN.md`, because
Phase 2 spawns a sub-agent (a plain script can't). From a session, say:

> run the e2e suite (integration_tests/py_ts/e2e/RUN.md)

The agent boots a bare host with `boot_bare_host.sh`, runs the live browser tests,
spawns the builder agent with the prompt in `RUN.md`, verifies the result with
`verify_panel.ts`, fills the results table, and tears the host down.

## Files

- `RUN.md` — the runbook the agent follows (phases, the exact builder prompt,
  acceptance criteria, results table).
- `boot_bare_host.sh` — boots a BARE substrate daemon (web + web_ws + web_rest +
  web_loader + a seeded `canvas` + the served frontend; **no** workflow agents).
- `verify_panel.ts` — headless-Chrome check that a canvas panel shows the answer.
- `pairing_verify.ts` — the Phase 2.5 pinnacle assertion: checks the builder *derived*
  the pairing cardinality (terminal_view/ai_view each 1:1 via `backend_id`; html_agent
  panels 1:N with no binding) from readmes alone.
- `dawee_verify.ts` — headless-Chrome check of the dawee port: panels render and the
  `dawee-mixer` BroadcastChannel bus crosses sandboxed iframes through the host
  `mixerbus` agent.
- `teardown_host.sh` — kill the daemon + remove its temp dir.

---

*Part of **Aisixteen Fantastic** — open core, licensed **Apache-2.0** ([`../../../LICENSE`](../../../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg. 7,238,635) are trademarks of AISixteen; the license covers the code only, not the marks — forks must rename. See the [root README](../../../README.md#license--brand).*
