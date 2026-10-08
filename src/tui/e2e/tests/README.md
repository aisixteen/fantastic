# CLI live tests — `src/tui/e2e/tests/<group>/<id>/`

Run by the KERNEL suite's launcher (`e2e/run.py`, second discovery root), the
model from its plug (`BACKEND=…`, `e2e/backends`). The fixture is the PRODUCT:
`_lib/canvas_live.py` builds `fantastic-tui` with the plug's feature, a
throwaway `FANTASTIC_HOME`, `fantastic --headless canvas` in a fresh dir
(seeded as the product stores it), and drives a real headless Chrome over CDP.
Evidence is the host's own stream (`[<id>] <page guid>|<agent> call <verb>`),
the disk, and the page's DOM / frames.

ARTIFACTS: `src/tui/e2e/runs/<CANVAS_E2E_STAMP or time>/<id>/` — host.log, the
project dir, the page's final DOM, page errors, summary.json. Gitignored; wipe
with `src/tui/e2e/wipe.sh`.

    cd src/lib/python && export VLLM_API_KEY=… && CANVAS_E2E_STAMP=run1 \
      BACKEND=vllm VLLM_ENDPOINT=… FANTASTIC_AI_MODEL=… VLLM_KV_SLOTS=… VLLM_KV_OVERFLOW=… \
      uv run python ../../../e2e/run.py --group canvas --lanes 2        # live (+ --free for all)
      uv run python ../../../e2e/run.py --group canvas --only-free --lanes 2   # scripted only

| id | capability | inferred from | kind |
|---|---|---|---|
| E40 `canvas_ai_reflect` | the AI finds the page kernel on its own and reflects it | the two-kernel setup | live |
| E41 `deck_order` | read a two-language deck: order + the gap | gaus_arta, unified_vis | live |
| E42 `lay_out` | arrange cells into an ordered row | every project | live |
| E43 `make_panel` | author a working calculator panel | dara flows, gaus calculator | live |
| E44 `bus_wire` | per-pair emit wiring, one source left unwired | dawee instruments → mixer | live |
| E45 `run_service` | start / report / stop a server through exec | dara Compose Control, terminals | live |
| E46 `embed_live` | frame a running service in a panel | dara :3001 / :8000 / :3002 | live |
| E47 `pdf_panel` | show a served PDF through a deny-all file leg | dara PRD / patent / mockups | live |
| E48 `set_scene` | set the singleton background (stays locked) | dara sunset, unified_vis motif | live |
| E49 `terminal_task` | answer from a canvas terminal (git) | the terminals | live |
| E50 `remember` | keep → close → restart → reopen → recall | kept dialogs | live |
| E51 `inventory` | inventory a 27-cell canvas by kind, bounded calls | dara (33 cells) | live |
| E52 `hydrate_scale` | 300 cells render within the bound | the dara 55 s regression | scripted |
| E53 `panel_types` | every kind renders and survives reloads | all | scripted |
| E54 `media` | video + audio play, before and after a reload | gaus final cuts, dawee WebAudio | scripted |
| E55 `dev_surface` | an AI builds a reactive dev surface (controls, logs, app, status) | dara Compose + terminals + webapp | live |
| E56 `dev_reactive` | a reference dev surface reacts to build / run / stop / edit | same | scripted |
| E57 `setup_ai` | the set-up cell writes an agent + default template; both answer | owner: set-up is VERY important | live |
| E58 `prototype` | from an EMPTY canvas: a script, the exec agent that runs it, Start/Stop + a live log wired both ways; then a change (count by 10, the last value big) — scored by clicking | owner 2026-10-04: yes/no for the prototyping flow (a garage, no packaging) | live |
| E59 `calc` | behind a SEEDED six-button panel (det, factor, simpson, roots, fib_mod, gcd_lcm) the AI writes `calc.py` + its own `test_calc.py` in one ask; scored by its tests, hidden vectors per function and every button | owner 2026-10-07: can the model write good multi-function code and test it |
| E60 `bundle_flow` | a python agent's whole life: double-click → a new bundle + instance + view, live edits of logic (reload) and view (re-render), kept across a restart, copied into a release that lives on its own, then the AI updates the release and overwrites it from the working bundle | owner 2026-10-08: the a–f workflow; the AI must tell the BUNDLE (code) from the AGENT (an instance) |
| E61 `canvas_cells` | scripted: a bare canvas (nothing floats), a FAKE coding agent first on PATH listed and launched exactly (it echoes), `sidebar` + `canvases` cells, `+ new canvas` opens an empty, isolated peer with its own records, a page bundle in its folder runs and re-renders on an edit | owner 2026-10-08: cells only, flat canvases, coding-agent launchers |
| E62 `mcp_server` | scripted: the lib's fake MCP server; the `mcp server` tile's form adds it, its view lists the server and its tools, the key is stored in the home (on no record) and sent as Bearer on every request, a tool answers, ⟳ lists again | owner 2026-10-08: MCP servers on the canvas |

## Results — 2026-10-01, vLLM qwen3.8-27b, 2 lanes (the latest run of each)

PASS 16 / 18: E40–E48, E50–E54, E56, E57 (E47 by its record: the PDF's own bytes
embedded as `data:`; rescored after the scorer learned that form).
FAIL, the model's own: **E49** (it `start`ed a new job on the terminal's exec
agent instead of `write`-ing to the running shell, then stopped to ask before
widening `allowed` itself) · **E55** (three runs: 5, 1, 0 of the 5 cells
within the time — the surface is the hardest ask).

What the runs fixed in the PRODUCT: a hydrated page never said `hello` (lib ts,
kernel session) · a cell's `bytes` body arrives as raw bytes over the bridge
(decoded now) · `to` is ai_core's routing tag (`wire` takes sender/receiver) ·
the AI needs the canvas verbs on page cells (operand gating) · the background's
contract stated + compiled before it is stored · the AI cell's button stayed
on "stop" after an answer (so Send interrupted) · the set-up form forgot its
fields after a write (so no template could be saved) · templates were checked
as full agent records (demanding `memory_store_id`).
Harness lessons: evidence from the host stream and the records, not the cell's
tool lines; a "thinking" bubble or a busy phase pill is never "done"; untrusted
panels render SANDBOXED — out-of-process frames, reached as CDP targets.
