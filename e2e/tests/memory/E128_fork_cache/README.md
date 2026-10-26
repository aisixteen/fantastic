# E128 fork cache

TURNS tier, the Anthropic plug (`BACKEND=anthropic`, see `e2e/README.md`); without the plug
or its key the run prints `E2E-SKIP:` and is reported as skipped. Python runtime.

An agent with `caching: provider_cache` is given a long first message (the primer, named by
`cache_primer_until`) and takes two turns, so the provider has cached the prefix. It marks
its history, and a fork of that mark takes its first send. Scored on the fork's
cache-read tokens (the backend's own count of what the API reported) being above zero:
the fork's prompt starts with the parent's bytes, so it hits the parent's cache.

```sh
cd src/lib/python && BACKEND=anthropic ANTHROPIC_ENDPOINT=https://api.anthropic.com/v1/messages \
ANTHROPIC_API_KEY=… FANTASTIC_AI_MODEL=<model id> \
uv run python ../../../e2e/tests/memory/E128_fork_cache/probe.py
```
