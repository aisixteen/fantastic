# `shared_provider` (E165) — two chats on one provider

Free: no model. Scripted OpenAI-compatible servers (`e2e/mocks/vllm.py`) answer for ONE
`vllm_backend` provider; two `ai_chat.tools` chats name it (#632). Rust lane only
(`runtimes: [rust]`), driven over `web_rest` on the packaged host.

1. **lines** — the creator (the `rest` leg) names the provider for each chat and writes the
   chat's own line on it (`can: {shared_prov: {call: [complete, watch, capabilities]}}`); the
   provider's record is unchanged by either create; a writer `w` holding `update: [can]` on
   `chat_b` but no line on the provider cannot hand `chat_b` a line on it.
2. **own knobs** — each chat's request carries its own `temperature` (0.3 and 0.9), `max_tokens`
   (8192 and 4096) and, for one, `top_k`; a knob a chat leaves out is the backend's default.
3. **KV accounting** — the provider's `reflect` shows both conversations live on its endpoint.
4. **lifecycle** — the provider's `endpoint` changed to a second server: the next request of a
   chat on it goes there; both chats deleted, the provider stays.

```sh
RUNTIME=rust uv run python e2e/tests/kernel/E165_shared_provider/probe.py
```
