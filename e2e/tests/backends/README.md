# `backends/` — the inference plug, measured at the wire and through the kernel

Every other group measures a model doing work; this one measures the PLUG the work
rides on. A wire fault and a kernel fault look identical from inside a long probe (an
empty turn, a chain that dies), so each wire is first measured RAW — no kernel, no
provider — and then the provider is measured as ai_core uses it.

| test | backend | what it measures |
|---|---|---|
| `wire_responses` | `responses` only | the Responses API: non-stream `object:response`/`completed` with an `output_text` message; stream `response.created … response.completed`, rising `sequence_number`, deltas == `output_text.done` == the final message, reasoning before output; a `max_output_tokens` cut ends `response.incomplete` (streamed and not); 400 for native `tools`, `store:true`, `previous_response_id` |
| `provider_contract` | any | through three live AI agents: tokens stream, a `<call>` runs and its reply lands, the answer carries a planted word, no stored row carries thinking; a 16-token budget makes the main turn FAIL naming the cut with no assistant row stored; `ps` → `cancel` mid-stream settles `cancelled`, closes `done/cancelled`, and stops the tokens |
| `system_prompt_file` (E99) | none (free) | NO MODEL — a scripted Responses endpoint records each request's `instructions`; on python · rust · swift (packaged host, `web_rest`): an AI agent's `system_prompt_file: {bridge, path}` read THROUGH a `file_bridge` as the agent — placeholders filled (`{store}`, `{self}`, an unknown `{…}` kept), an editor's save and an atomic save live on a following turn (`watch_path`), record `system_prompt` > file > default, `{tree}\n\n{self}\n\n{howto}` == the default block byte for byte, a deleted file or an ungranted bridge → the default block with reflect's `system_prompt_file_error`, a malformed value refused at the write |

The Responses expectations follow the contract agreed with the proxy owner on
2026-09-27 (raw text, `reasoning_text`, a cut ends `response.incomplete`, stateless,
native tools refused).

## Running

`system_prompt_file` needs no plug: `uv run python ../../../e2e/run.py --only-free --id system_prompt_file`
(or `RUNTIME=rust uv run python ../../../e2e/tests/backends/E99_system_prompt_file/probe.py`).

```sh
cd src/lib/python
set -a; . ../../../e2e/backends/connection.local.env; set +a

# the Responses plug — its own URL and key names, nothing guessed
BACKEND=responses RESPONSES_ENDPOINT=<…/v1/responses> OPENAI_API_KEY=<key> \
  uv run python ../../../e2e/run.py --group backends --lanes 1

# the vLLM plug — `provider_contract` only (there is no raw vLLM wire probe)
BACKEND=vllm VLLM_ENDPOINT=<…/v1> VLLM_KV_SLOTS=<n> VLLM_KV_OVERFLOW=<evict|reject> \
  uv run python ../../../e2e/run.py --group backends --id provider_contract --lanes 1
```
