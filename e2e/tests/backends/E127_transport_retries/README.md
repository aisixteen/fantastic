# E127 transport_retries

NO MODEL, Rust runtime, packaged host over `web_rest`. The vllm mock answers HTTP 503
once: the agent retries the pass after the 2 s backoff, stores a `[retry 1/2]` notice
row the model reads on the next request, emits a `retrying` status, and the second
request succeeds. A 503 on every try fails loud after the retry budget, and nothing
re-runs the failed pass (#795): two requests in all. A 400 and `transport_retries: 0`
are not link-retried: one request each.

Lines: the provider holds `memory: {call: [*]}`, the chat `memory` and
`{call: [complete, watch, capabilities]}` on its provider, in their own `can`.

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --only-free --id transport_retries
```
