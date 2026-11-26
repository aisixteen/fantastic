//! fantastic-ai-core — shared machinery for **context-managed completion**
//! backends behind a [`Provider`] seam. The ollama / responses / vllm backends
//! supply only their `Provider` impl + a thin `Bundle` that dispatches verbs
//! through this crate.
//!
//! An AI agent is a MACHINE WOKEN BY MESSAGES (#795, [`life`]): every message is
//! stored and acked, and one pass at a time reads every message not read yet — **no
//! loop, no agent menu, no waits**. Tool-calling IS this crate's, and RAW: the
//! provider's answer is plain text, [`tool_parse`] reads the `<call name=…>` envelope
//! out of it, and [`route`] sends each call the answer named; each reply comes back
//! later as a message to its own `reply_to`. The model decides what to answer, when
//! and to whom. Nothing iterates on the model's behalf, and no provider's native tool
//! API is ever used.
//!
//! The other thing the backend owns, because it owns the per-client conversation,
//! is **context management**: per-client history, auto-compaction on overflow, and
//! the `recall` / `context_status` verbs (see [`projection`], [`strategies`],
//! [`context`]).
//!
//! Two layers (mirroring the Python `ai_core`):
//!
//!  1. PROCESS-GLOBAL STATE keyed by agent id ([`state`]): the agent's life (a pass
//!     running, the unread messages, the calls in flight), the pass's status entry,
//!     projection/compaction bookkeeping. Keyed by id so multiple backends coexist in
//!     one kernel. No task handle — the kernel's task table owns in-flight work.
//!
//!  2. PER-BACKEND CONFIG ([`agent_loop::BackendConfig`]): the caller route
//!     (cli round-trip vs per-client inbox) + error-name prefix. The
//!     [`Provider`] (built per agent by the backend) is the streaming seam. A
//!     backend's KEY is read from a credential store before each request
//!     ([`credentials`]) — never the environment, never the record.
//!
//! ## LLM backend contract
//!
//! ### Verbs
//!
//! Declared once in [`declared`] (one struct per verb); `reflect` and `docs` render
//! them. A `send` stores its message and acks (`{accepted, re, client_id}`); the pass
//! that reads it routes the model's own `<call>`s ([`route`]). A message that lands
//! while a pass runs is read by exactly one next pass.
//!
//! There is NO `interrupt` verb: a pass's request is a kernel execution (the
//! provider's `complete`), so it is cancelled with the SUBSTRATE's universal `ps` +
//! `cancel`, and the cancelled settle ends the pass as stopped (its open parts abort,
//! a marker row, `status(done, reason=cancelled)`). `stop` does the same from the
//! agent's side. Cancellation belongs to the one primitive, not to one bundle.
//!
//! ### Events (backend → caller's inbox, via the route)
//!
//! - `{type:"status", source, client_id, ts, phase, detail}` where `phase` ∈
//!   `thinking|streaming|retrying` while a pass runs; a pass ENDS on
//!   `done` (reason `ok`, `cancelled`) or `failed` (reason `error`, `timeout`,
//!   `gave_up`); outside a call, `paused` (the pause verb, or a boot whose store says paused)
//!   and `idle` (a resume with nothing held).
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
pub mod chat_bundle;
pub mod declared;
pub mod call_check;
pub mod constants;
pub mod context;
pub mod credentials;
pub mod embed;
pub mod events;
pub mod folds;
pub mod fork;
pub mod helpers;
pub mod history_view;
mod images;
mod life;
pub mod steps;
pub mod line_summary;
pub mod memory;
mod model_format;
pub mod pause;
pub mod projection;
pub mod prompt_file;
pub mod provider;
pub mod remote_provider;
pub mod retrieval;
pub mod route;
mod row_attrs;
pub mod record;
pub mod state;
pub mod strategies;
pub mod stream_parts;
pub mod think;
pub mod tool_parse;
mod used;
pub mod verbs;
pub mod view;

pub use agent_loop::BackendConfig;
pub use constants::{CHAT_CARD_MAX_HEIGHT, USAGE_KEYS};
pub use provider::{Provider, ProviderEvent, ProviderStream};
pub use think::{without_thinking, ThinkWrap, THINK_CLOSE, THINK_OPEN};
pub use verbs::SEND_TIMEOUT_SECS;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_decl;
#[cfg(test)]
mod tests_context_none;
#[cfg(test)]
mod tests_credentials;
#[cfg(test)]
mod tests_folds;
#[cfg(test)]
mod tests_model_window;
#[cfg(test)]
mod tests_pause;
#[cfg(test)]
mod tests_st306;
#[cfg(test)]
mod tests_steps;
