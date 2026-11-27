# `chat_provider_split` (E150) — a chat and its provider apart

Free: no model. Scripted OpenAI-compatible servers (`e2e/mocks/vllm.py`) answer for
`vllm_backend` PROVIDERS; the chat is `ai_chat.tools`, its `provider_id` naming one. Rust lane
only (`runtimes: [rust]`), driven over `web_rest` on the packaged host.

1. **local split** — the chat answers `send`; its record names the provider and holds no
   `endpoint`; its `history` holds the transcript; the provider's holds none.
2. **swap** — `update_agent` the chat's `provider_id` to a second provider (another server): the
   next pass reaches the second server and its request carries the earlier rows.
3. **lines** — every line is the caller's own `can`: a chat with no line on its provider is
   refused by the provider's name (`cannot watch provider …`) and the server sees nothing; a
   chat holding the line completes and the provider's record is byte-equal before and after;
   a chat holding the line under `mid`, an ancestor with no line on the provider, is cut by the
   cap (refused by the provider's name, the server sees nothing).
4. **across the bridge** (muted, #763) — the provider lives on a second kernel that dials the
   first through a `ws_bridge` (as `E118_kernel_bridges`); the chat names it `<guid>|<id>` and
   completes there.

```sh
RUNTIME=rust uv run python e2e/tests/kernel/E150_chat_provider_split/probe.py
```
