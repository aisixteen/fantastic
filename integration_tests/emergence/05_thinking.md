# 05 — does the thought channel survive the backend

`tmp/live_llamacpp.py` · a smoke test, not an emergence probe — but everything else
here depends on it

## Why it exists

Under ollama, a thinking model returned **427 chars of `thinking` and ZERO chars of
`content`**, `done_reason=stop`. Every "it planned and never acted" failure traced
back to that, and the workaround was to run the probes with `think=False` — so the
models were never allowed to reason at all.

## Root cause

Two things, neither of which ollama handled:

1. `gemma-4-12B-it` gates reasoning behind a chat-template kwarg
   **`enable_thinking`, default `false`** (`chat_template.jinja:186`).
2. Its thought channel is `<|channel>thought … <channel|>` — **not** `<think>` tags.

A server that sets neither the kwarg nor understands the markers produces exactly the
observed failure.

## What fixed it

`llama-server` splits the channels explicitly. Two flags:

```
--chat-template-kwargs '{"enable_thinking":true}'
--reasoning-format deepseek
```

`--reasoning-format` is the dial, one restart per setting:

| value | thoughts land in |
|---|---|
| `deepseek` | `message.reasoning_content` (what `llamacpp_backend` reads) |
| `deepseek-legacy` | `<think>` inline in `content` **and** the field |
| `none` | unparsed, inline in `content` |

The `llamacpp_backend` bundle sends `chat_template_kwargs` **per request** from the
record's `think` field, so toggling thinking is a record edit, not a server restart.

## Scoring

Two layers, because they behave differently and confusing them wasted a run:

- **provider** — `<think>…</think>` present in the raw stream when `think=True`,
  absent when `think=False`, and `content` non-empty either way.
- **kernel** — `response` non-empty with thinking on and off.

`response` is **post-strip by design** (`ai_core.core.without_thinking` removes the
wrapper before the turn is persisted), so asserting `<think> in response` tests the
wrong surface and **fails on a healthy system**. The first run of this script did
exactly that and scored FAIL. The comment is in the script now so it is not
re-learned.

## Recorded run — 2026-08-04 — ✅

```
kernel   think=False response non-empty : True
kernel   think=True  response non-empty : True
provider think=False no thought channel : True
provider think=True  thought wrapped    : True (380 ch)
provider think=True  content after it   : True (27 ch)
```

Thinking costs generation time but **not context** — the thought is stripped before
persistence, so it does not accumulate across turns.

## Local server setup

```sh
LLAMA_CACHE="$HOME/Projects/models" llama-server \
  -hf ggml-org/gemma-4-12B-it-GGUF:Q4_0 \
  --host 127.0.0.1 --port 8080 \
  -c 32768 --parallel 1 -ngl 99 \
  --jinja \
  --chat-template-kwargs '{"enable_thinking":true}' \
  --reasoning-format deepseek
```

Sizing measured on an M1 Pro: 6.9 GB model + 152 MB mmproj, ~19 tok/s generation
(memory-bandwidth bound; ~29 tok/s is the theoretical ceiling for a 12B Q4 at
~200 GB/s), 149–195 tok/s prompt processing, KV cache reused across turns so only the
new suffix is processed. `-c 32768` because [03](03_plan.md)'s full 16-pass
conversation measured 14,340 tokens and a weaker model needs more passes than that.

`llama-server` sets **CORS `*` with no API key**. It is bound to loopback, but any
page in the browser can reach it — relevant given the frontend kernel. Use
`--api-key` if that matters.

Note: `--chat-template-kwargs enable_thinking` is deprecated in b10210 in favour of
`--reasoning on|off`. The per-request path in `llamacpp_backend` sends the same
kwarg, so that deprecation will eventually need addressing.
