# E2E runbook — live integration + emergence from zero → a connected system

> scope: whole stack (host Python + browser JS + live Anthropic + a spawned builder)
> cost: real tokens + minutes. Run RARELY. See `README.md`.
> The operating agent (Claude Code) executes the phases below in order.

## Phase 0 — preflight

- `ls python/.venv/bin/fantastic` exists (`uv sync` if not).
- `ls ts/dist/main.js` exists (`cd ts && npm run build` if not).
- `ls ts/dist/js_kernel.zip` exists (`cd ts && sh scripts/pack.sh` if not) —
  required for zip-revive checks; `revive_verify.ts` skips cleanly when absent.
- `chromeAvailable()` (system Chrome) — else Phase 1/2c skip.
- `ANTHROPIC_KEY` (or `ANTHROPIC_API_KEY`) in repo `.env` — else Phase 1 (B–E) and
  Phase 2 skip (the deterministic Phase-1 A still runs).

## Phase 1 — live integration (scripted)

Run the heavy browser tests against a live model:

```bash
cd integration_tests/py_ts && node --test --test-force-exit scheduler_ai_html.browser.itest.ts
```

Expect **10/10 pass** with a key present: A (scheduler→python→panel, deterministic);
B (scheduler→AI→panel, "apples or bananas"); C (AI→panel by tool-call); D (AI→python);
E (AI→AI leaf); F (a python JOB calls an AI via its kernel connector — host-side
meta-orchestration); G (ai_view renders inline fronting a backend — keyless); H (AI
*composes* a schedule → AI→scheduler→python); I (scheduler→python-job→AI via connector,
deep cascade); J (scheduler tick → JS panel → AI). These last three close the deep
cross-kernel cascade (AI→scheduler, scheduler→PY→AI, scheduler→JS→AI). Without a key,
**A + G pass** and the rest skip. Record the result.

## Phase 2 — emergence from zero → a CONNECTED system

The point: prove an LLM can build a *connected* workflow system from **nothing but
the running system + its readmes**. Nobody hands it the wiring.

### 2a. Boot a bare substrate

```bash
bash integration_tests/py_ts/e2e/boot_bare_host.sh 8930
```

Prints `TMP=… PID=… PORT=8930 WEB=… REST=… REST_URL=… CANVAS_URL=…`. This daemon has
web / web_ws / web_rest / web_loader / a seeded `canvas` / the served frontend — and
**no** `python_runtime`, scheduler, AI, or panels. The builder creates all of those.

### 2b. Spawn the readme-only builder agent

Spawn a sub-agent (general-purpose) with **exactly** this prompt, substituting the
`REST_URL` from 2a. It gets **only** `curl` + what the system tells it — it must NOT
read this repo's source or use any kernel tools.

> You are wiring a live agent system you've never seen, through ONE door: HTTP at
> `<REST_URL>`. `POST <REST_URL>/<target_id>` with a JSON body `{"type":"<verb>",
> ...}` sends a verb to an agent; `GET <REST_URL>/_reflect[/<id>][?readme=1]` reads
> the tree / an agent / its readme. Do NOT read any source files on disk and do NOT
> use any tool other than `curl` (via Bash) — the system describes itself; learn it
> only from `reflect` and readmes.
>
> GOAL — build, from zero, a small but fully CONNECTED workflow system where every
> piece talks to the others by id:
>   1. a **scheduler** that periodically wakes a **"router" AI agent**;
>   2. the router, each tick, must DISPATCH work — call a **python_runtime** job to
>      compute something AND consult a **second AI** agent — then write the outcome
>      into a shared **memory** agent (yaml_state);
>   3. surface results on **two HTML panels** on the `canvas` (one for the router's
>      answer, one reading the shared memory), each a frontend `html_agent.ts`
>      record persisted under `canvas` via the `web_loader` store, watching the host
>      agent(s) by id.
> Use the AI bundle `anthropic_backend.tools` (each AI needs its own history `file`
> agent via `file_agent_id`); the scheduler needs a `file` agent too. Keep prompts
> tiny (one-word / one-number answers) to stay cheap. Make the schedule recurring
> with a modest interval (~8s) so a browser opened later catches a fresh fire.
>
> First `GET <REST_URL>/_reflect?readme=1` and read the kernel readme end-to-end —
> it tells you the transports, how memory/AI/compute are addressed, how panels are
> persisted, and how an AI worker's completion is routed. Then build it.
>
> VERIFY before returning: poll the router AI's `history` (and the memory agent's
> `read`) over curl to confirm a scheduled tick actually produced an answer. Then
> REPORT: every agent id you created with its handler_module + one-line role; the
> exact panel records you persisted (id, what each watches, the gist of its JS); and
> the router's most recent answer + the memory contents. Be concrete and exhaustive.

