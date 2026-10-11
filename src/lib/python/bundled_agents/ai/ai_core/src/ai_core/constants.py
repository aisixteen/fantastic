"""THE TUNABLES of ai_core — every number the loop, the context machinery and the
fold strategy run on, in ONE place (owner, 2026-09-30: "all constants to constant
file"). Each keeps the note that says why it has the value it has; a change here
is a change to measured behaviour, so re-measure before moving one.

The modules import these by name, so each is still reachable where it is used
(`ai_core.core.SEND_TIMEOUT`, …) and a test that patches the module's name keeps
working. Nothing here may import another ai_core module.
"""

from __future__ import annotations

import os

# ─── the context window (context.py) ───────────────────────────────────────

CHARS_PER_TOKEN = 4
# 4096 until 2026-08-24, and by then it did not fit the kernel's OWN system block.
# Measured: budget = 4096 - 1024 output reserve = 3072, against a default system
# prompt of ~3174 tok (identity + tree + bundle catalog + the send and context
# howtos). Every agent created without an explicit `context_window` failfasted with
# `too_small` before the model was ever called — the margin had quietly reached zero,
# and a ~100-token addition to a verb doc was enough to take it negative.
#
# 128k is not a guess about any particular model: it is the window a caller gets when
# they say NOTHING, and the honest default is one that fits this kernel's own prompt
# with room to work. A model with a smaller real window is named on the record
# (`context_window`, or ollama's `num_ctx`) — the explicit lever always wins, and the
# compaction path is exercised by whatever value is set, not by this constant.
DEFAULT_CONTEXT_WINDOW = 131072
DEFAULT_OUTPUT_RESERVE = 1024
BUDGET_FLOOR = 256  # never project to a budget below this
# How many of the most recent turns a compaction keeps verbatim when the record
# names no `recent_n` (clamped to RECENT_N_RANGE either way).
RECENT_N = 6
RECENT_N_RANGE = (1, 50)

# ─── the turn loop (core.py) ───────────────────────────────────────────────

RECALL_EVERY = 3  # fold rows between recalls
# THE TRIGGER: compaction fires once the calibrated prompt passes this share of the
# budget — early enough that the summariser itself has headroom.
TRIGGER = 0.8
SEND_TIMEOUT = float(
    os.environ.get("FANTASTIC_AI_SEND_TIMEOUT", "180")
)  # hard ceiling per-generation; releases the lock (env-overridable — slow local models need more)
DEFAULT_CLIENT_ID = "cli"  # headless / REPL caller defaults here
MAX_CALL_DEPTH = 8  # recursion guard: AI→AI→… chains refuse past this depth

# ─── folds (folds.py) ──────────────────────────────────────────────────────

SUMMARY_INPUT_CAP = (
    24_000  # chars handed to the summarizer per fold — never a 60k reply
)

# ─── projection (strategies/base.py) ───────────────────────────────────────

# A fixed token budget reserved at the seam for the notice WRAPPER prose (everything in
# the notice except the summary itself, which the strategy already pays for). Keeps ONE
# budget authority — the strategy nets this out before fitting its tail.
#
# MEASURED, not guessed. It was 80 against a real envelope of ~132, and that
# 50-token shortfall was not harmless: it put the strategy's projection just over
# budget, which woke the seam's backstop, which trims from the FRONT — where
# `compact` had just pinned the goal turn. The under-reserve was therefore the
# mechanism that undid "PIN THE GOAL". Re-measure this if the notice text changes.
NOTICE_ENVELOPE_RESERVE = 160

# ─── the strip strategy (strategies/strip.py) ──────────────────────────────

LOW = 0.6  # strip down to this fraction of the budget once over it

# ─── the fold strategy (strategies/fold.py) ────────────────────────────────

MAX_TITLE = 140
# THE SUMMARY BUDGET (owner, 2026-10-11): a fold summary gets a FIXED 1024 tokens, and
# the model is told exactly that number; the fold's mechanical index rides beside it,
# outside the budget. The old formula — a fifth of the span, minus the index — put
# short, user-heavy chat spans on a 48-token floor: the model overshot, the one
# re-summarise was cut while it counted tokens, and nothing folded (measured, E39).
SUMMARY_BUDGET = 1024
# Re-summarises after the first answer; past them the fold fails LOUD (the turn ends
# `reason: fold_summary`), never a silent unfolded span (owner, 2026-10-11).
SUMMARY_RETRIES = 3
# THE THINKING ALLOWANCE on a summary call's `max_tokens` (decided 2026-09-26). The
# endpoint counts a reasoning model's thought against `max_tokens`; at a bare budget
# (as small as ~93 tokens for a short span) the thought alone hit the bound, every
# small summary was cut and refused, and the last-resort cut dropped the fold that held
# the run's decision (measured, E81). The SUMMARY TEXT — thought stripped — must
# still fit the budget; the allowance is for the thinking only.
THINK_ALLOWANCE = 1024
INDEX_USER_CHARS = 1200  # the user's own words, verbatim, at most this much per fold
KEEP_TAIL = 6  # rows at the end of the view that never fold — the live tail
# Rows per fold at the watermark (owner, 2026-10-11: 1024 "for now"). At 8 a chat past
# folded 8 rows at a time into fold rows nearly as big as the rows (summary + index):
# 47 folds, 4 levels, 391 s before the first answer (E39). At 1024 one overflow folds
# the whole foldable stretch into ONE fold.
SPAN = 1024
#: Adjacent folds at the head of the view before they fold as a group.
#:
#: THREE, not four. A real run folds at most once per turn — each projection
#: drops the view under the watermark and stops — so the head accumulates folds
#: one at a time and this threshold is the whole story. At four it never fired:
#: measured 2026-09-11, `nested_folds` and `flat_roots` both ended `levels [1]`,
#: a head that grows one fold per span forever, which is the growth this
#: constant exists to stop.
#:
#: Three still skips-then-batches, so depth grows as log₃ of the history rather
#: than one level per cut (the 2026-09-06 measurement: twelve levels in 26 cuts,
#: when every span began on the previous fold).
FOLD_RUN = 3

# ─── the no-call reminder (core.py) ─────────────────────────────────────

# A chain turn that dispatches no call is told to TRY AGAIN (emit a call, or end
# the chain with `done`/`await`). MAX_SILENT_TURNS in a row — prose-only turns and
# dead turns (only thinking) alike, reset by any call — and the chain GIVES UP:
# no more reminders, the caller is told the model could not answer (owner,
# 2026-09-30, after a single dead turn ended a 30-pass chain and an unbounded
# reminder loop was the only alternative).
MAX_SILENT_TURNS = 10
