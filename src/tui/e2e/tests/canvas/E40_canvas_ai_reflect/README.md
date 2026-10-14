# E40 `canvas_ai_reflect` — does the AI find the page kernel on its own?

The hardest form (owner, 2026-09-30: "we go hardest"). A real model runs an AI
CELL on a fresh canvas — a HOST agent with its `ai_view` client on the page, as
a terminal is — and is asked, in plain words, to look around the canvas and the backend. ai_core's
default system block stays whole (the call grammar, the verbs, its identity);
the cell's FIRST turn carries a `[canvas setup]` block (`CANVAS_SETUP`,
`ai_view.ts`): two levels (a host agent, shown as a cell on a canvas page), two
kernels (the host = `kernel`; the page = `<page guid>|…`, its kernel
`<page guid>|kernel`), and "to learn your environment, reflect each kernel —
the host first, its reflect lists the connected peers". **No page address is
given**; the AI finds the page itself —

- its OWN record's `can_call` names `<page guid>|canvas`, `<page guid>|background`
  and `<page guid>|*` (reflect);
- the host root's reflect lists connected `peers: [{guid, via}]`, and
  `reflect <guid>|kernel` returns the page's tree and verbs; `around` is the
  host's `users` gateway (from the page records), named in the page canvas's reflect.

History: run 1 (2026-09-30, qwen3.8-27b) FAILED with the setup sent as
`system_prompt` — which REPLACED ai_core's default block, so the model saw no
call grammar at all ("I don't actually have any tools"). The setup moved into
the turn.

**PASS**: the AI calls `reflect` on an agent of the PAGE (`<page guid>|…`) at
least once — it found the page and looked. **Reported, not asserted**: every
call it made, in order (`→ target verb`); whether it called `users around`; whether its
last answer names the terminal and html cells placed near it; the turns taken.

Setup, all by the probe: the CLI binary built with the plug's backend feature;
a throwaway `FANTASTIC_HOME` whose one saved set-up IS the plug's record (default);
`fantastic canvas --port N` in a fresh dir; headless Chrome drives
the page: a terminal cell at the right, an html panel below it, then the AI
cell at the left with the first turn "Hey! Look around and see whats on a canvas and a backend".

Run it (the plug's variables, nothing guessed — e.g. vLLM):

    cd src/lib/python && BACKEND=vllm VLLM_ENDPOINT=… FANTASTIC_AI_MODEL=… \
      VLLM_KV_SLOTS=… VLLM_KV_OVERFLOW=… \
      uv run python ../../../e2e/run.py --id canvas_ai_reflect --lanes 1

The CLI links `anthropic_backend.tools`, `responses_backend.tools` and
`vllm_backend.tools`; another plug is refused up front, naming what it needs.
