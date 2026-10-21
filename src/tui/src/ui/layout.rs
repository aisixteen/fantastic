// GENERATED FROM design/spec.json BY design/sync.py — DO NOT EDIT BY HAND.
//
// Change the look in the design surface (`sh design/serve.sh`), save, then run
// `sh design/sync.sh`. Editing this file directly means the next sync silently
// reverts you.

//! Every dimension the frame is laid out from.

/// rows reserved at the top for a docked title. Zero: the title belongs to the start screen only, and every pane starts at row 0.
pub const TITLE_H: u16 = 0;

/// width of the agent tree pane, in cells
pub const TREE_W: u16 = 28;

/// the input line plus its hint
pub const COMPOSER_H: u16 = 2;

/// the status bar
pub const STATUS_H: u16 = 1;

/// cells of indent per tree depth
pub const INDENT: u16 = 2;

/// the outline's context gutter, ' X '
pub const GUTTER_W: u16 = 3;