### 2c. Verify the panels render (headless Chrome)

```bash
node integration_tests/py_ts/e2e/verify_panel.ts 8930
```

Opens `CANVAS_URL`, waits for the panels to hydrate, and dumps what each shows.
Acceptance: at least one panel displays the live workflow output (e.g. matches
`/apples|bananas/i` or a computed number), no uncaught page errors.

For **zip-revive** checks (when the builder assembled the system from the zip
readme and is serving `bundle.min.js` rather than the dev `main.js`), use
`revive_verify.ts` instead — it checks the same liveness criteria PLUS that
the inlined xterm.css was injected and the single-bundle revive left a
`terminal_backend` on disk:

```bash
node integration_tests/py_ts/e2e/revive_verify.ts <CANVAS_URL> <WORKDIR>
```

### 2d. Capture the final tree + tear down

```bash
curl -s "<REST_URL>/_reflect" | python3 -m json.tool   # the system the builder grew
bash integration_tests/py_ts/e2e/teardown_host.sh <PID> <TMP>
```

## Phase 2.5 — pairing cardinality from readmes alone (the pinnacle)

The point: prove an LLM derives the **pairing CARDINALITY** of the four unit kinds
from self-description ALONE — never told a number:
- **terminal_view ↔ terminal_backend = 1:1** (a view *bound by `backend_id`* to an
  *exclusive PTY session*);
- **ai_view ↔ AI backend = 1:1** (a view *bound by `backend_id`* to *shared compute*);
- **html_agent panel ↔ python_runtime = 1:N** (a *content* agent, no binding — its JS
  `fantastic.send`/`watch`es a *many-jobs* runtime by id, behaviorally).

Reuses `boot_bare_host.sh` (2a). The builder is **the operating agent's spawned Claude
Code sub-agent**, NOT the kernel's Anthropic backend — curl + reflect only. It curls
the HOST backend readmes (terminal/ai/python_runtime); the FRONTEND view-bundle
readmes (not curl-reachable on a bare host) are handed to it verbatim below — real
strings, **no cardinality stated**. The builder must DERIVE the cardinality.

Spawn a sub-agent (general-purpose, Bash+`curl` ONLY, no source/kernel tools) with
this prompt, substituting `REST_URL`:

