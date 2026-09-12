//! fantastic-ai-core — shared machinery for **context-managed completion**
//! backends behind a [`Provider`] seam. The ollama / NIM / anthropic backends
//! supply only their `Provider` impl + a thin `Bundle` that dispatches verbs
//! through this crate.
//!
//! A backend does ONE provider pass per `send` — **no loop, no agent menu**. But
//! tool-calling IS this crate's, and RAW: the provider streams plain text, [`tool_parse`]
//! reads the `<call name=…>` envelope out of it, and [`route`] dispatches what the
//! completion named, delivering each reply to its own `reply_to`. A chain advances
//! one pass at a time and only because the model asked again — no call ends the
//! turn, and stop is the default. Nothing iterates on the model's behalf, and no
//! provider's native tool API is ever used.
//!
//! (This header said "no tool parsing" and called the loop emergent until
//! 2026-08-06. It was lagging, not principled: python superseded that doctrine on
//! 2026-08-02, and every backend crate in this workspace already documented the
//! parse as ai-core's. One of two stale copies of a fact is worse than none.)
//!
//! The other thing the backend owns, because it owns the per-client conversation,
//! is **context management**: per-client history, auto-compaction on overflow, and
//! the `recall` / `context_status` verbs (see [`projection`], [`strategies`],
//! [`context`]).
//!
//! Two layers (mirroring the Python `ai_core`):
//!
//!  1. PROCESS-GLOBAL STATE keyed by agent id ([`state`]): the FIFO lock,
//!     current/queue status entries, projection/compaction bookkeeping. Keyed by
//!     id so multiple backends coexist in one kernel. No task handle — the
//!     kernel's task table owns in-flight work.
//!
//!  2. PER-BACKEND CONFIG ([`agent_loop::BackendConfig`]): the caller route
//!     (cli round-trip vs per-client inbox) + error-name prefix. The
//!     [`Provider`] (built per agent by the backend) is the streaming seam.
//!     NIM-only bits (api_key verbs, Bearer client cache, 429 retry, SSE
//!     parse) stay IN the nvidia crate.
//!
//! ## LLM backend contract (canonical reference)
//!
//! ### Verbs (caller → backend, via `kernel.send`)
//!
//! - `send` args `{text:str, client_id:str?, system_prompt:str?, send_to:str?}` →
//!   `{response:str, final:str, client_id:str}` on success,
//!   `{error:str, client_id:str}` on failure. ONE pass, then [`route`] honours
//!   where its output was addressed: the model's own `<call>`s first, else
//!   `send_to` hands the pass's TEXT to that agent id once (no propagation). A
//!   `system_prompt` override replaces the default system block whole.
//!   `_call_stack` is reserved for the recursion guard, refused BEFORE the FIFO
//!   lock so a cycle cannot deadlock on it.
//! - `history` args `{client_id:str?}` → `{messages, client_id}`.
//! - `recall` args `{client_id:str?, query:str?, limit:int?, before:int?}`.
//! - `context_status` → the budget posture + last compaction + reaction.
//! - `reflect`, `boot`, `shutdown`, `status`.
//!
//! There is NO `interrupt` verb: a `send` IS a kernel task, so an in-flight
//! generation is cancelled with the SUBSTRATE's universal `ps` + `cancel`
//! (`send "kernel" {type:"cancel", task_id:"<agent>:<n>"}`) — which runs this
//! crate's [`verbs::on_cancel`] hook (close the turn out: clear the in-flight
//! entry, emit the final `status(done, reason=cancelled)` + `done`) and then
//! drops the generation's future, freeing the provider stream and the FIFO
//! guard. Cancellation belongs to the one primitive, not to one bundle.
//!
//! ### Events (backend → caller's inbox, via the route)
//!
//! - `{type:"status", source, client_id, ts, phase, detail}` where
//!   `phase` ∈ `queued|thinking|streaming|done`.
//! - `{type:"token", text, source, client_id}` — one per chunk.
//! - `{type:"done", source, client_id}` — final event.
//! - `{type:"context", ...}` — the compaction push half (see [`projection`]).
//!
//! ### Reasoning channel
//!
//! A reasoning model's deliberation arrives on a SEPARATE provider channel
//! (ollama `message.thinking`, NIM `reasoning_content`). A provider that reads
//! only the content channel reports a reasoning turn as an EMPTY completion, so
//! every provider reads BOTH and wraps the deliberation in
//! [`think::THINK_OPEN`]/[`think::THINK_CLOSE`] — **once per turn**. The
//! completion is then parsed RAW (a tool call written mid-thought still counts)
//! and persisted STRIPPED (see [`think::without_thinking`]).

#![deny(missing_docs)]

pub mod agent_loop;
pub mod assembly;
pub mod context;
pub mod events;
pub mod folds;
pub mod helpers;
pub mod history;
pub mod history_view;
pub mod projection;
pub mod provider;
pub mod retrieval;
pub mod route;
pub mod state;
pub mod strategies;
pub mod think;
pub mod tool_parse;
pub mod verbs;
pub mod view;

pub use agent_loop::{run_generation, BackendConfig};
pub use events::CallerRoute;
pub use helpers::DEFAULT_CLIENT_ID;
pub use provider::{Provider, ProviderEvent, ProviderStream};
pub use route::route_reply;
pub use think::{without_thinking, ThinkWrap, THINK_CLOSE, THINK_OPEN};
pub use verbs::{CANCEL_DOC, SEND_TIMEOUT_SECS};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_folds;
