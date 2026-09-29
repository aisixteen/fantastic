# `backends/` — the inference plug, measured at the wire and through the kernel

Every other group measures a model doing work; this one measures the PLUG the work
rides on. A wire fault and a kernel fault look identical from inside a long probe (an
empty turn, a chain that dies), so each wire is first measured RAW — no kernel, no
provider — and then the provider is measured as ai_core uses it.

| test | backend | what it measures |
|---|---|---|
| `wire_anthropic` | `anthropic` only | the Messages stream: `message_start … message_stop`, text in `text_delta`, thinking on its own channel before the text, `end_turn`; no `tool_use` without `tools`; a `max_tokens` cut reports `max_tokens` and still closes; a malformed request is a deliberate 4xx JSON |
| `wire_responses` | `responses` only | the Responses API: non-stream `object:response`/`completed` with an `output_text` message; stream `response.created … response.completed`, rising `sequence_number`, deltas == `output_text.done` == the final message, reasoning before output; a `max_output_tokens` cut ends `response.incomplete` (streamed and not); 400 for native `tools`, `store:true`, `previous_response_id` |
| `provider_contract` | any | through three live AI agents: tokens stream, a `<call>` runs and its reply lands, the answer carries a planted word, no stored row carries thinking; a 16-token budget makes the main turn FAIL naming the cut with no assistant row stored; `ps` → `cancel` mid-stream settles `cancelled`, closes `done/cancelled`, and stops the tokens |

The Responses expectations follow the contract agreed with the proxy owner on
2026-09-27 (raw text, `reasoning_text`, a cut ends `response.incomplete`, stateless,
native tools refused).

## Running

```sh
cd src/lib/python
set -a; . ../../../e2e/backends/connection.local.env; set +a

# the Anthropic plug (ENDPOINT = …/v1/messages)
BACKEND=anthropic uv run python ../../../e2e/run.py --group backends --lanes 1

# the Responses plug — its own URL and key names, nothing guessed
BACKEND=responses RESPONSES_ENDPOINT=<…/v1/responses> OPENAI_API_KEY=<key> \
  uv run python ../../../e2e/run.py --group backends --lanes 1
```
