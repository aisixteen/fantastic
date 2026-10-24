//! THE TUNABLES of ai-core — every number the loop, the context machinery and the
//! fold strategy run on, in ONE place. Each keeps the note that says why it has
//! the value it has; a change here is a change to measured behaviour, so
//! re-measure before moving one.
//! Mirrors python `ai_core/constants.py`, section for section.
//!
//! The modules that use these re-export them by name
//! (`crate::strategies::fold::KEEP_TAIL`, `fantastic_ai_core::SEND_TIMEOUT_SECS`, …),
//! so every existing path keeps working. Nothing here may depend on another
//! ai-core module.

use fantastic_bundle::py_float;
use serde_json::json;

// ─── the context window (python context.py) ─────────────────────────────────

/// ~chars per token for the estimate.
pub const CHARS_PER_TOKEN: usize = 4;
/// Default window when nothing is configured.
///
/// Was 4096, which no longer fit the kernel's OWN system block: budget =
/// 4096 − 1024 output reserve = 3072 against a ~3174-token default prompt, so
/// every agent created without an explicit `context_window` failfasted
/// `too_small` before the model was ever called. The margin had reached zero
/// and a ~100-token verb-doc addition took it negative.
///
/// 128k is not a guess about any particular model: it is the window a caller
/// gets when they say NOTHING, and the honest default is one that fits this
/// kernel's own prompt with room to work. A model with a smaller real window is
/// named on the record (`context_window`, or ollama's `num_ctx`) — the explicit
/// lever always wins.
pub const DEFAULT_CONTEXT_WINDOW: i64 = 131_072;
/// Default output headroom reserved out of the window.
pub const DEFAULT_OUTPUT_RESERVE: i64 = 1024;
/// Never project to a budget below this.
pub const BUDGET_FLOOR: i64 = 256;
/// How many of the most recent turns a compaction keeps verbatim when the
/// record names no `recent_n` (clamped to [`RECENT_N_RANGE`] either way).
/// Python `RECENT_N`.
pub const RECENT_N: i64 = 6;
/// The clamp on `recent_n`. Python `RECENT_N_RANGE`.
pub const RECENT_N_RANGE: (i64, i64) = (1, 50);

// ─── the turn loop (python core.py) ─────────────────────────────────────────

/// Fold rows between automatic recalls. Python `RECALL_EVERY`.
pub const RECALL_EVERY: usize = 3;

/// THE TRIGGER: compaction fires once the calibrated prompt passes this share of
/// the budget — early enough that the summariser itself has headroom. Python
/// `TRIGGER`.
pub const TRIGGER: f64 = 0.8;

/// Default per-`send` ceiling, in seconds — overridable via the
/// `FANTASTIC_AI_SEND_TIMEOUT` env (see [`send_timeout_secs`]). Mirrors the
/// python/swift default; a slow LOCAL model can raise it via the env. Python
/// `SEND_TIMEOUT` (the hard ceiling per generation; releases the lock).
pub const SEND_TIMEOUT_SECS: f64 = 180.0;

/// The READ CLOCK on a provider stream, in seconds: the longest the endpoint may
/// go without sending a byte (headers included) before the stream is judged
/// stalled and the turn ends with an error row (#177). A record's
/// `stream_idle_seconds` replaces it; it is a bound BETWEEN bytes, never a widened
/// [`SEND_TIMEOUT_SECS`].
pub const STREAM_IDLE_SECONDS: f64 = 60.0;

/// The effective per-`send` ceiling: `FANTASTIC_AI_SEND_TIMEOUT` if set, else
/// [`SEND_TIMEOUT_SECS`]. python `SEND_TIMEOUT = float(os.environ.get(
/// "FANTASTIC_AI_SEND_TIMEOUT", "180"))`.
///
/// A FLOAT, and PARSED LIKE PYTHON'S: `0.05` is a sub-second ceiling, and the
/// ceiling printed in a refusal reads like python's (`180.0s`). A value python's
/// `float()` rejects is a MISCONFIGURATION and fails loud NAMING the variable,
/// exactly as python's import-time `float()` raises; there is no fallback to the
/// default.
///
/// # Panics
/// When `FANTASTIC_AI_SEND_TIMEOUT` is set to something python's `float()`
/// would reject.
pub fn send_timeout_secs() -> f64 {
    let Ok(raw) = std::env::var("FANTASTIC_AI_SEND_TIMEOUT") else {
        return SEND_TIMEOUT_SECS;
    };
    match py_float(&json!(raw)) {
        Ok(v) => v,
        Err(e) => panic!("FANTASTIC_AI_SEND_TIMEOUT is not a number: {e}"),
    }
}

/// Headless / REPL caller default. Matches Python's `DEFAULT_CLIENT_ID`.
pub const DEFAULT_CLIENT_ID: &str = "cli";

