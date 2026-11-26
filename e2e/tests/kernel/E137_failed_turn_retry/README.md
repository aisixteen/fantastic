# `failed_turn_retry` (E137) — a failed pass is retried only on the link (#533, #795)

Free: no model. A scripted OpenAI-compatible server (`e2e/mocks/vllm.py`; the probe
adds a stream cut with `finish_reason: length`) plays a provider that fails, on the
PACKAGED host of `RUNTIME`, over `web_rest`: on both runtimes an `ai_chat.tools` chat naming a `vllm_backend` provider (`provider_id`, #521), with `transport_retries: 1`.

1. HTTP 500 once, then a plain `done`: the second request is the same pass retried on
   the link; it answers, and the history holds no `error` row; the chain settles.
2. A stream cut at `max_tokens` (`finish_reason: length`): no link failure, so one
   request; the history holds one `error` row naming `max_tokens`.
3. HTTP 500 on EVERY request: exactly `1 + transport_retries` (2) requests, then the
   pass fails (the agent is idle, no request follows). The `failed` status event
   itself is not readable over `web_rest`; the unit tests assert it.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id failed_turn_retry --lanes 2
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E137_failed_turn_retry/probe.py
```
