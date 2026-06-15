//! fantastic-ai-core — shared reflect-driven LLM-agent machinery behind
//! a [`Provider`] seam. The ollama / NIM (and future) backends supply
//! only their `Provider` impl + a thin `Bundle` that dispatches verbs
//! through this crate.
//!
//! Two layers (mirroring the Python `ai_core` dedup):
//!
//!  1. PROCESS-GLOBAL STATE keyed by agent id ([`state`]): the FIFO
//!     lock, in-flight task, current/queue status entries, menu cache.
//!     Keyed by id so multiple backends coexist in one kernel.
//!
//!  2. PER-BACKEND CONFIG ([`agent_loop::BackendConfig`]): the caller
//!     route (cli round-trip vs per-client inbox), tool-args-as-json
//!     (OpenAI vs ollama shape), and parallel-vs-serial tool dispatch.
//!     The [`Provider`] (built per agent by the backend) is the streaming
//!     seam. NIM-only bits (api_key verbs, Bearer client cache, 429
//!     retry, SSE parse) stay IN the nvidia crate.
//!
//! ## LLM backend contract (canonical reference)
//!
//! Every LLM backend bundle MUST implement the same verb + event shape
//! so any chat UI can retarget via a single `update_agent
//! upstream_id=<other>` with no frontend changes.
//!
//! ### Verbs (caller → backend, via `kernel.send`)
//!
//! - `send` args `{text:str, client_id:str?}` →
//!   `{response:str, final:str, client_id:str}` on success,
//!   `{error:str, client_id:str}` on failure.
//! - `history` args `{client_id:str?}` →
//!   `{messages:[{role,content,…}], client_id:str}`.
//! - `interrupt` → `{interrupted:bool}`. Cancels in-flight `send`.
//! - `reflect`, `boot`, `shutdown`, `status`, `refresh_menu`.
//!
//! ### Events (backend → caller's inbox, via the route)
//!
//! - `{type:"status", source, client_id, ts, phase, detail}` where
//!   `phase` ∈ `queued|thinking|streaming|tool_calling|done`.
//! - `{type:"token", text, source, client_id}` — one per chunk.
//! - `{type:"say", text:"[tool target -> reply]", source, client_id}`.
//! - `{type:"done", source, client_id}` — final event.

#![deny(missing_docs)]

pub mod agent_loop;
pub mod assembly;
pub mod context;
pub mod events;
pub mod helpers;
pub mod history;
pub mod projection;
pub mod provider;
pub mod state;
pub mod strategies;
pub mod verbs;

pub use agent_loop::{run_generation, BackendConfig};
pub use events::CallerRoute;
pub use helpers::DEFAULT_CLIENT_ID;
pub use provider::{Provider, ProviderEvent, ProviderStream};
pub use verbs::SEND_TIMEOUT_SECS;

#[cfg(test)]
mod tests;