> You are wiring a live agent system you've never seen, through ONE door: HTTP at
> `<REST_URL>`. `POST <REST_URL>/<target_id>` with JSON `{"type":"<verb>", ...}` sends
> a verb to an agent by id; `GET <REST_URL>/_reflect[?readme=1]` reads the host tree,
> `GET <REST_URL>/_reflect/<id>?readme=1` one agent + readme, and
> `GET <REST_URL>/_reflect?bundles=all&readme=1` lists installed HOST bundles (what you
> can create) with readmes. Do NOT read any source on disk; use ONLY `curl` (via Bash).
> Learn the system from reflect + readmes.
>
> Create a HOST agent: `POST <REST_URL>/kernel {"type":"create_agent","handler_module":
> "<bundle>", ...}` (`kernel` = the host root). Show something in the canvas by
> persisting a FRONTEND record via the store: `POST <REST_URL>/web_loader {"type":
> "persist_record","record":{"id":"<unique>","parent_id":"canvas","handler_module":
> "<view bundle>", ...}}`. The frontend view-bundle catalog (the client bundles you may
> persist — read what each fronts):
>   - `terminal_view.ts` — "HTML/xterm CLIENT for a host PTY. Fronts any agent answering
>     the PTY verb surface (boot/write/ack/resize/interrupt/stop) and emitting
>     output/exited, bound by `backend_id`: watches the backend's output, renders it,
>     sends keystrokes via write."
>   - `ai_view.ts` — "HTML chat CLIENT for a host LLM backend. Fronts any agent answering
>     send/history/interrupt/status, bound by `backend_id`: renders streamed token/done
>     events, sends user turns via send."
>   - `html_agent.ts` — "Frontend HTML content agent. Holds a mutable `html` body in its
>     record, rendered in a sandboxed frame; the injected connector relays
>     send/emit/watch/onMessage to the kernel. Content, not a host client." (the injected
>     connector is the global `fantastic`: `fantastic.send(id, payload)`,
>     `fantastic.watch(id, cb)` — reach any host agent by id from a panel's JS.)
>
> GOAL — build a small workspace and connect every piece by id, deciding the wiring
> YOURSELF from the readmes (nobody tells you how the pieces pair or how many of each):
>   1. an interactive **terminal**;
>   2. a **chat with an AI**;
>   3. a **couple of HTML panels that run python computations and display the results**.
> Discover from reflect/readmes which HOST bundle provides each capability, create what
> is needed, and connect each frontend piece to the host capability it needs — *the
> connection method AND how many of each you make are your call, derived from what the
> readmes say each thing IS*. Keep compute trivial (one-number answers, e.g. 6*7) and do
> NOT run a live AI completion — the AI backend only needs to exist and be paired.
>
> VERIFY over curl before returning: `GET <REST_URL>/_reflect?tree=all` (host agents you
> made) and `POST <REST_URL>/web_loader {"type":"load_tree"}` (frontend records you
> persisted); confirm each frontend piece points at the right host agent.
>
> REPORT exhaustively: every agent id created (host + frontend) with handler_module +
> one-line role; for EACH frontend piece, exactly HOW you connected it to a host agent
> and WHY (the readme phrase that led you there, and how many backends you made for how
> many fronts); and the final structure.

Then assert the derived cardinality structurally (no browser needed):

```bash
node integration_tests/py_ts/e2e/pairing_verify.ts <REST_URL>
```

PASS = `terminal_view`/`ai_view` each carry a `backend_id` to a distinct backend of the
right type (1:1, no shared/orphan PTY); `html_agent` panels carry NO `backend_id` and
their `html` send/watches a `python_runtime` (1:N). A wrong derivation (panel given a
`backend_id`, a shared PTY, a view with none) is the SIGNAL the self-description is
insufficient → strengthen the still-implicit readme nudge (Phase A) and re-run. That
iteration loop is the test's value.

## Phase 3 — OUTPUT (what the operator gets back)

After the run, the operating agent emits, **inline in chat**:

1. **Agents created** — a compact tree/list from the final `_reflect`: `id —
   handler_module — role`, grouped by workflow.
2. **Sequence diagrams (super simple)** — one tiny ASCII diagram per workflow chain,
   e.g.:

   ```
   scheduler ──every 8s──▶ ai_router
                              ├──▶ python_runtime   (compute N)
                              ├──▶ ai_helper        (classify)
                              ├──▶ memory           (write result)
                              └──▶ panel_answer / panel_memory  (show)
   ```

3. **What it does for you** — ONE plain-language paragraph: what this self-built
   system actually does, in human terms, no jargon.

## Results

| Phase | What | Pass/Skip/Fail | Notes |
|---|---|---|---|
| 1 | live browser itests (A–J) | | |
| 2 | builder wired the connected system from zero (curl+readmes only) | | |
| 2c | panels render the live output | | |
| 3 | agents + diagrams + paragraph emitted | | |
