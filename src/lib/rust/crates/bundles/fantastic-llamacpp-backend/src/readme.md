llamacpp_backend — local `llama-server` LLM agent (OpenAI-compatible); thin over fantastic_ai_core.

Verbs: send, history, recall, context_status, status, reflect. No api_key verb: a local
server usually has none, so this backend is available by default rather than refusing.

Tool-calling is RAW: the provider streams text and ai_core parses the
`<tool_call>`/`<tool_response>` envelope (tool_parse). No native tools array is sent.

`endpoint` defaults to http://127.0.0.1:8080/v1.

`think` toggles the chat template's `enable_thinking` PER REQUEST — gemma-4 gates its
thought channel behind it and defaults it FALSE; a server that sets neither streams
thinking text with EMPTY content. Unset means the kwarg is not sent at all, because a
model without a thinking gate rejects an unknown one.

The MODEL and the CONTEXT WINDOW belong to the server (`-hf`/`-m`, `-c N`), not to the
record: llama-server serves exactly one model and ignores the request's `model` field.
