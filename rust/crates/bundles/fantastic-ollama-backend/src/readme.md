# ollama_backend — local LLM agent
Talks to a local ollama. Per-client chat threads, FIFO lock, native tool-calls, menu cache. Persistence via `file_agent_id`.

This crate is the canonical Rust reference implementation of the
**LLM backend contract**: every other backend (nvidia_nim, future
apple_fm, etc.) must speak the same verbs and emit the same events
so a chat UI (`ai_chat_webapp`) can retarget by changing one record
field. See the module header in `lib.rs` for the full contract.
