# E127 transport_retries

NO MODEL, Python runtime, packaged host over `web_rest`. The vllm mock answers HTTP 503
once: the agent retries the turn after the 2 s backoff, stores a `[retry 1/2]` notice
row the model reads on the next request, emits a `retrying` status, and the second
request succeeds. A 503 on every try fails loud after the retry budget, then the failed
turn is retried on its own (#533; `misfire_limit: 2`): four requests in all, and the
chain gives up. A 400 and `transport_retries: 0` are not link-retried: two requests
each (the failed turn and its own retry).

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --only-free --id transport_retries
```
