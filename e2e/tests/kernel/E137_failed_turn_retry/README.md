# `failed_turn_retry` (E137) — a failed turn is retried on its own (#533)

Free: no model. A scripted OpenAI-compatible server (`e2e/mocks/vllm.py`; the probe
adds a stream cut with `finish_reason: length`) plays a provider that fails, for the
Rust kernel's own `ai_chat.tools` agent (provider: a `vllm_backend`, via `provider_id`) on the PACKAGED host, over `web_rest`. Rust only: Python has no #533 retry yet (checklist in #170).

1. HTTP 500 once, then a plain `done`: the next request is the agent's own next turn
   and its last user turn carries `[turn failed]` and the provider's error text; the
   history holds one `error` row; the chain settles.
2. A stream cut at `max_tokens` (`finish_reason: length`), then `done`: same, and the
   text names `max_tokens`.
3. HTTP 500 on EVERY turn: the chain makes exactly `MAX_SILENT_TURNS` (10) requests
   and stops (`gave_up`; the agent is idle, no request follows). The `gave_up` status
   event itself is not readable over `web_rest`; the unit tests assert it.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id failed_turn_retry --lanes 1
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E137_failed_turn_retry/probe.py
```
