// GENERATED FROM design/spec.json BY design/sync.py — DO NOT EDIT BY HAND.
//
// Change the look in the design surface (`sh design/serve.sh`), save, then run
// `sh design/sync.sh`. Editing this file directly means the next sync silently
// reverts you.


use ratatui::style::Color;

// ── palette ──────────────────────────────────────────────────────────────────
/// the canvas — Slack/Primer charcoal, biased toward the magenta
pub const BG: Color = Color::Rgb(0x0f, 0x0e, 0x14);

/// the sidebar, the thread panel header, the status bar
pub const PANEL: Color = Color::Rgb(0x16, 0x14, 0x1d);

/// a selected row, a hovered row
pub const BAND: Color = Color::Rgb(0x22, 0x1f, 0x2c);

/// dividers and at-rest frames
pub const RULE: Color = Color::Rgb(0x2d, 0x2a, 0x38);

/// ordinary text
pub const FG: Color = Color::Rgb(0xe7, 0xe4, 0xee);

/// secondary text: times, tool calls, summaries
pub const MUTED: Color = Color::Rgb(0xa0, 0x9b, 0xab);

/// tertiary text: hints, the gutter's quiet marks
pub const DIM: Color = Color::Rgb(0x67, 0x62, 0x7a);

/// a question whose thread finished without it
pub const STALE: Color = Color::Rgb(0x4c, 0x48, 0x58);

/// FANTASTIC magenta, deep — the ai's name, reply counts
pub const ACCENT: Color = Color::Rgb(0xc4, 0x3f, 0xb0);

/// FANTASTIC magenta, bright — the one brand highlight: selection edge, focus
pub const ACCENT_HI: Color = Color::Rgb(0xff, 0x37, 0xd7);

/// you — the human's name, the composer sigil
pub const PROMPT: Color = Color::Rgb(0x39, 0xc5, 0xcf);

/// running — Primer success
pub const WORKING: Color = Color::Rgb(0x3f, 0xb9, 0x50);

/// an open question: its badge and bar. A marker, never a thread state — Primer attention
pub const BLOCKED: Color = Color::Rgb(0xd2, 0x99, 0x22);

/// settled — Primer accent
pub const DONE: Color = Color::Rgb(0x58, 0xa6, 0xff);

/// a settled call whose status is not completed — Primer danger
pub const FAILED: Color = Color::Rgb(0xf8, 0x51, 0x49);

/// a chosen quick pick
pub const PICK: Color = Color::Rgb(0x4a, 0x1f, 0x44);

/// the question you came for, lit in its thread
pub const LIT: Color = Color::Rgb(0x24, 0x1d, 0x10);

/// text on a bright badge
pub const INK: Color = Color::Rgb(0x0f, 0x0e, 0x14);

/// a rail tile — muted so the status marks on it stay the loudest thing
pub const TILE_VIOLET: Color = Color::Rgb(0x6f, 0x55, 0xb0);

/// a rail tile — muted so the status marks on it stay the loudest thing
pub const TILE_ROSE: Color = Color::Rgb(0xa9, 0x46, 0x6b);

/// a rail tile — muted so the status marks on it stay the loudest thing
pub const TILE_GREEN: Color = Color::Rgb(0x3a, 0x7d, 0x52);

/// a rail tile — muted so the status marks on it stay the loudest thing
pub const TILE_AMBER: Color = Color::Rgb(0x9a, 0x6c, 0x2c);

/// a rail tile — muted so the status marks on it stay the loudest thing
pub const TILE_TEAL: Color = Color::Rgb(0x2d, 0x7f, 0x86);

/// a rail tile — muted so the status marks on it stay the loudest thing
pub const TILE_BLUE: Color = Color::Rgb(0x3a, 0x64, 0xa8);

// ── glyphs ───────────────────────────────────────────────────────────────────
pub const NODE_LIVE: char = '●';
pub const NODE_BARE: char = '○';
pub const RUNNING: char = '*';
pub const INQUIRY: char = '✉';
pub const FOLDED: char = '▸';
pub const OPENED: char = '▾';
pub const MARK_FULL: char = '█';
pub const MARK_STUB: char = '▒';
pub const MARK_FOLD: char = '░';
pub const MARK_OUT: char = '·';
pub const SIGIL_MESSAGE: char = '›';
pub const SIGIL_VERB: char = '/';
pub const SIGIL_LOCKED: char = '×';

// ── the title gradient, sampled per row ──────────────────────────────────────
pub const TITLE_STOPS: [(f32, (u8, u8, u8)); 3] = [
    (0.0, (0xff, 0x78, 0xe1)),
    (0.5, (0xff, 0x37, 0xd7)),
    (1.0, (0xcd, 0x64, 0xff)),
];

// ── timing ───────────────────────────────────────────────────────────────────
/// the draw clock. 16 = 60fps.
pub const FRAME_MS: u64 = 16;

/// presence, blink, poll cadence
pub const TICK_MS: u64 = 250;

/// the title builds top-down over this
pub const REVEAL_SECS: f32 = 1.5;

/// the ease-out that docks the title
pub const MINIMIZE_SECS: f32 = 1.0;

/// the attract prompt's square wave, toggles per 1/hz seconds
pub const BLINK_HZ: f32 = 1.2;

/// idle this long on attract and the intro movie plays
pub const IDLE_INTRO_SECS: f32 = 10.0;
