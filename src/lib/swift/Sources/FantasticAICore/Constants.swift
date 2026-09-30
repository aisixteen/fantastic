// THE TUNABLES of the AI core — every number the loop, the context machinery and
// the fold strategy run on, in ONE place (owner, 2026-09-30: "all constants to
// constant file"). Each keeps the note that says why it has the value it has; a
// change here is a change to measured behaviour, so re-measure before moving one.
//
// Python `ai_core/constants.py` is the reference: same names, same values, same
// notes. Every use site names `AIConstants.<NAME>` directly — there is no second
// spelling of any of these anywhere in the target.

import FantasticJSON
import FantasticKernel
import Foundation

public enum AIConstants {
    // ─── the context window (Context.swift) ────────────────────────────────

    /// ~chars per token for the estimate.
    public static let CHARS_PER_TOKEN = 4
    /// The window a caller gets when they say NOTHING.
    ///
    /// 4096 until 2026-08-24, and by then it did not fit the kernel's OWN system
    /// block. Measured: budget = 4096 − 1024 output reserve = 3072, against a
    /// default system prompt of ~3174 tok (identity + tree + bundle catalog + the
    /// send and context howtos). Every agent created without an explicit
    /// `context_window` failfasted with `too_small` before the model was ever
    /// called — the margin had quietly reached zero, and a ~100-token addition to
    /// a verb doc was enough to take it negative.
    ///
    /// 128k is not a guess about any particular model: it is the window a caller
    /// gets when they say NOTHING, and the honest default is one that fits this
    /// kernel's own prompt with room to work. A model with a smaller real window is
    /// named on the record (`context_window`, or ollama's `num_ctx`) — the explicit
    /// lever always wins, and the compaction path is exercised by whatever value is
    /// set, not by this constant.
    public static let DEFAULT_CONTEXT_WINDOW: Int64 = 131_072
    /// Default output headroom reserved out of the window.
    public static let DEFAULT_OUTPUT_RESERVE: Int64 = 1024
    /// Never project to a budget below this.
    public static let BUDGET_FLOOR: Int64 = 256

    // ─── the turn loop (AIBackend.swift, ContextRecall.swift) ───────────────

    /// Fold rows between automatic recalls.
    public static let RECALL_EVERY = 3
    /// THE TRIGGER: compaction fires once the calibrated prompt passes this share
    /// of the budget — early enough that the summariser itself has headroom.
    public static let TRIGGER = 0.8
    /// Hard ceiling per generation; on expiry the lock is released and the
    /// generation is force-stopped with a terminal `done{error:"send: timeout …"}`.
    /// Bounds runaway tool loops / a stuck provider. Env-overridable
    /// (`FANTASTIC_AI_SEND_TIMEOUT`, read once) — slow local models need more.
    ///
    /// FAILS LOUD ON A BAD VALUE, exactly as python's module-level
    /// `float(os.environ.get(…, "180"))` does: the runtime does not come up. Swift
    /// once parsed with `Double(raw)` and SILENTLY FELL BACK to 180 on anything it
    /// could not read (`"0"` included), so a mis-set variable produced a kernel
    /// that looked configured and was not.
    public static let SEND_TIMEOUT: Double = {
        guard let raw = ProcessInfo.processInfo.environment["FANTASTIC_AI_SEND_TIMEOUT"] else {
            return 180
        }
        do {
            return try pyFloat(.string(raw))
        } catch {
            fatalError("FANTASTIC_AI_SEND_TIMEOUT: \(error)")
        }
    }()
    /// Headless / REPL caller defaults here.
    public static let DEFAULT_CLIENT_ID = "cli"
    /// Recursion guard: AI→AI→… chains refuse past this depth.
    public static let MAX_CALL_DEPTH = 8

    // ─── folds (Folds.swift) ───────────────────────────────────────────────

    /// Chars handed to the summarizer per fold — never a 60k reply.
    public static let SUMMARY_INPUT_CAP = 24_000

