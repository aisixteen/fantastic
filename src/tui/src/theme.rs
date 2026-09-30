//! EVERY colour and EVERY glyph, from ONE place — and that place is
//! `design/spec.json`, not this file. A re-skin runs `sh design/serve.sh`, edits
//! in the browser, saves, and runs `sh design/sync.sh`; the values below are
//! re-exported from the generated `theme_gen`.
//!
//! What stays HAND-WRITTEN here is everything that is a judgement rather than a
//! value: which glyph a `Mark` takes, which hue a `Closure` takes, what a style
//! composes to. Those are decisions, and a design tool should not be able to
//! change them by accident.
//!
//! Dark, always. The canvas is opaque black (`chrome::fill_black` runs first on
//! every frame) so the shell looks identical on a light or dark terminal.

// The palette is declared WHOLE, not grown one constant per step: a re-skin
// must have one file to edit, and a glyph that lives here cannot be typed as a
// literal somewhere else by accident. Steps 7-10 (tree, outline, gutter, cards)
// consume what step 1 does not.
#![allow(dead_code)]

use ratatui::style::{Color, Modifier, Style};

pub use crate::theme_gen::*;

// ── roles ────────────────────────────────────────────────────────────────────
// A ROLE is a judgement about which palette entry a thing takes, so it lives here
// and not in the spec: the spec says what magenta IS, this says that a section
// heading is the one that wears it. One value, two names, and the name that can
// change is this one.

/// A section heading inside a panel.
pub const LABEL: Color = ACCENT;
/// The line the cursor is on.
pub const SELECT: Color = ACCENT_HI;
/// Nothing in flight. Deliberately the same grey as ordinary secondary text:
/// idle is the absence of news, not news.
pub const IDLE: Color = DIM;

// ── styles ───────────────────────────────────────────────────────────────────
pub fn base() -> Style {
    Style::default().bg(BG).fg(FG)
}

pub fn dim() -> Style {
    Style::default().fg(DIM)
}

pub fn prompt() -> Style {
    Style::default().fg(PROMPT).add_modifier(Modifier::BOLD)
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT_HI).add_modifier(Modifier::BOLD)
}

/// The gutter glyph and its colour for one outline line. THE GUTTER IS NOT
/// DERIVED: it comes from the agent's own `view` row, so this maps a recorded
/// resolution and nothing more.
pub fn mark(m: crate::thread::Mark) -> (char, Color) {
    use crate::thread::Mark;
    match m {
        // Rides in the prompt whole.
        Mark::Full => (MARK_FULL, FG),
        // In the prompt, abbreviated by the kernel.
        Mark::Stub => (MARK_STUB, Color::Gray),
        // In the prompt only as a fold's summary line.
        Mark::Fold => (MARK_FOLD, DIM),
        // The model cannot see this row at all.
        Mark::NotInView => (MARK_OUT, DIM),
        // A resolution this shell does not know. SHOWN, never guessed: a new
        // vocabulary word must be visible, not silently rendered as one of ours.
        Mark::Unknown => ('?', BLOCKED),
    }
}

/// The colour for a line's closure.
// ── THE TRANSCRIPT'S ROLES — which hue says WHO or WHAT a line is ─────────
/// The human's own name label.
pub const SAY_HUMAN: Color = PROMPT;
/// The agent the pane belongs to: its label.
pub const SAY_SELF: Color = ACCENT_HI;
/// Any other agent's label.
pub const SAY_OTHER: Color = DONE;
/// A tool call (`▸ send`) — work the model put in motion.
pub const SAY_CALL: Color = BLOCKED;
/// A tool reply — data that came back, not conversation.
pub const SAY_REPLY: Color = MUTED;
/// Plumbing: folds, recalls, other kinds.
pub const SAY_PLUMBING: Color = DIM;

pub fn closure(c: crate::thread::Closure) -> Color {
    use crate::thread::Closure;
    match c {
        Closure::Running => WORKING,
        Closure::Failed => FAILED,
        Closure::Closed => DONE,
        Closure::Plain => FG,
    }
}
