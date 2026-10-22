//! fantastic-ai-core — shared machinery for **context-managed completion**
//! backends behind a [`Provider`] seam. The ollama / responses / vllm backends
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
//!     [`Provider`] (built per agent by the backend) is the streaming seam. A
//!     backend's KEY is read from a credential store before each request
//!     ([`credentials`]) — never the environment, never the record.
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
//! - `stop` args `{stream_id:str?}` → `{stopped:bool}` — a receiver of this
//!   agent's streaming parts stops the generation in flight (the provider stream
//!   closes, open parts abort, the turn keeps a `[stopped by <caller>]` marker).
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
//! - STREAMING PARTS ([`stream_parts`]): the output split as it
//!   is written into `text` / `thinking` / `call` parts — `part_start {stream_id,
//!   source, kind, name?, target?, to?, re?}` (`re`: the message this turn
//!   answers) → `part_delta {text}` (batched 50 ms) → `part_end {ok?}` (`ok` on a
//!   call: whether it parsed) | `part_abort {reason}`. A call part goes to its
//!   addressee's inbox (each of `to`) as an emit once `<target_id>`/`<to>` has
//!   closed; every other part goes where the tokens go. ANIMATION, never action:
//!   a call with no `</call>` is not a call, and a CUT pass leaves a marker row
//!   ([`stream_parts::cut_marker`]), never a half answer. The MESSAGE a call part
//!   becomes carries its `stream_id` (matched by exact body), and the receiving
//!   AI's header names it: `[from X · msg N · re M · stream <id>]`.
//!
//! ### Reasoning channel
//!
//! A reasoning model's deliberation arrives on a SEPARATE provider channel
//! (ollama `message.thinking`, vLLM `reasoning_content`). A provider that reads
//! only the content channel reports a reasoning turn as an EMPTY completion, so
//! every provider reads BOTH and wraps the deliberation in
//! [`think::THINK_OPEN`]/[`think::THINK_CLOSE`] — **once per turn**. Dispatch and
//! history then read only the ANSWER (see [`think::without_thinking`]): a call
//! written inside thinking never runs.

#![deny(missing_docs)]

pub mod agent_loop;
pub mod assembly;
pub mod caching;
pub mod call_check;
pub mod constants;
pub mod context;
pub mod credentials;
pub mod events;
pub mod folds;
pub mod helpers;
pub mod history_view;
pub mod memory;
pub mod projection;
pub mod prompt_file;
pub mod provider;
pub mod retrieval;
pub mod route;
pub mod schema;
pub mod state;
pub mod strategies;
pub mod stream_parts;
pub mod think;
pub mod tool_parse;
pub mod verbs;
pub mod view;

pub use agent_loop::{run_generation, BackendConfig};
pub use helpers::DEFAULT_CLIENT_ID;
pub use provider::{Provider, ProviderEvent, ProviderStream};
pub use route::route_reply;
pub use think::{without_thinking, ThinkWrap, THINK_CLOSE, THINK_OPEN};
pub use verbs::{CANCEL_DOC, SEND_TIMEOUT_SECS};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_context_none;
#[cfg(test)]
mod tests_credentials;
#[cfg(test)]
mod tests_folds;
#[cfg(test)]
mod tests_model_window;