    // ─── projection (Strategies.swift) ─────────────────────────────────────

    /// A fixed token budget reserved at the seam for the notice WRAPPER prose
    /// (everything in the notice except the summary itself, which the strategy
    /// already pays for). Keeps ONE budget authority — the strategy nets this out
    /// before fitting its tail.
    ///
    /// MEASURED, not guessed. It was 80 against a real envelope of ~132, and that
    /// 50-token shortfall was not harmless: it put the strategy's projection just
    /// over budget, which woke the seam's backstop, which trims from the FRONT —
    /// where `compact` had just pinned the goal turn. The under-reserve was
    /// therefore the mechanism that undid "PIN THE GOAL". Re-measure this if the
    /// notice text changes.
    public static let NOTICE_ENVELOPE_RESERVE: Int64 = 160

    // ─── the strip strategy (StripStrategy.swift) ──────────────────────────

    /// Strip down to this fraction of the budget once over it.
    public static let LOW = 0.6

    // ─── the fold strategy (FoldStrategy.swift) ────────────────────────────

    /// The longest a fold title may be, in chars.
    public static let MAX_TITLE = 140
    // THE SUMMARY BUDGET — this strategy's own defaults (the per-agent strategy
    // slot is where they become configurable; not exposed now). A summary may take
    // a fifth of the span it replaces, and never more than a quarter of the body
    // budget: a fold that is itself a large fraction of the window buys nothing.
    public static let SUMMARY_SHARE = 0.20
    public static let WINDOW_SHARE = 0.25
    /// Tokens — a title and one sentence always fit.
    public static let SUMMARY_FLOOR: Int64 = 48
    /// THE THINKING ALLOWANCE on a summary call's `max_tokens` (decided
    /// 2026-09-26). The endpoint counts a reasoning model's thought against
    /// `max_tokens`; at a bare budget (as small as ~93 tokens for a short span)
    /// the thought alone hit the bound, every small summary was cut and refused,
    /// and the last-resort cut dropped the fold that held the run's decision
    /// (measured, E81). The SUMMARY TEXT — thought stripped — must still fit the
    /// budget; the allowance is for the thinking only.
    public static let THINK_ALLOWANCE: Int64 = 1024
    /// The user's own words, verbatim, at most this much per fold.
    public static let INDEX_USER_CHARS = 1200
    /// Rows at the end of the view that never fold — the live tail.
    public static let KEEP_TAIL = 6
    /// Rows per fold at the watermark — small enough to summarize, large enough
    /// to matter.
    public static let SPAN = 8
    /// Adjacent folds at the head of the view before they fold as a group.
    ///
    /// THREE, not four. A real run folds at most once per turn — each projection
    /// drops the view under the watermark and stops — so the head accumulates
    /// folds one at a time and this threshold is the whole story. At four it
    /// never fired: measured 2026-09-11, `nested_folds` and `flat_roots` both
    /// ended `levels [1]`, a head that grows one fold per span forever, which is
    /// the growth this constant exists to stop.
    ///
    /// Three still skips-then-batches, so depth grows as log₃ of the history
    /// rather than one level per cut (the 2026-09-06 measurement: twelve levels in
    /// 26 cuts, when every span began on the previous fold).
    public static let FOLD_RUN = 3

    // ─── the no-call reminder (Routing.swift) ──────────────────────────────

    /// A chain turn that dispatches no call is told to TRY AGAIN (emit a call, or
    /// end the chain with `done`/`await`). MAX_SILENT_TURNS in a row — prose-only
    /// turns and dead turns (only thinking) alike, reset by any call — and the
    /// chain GIVES UP: no more reminders, the caller is told the model could not
    /// answer (owner, 2026-09-30, after a single dead turn ended a 30-pass chain
    /// and an unbounded reminder loop was the only alternative).
    public static let MAX_SILENT_TURNS = 10
}