/// Recursion guard: AI→AI→… chains refuse past this depth (python
/// `MAX_CALL_DEPTH`). In rust the guard is the KERNEL's (`call_guard`), shared by
/// every bundle, so the value lives there; it is named here so this file lists
/// every number ai-core runs on.
pub use fantastic_kernel::call_guard::MAX_CALL_DEPTH;

// ─── folds (python folds.py) ────────────────────────────────────────────────

/// Chars handed to the summarizer per fold — never a 60k reply.
pub const SUMMARY_INPUT_CAP: usize = 24_000;

// ─── projection (python strategies/base.py) ─────────────────────────────────

/// Fixed token budget reserved at the seam for the notice WRAPPER prose
/// (everything in the notice except the summary the strategy already
/// pays for). Keeps ONE budget authority.
///
/// MEASURED, not guessed. It was 80 against a real envelope of ~132, and that
/// 50-token shortfall was not harmless: it put the strategy's projection just
/// over budget, which woke the seam's backstop, which trims from the FRONT —
/// where `compact` had just pinned the goal turn. The under-reserve was
/// therefore the mechanism that undid "PIN THE GOAL". Re-measure this if the
/// notice text changes. Python `ai_core/strategies/base.py`.
pub const NOTICE_ENVELOPE_RESERVE: i64 = 160;

// ─── the strip strategy (python strategies/strip.py) ────────────────────────

/// Strip down to this fraction of the budget once over it.
pub const LOW: f64 = 0.6;

// ─── the fold strategy (python strategies/fold.py) ──────────────────────────

/// The longest a fold title may be, in chars.
pub const MAX_TITLE: usize = 140;

/// THE SUMMARY BUDGET: a fold summary gets a FIXED 1024
/// tokens, and the model is told exactly that number; the fold's mechanical index
/// rides beside it, outside the budget. A budget derived from the span (a fifth of
/// it, minus the index) puts short, user-heavy chat spans on a tiny floor the
/// model overshoots, and nothing folds.
pub const SUMMARY_BUDGET: i64 = 1024;
/// Re-summarises after the first answer; past them the fold fails LOUD (the turn
/// ends `reason: fold_summary`), never a silent unfolded span.
pub const SUMMARY_RETRIES: usize = 3;
/// THE THINKING ALLOWANCE on a summary call's `max_tokens`.
/// The endpoint counts a reasoning model's thought against `max_tokens`; at a bare
/// budget (as small as ~93 tokens for a short span) the thought alone hit the
/// bound, every small summary was cut and refused, and the last-resort cut dropped
/// the fold that held the run's decision. The SUMMARY TEXT
/// — thought stripped — must still fit the budget; the allowance is for the
/// thinking only.
pub const THINK_ALLOWANCE: i64 = 1024;
/// The user's own words, verbatim, at most this much per fold.
pub const INDEX_USER_CHARS: usize = 1200;
/// Rows at the end of the view that never fold — the live tail.
pub const KEEP_TAIL: usize = 6;
/// WHEN AND HOW MUCH TO FOLD — shares of the body budget, never a row count. A
/// fold starts once the view passes `FOLD_AT` of the
/// budget; ONE fold then takes the oldest foldable rows, by tokens, until the view
/// lands at `LOW` or under. A row count folds a chat a few rows at a time into
/// fold rows nearly as big as the rows: many folds, several levels, and a long
/// wait before the first answer.
pub const FOLD_AT: f64 = 0.95;
/// Adjacent folds at the head of the view before they fold as a group.
///
/// THREE, not four. A real run folds at most once per turn — each projection
/// drops the view under the watermark and stops — so the head accumulates folds
/// one at a time and this threshold is the whole story. At four it never fires:
/// the head ends `levels [1]` and grows one fold per span forever, which is the
/// growth this constant exists to stop.
///
/// Three still skips-then-batches, so depth grows as log₃ of the history rather
/// than one level per cut.
pub const FOLD_RUN: usize = 3;

// ─── the no-call reminder (python core.py) ──────────────────────────────────

/// A chain turn that dispatches no call is told to TRY AGAIN (emit a call, or end
/// the chain with `done`/`await`). MAX_SILENT_TURNS in a row — prose-only turns
/// and dead turns (only thinking) alike, reset by any call — and the chain GIVES
/// UP: no more reminders, the caller is told the model could not answer (failing
/// on the first dead turn would end long chains early; an unbounded reminder loop
/// would never end).
pub const MAX_SILENT_TURNS: usize = 10;

// ─── inline agent views (#339) ──────────────────────────────────────────────

/// The tallest a chat card may be, in CSS px. A card fills the chat column's
/// width and its panel defines its height; content taller than this scrolls
/// inside the card. The canvas's TS twin (`chat_card.ts`) says the same 1200 —
/// a test reads both.
pub const CHAT_CARD_MAX_HEIGHT: i64 = 1200;
