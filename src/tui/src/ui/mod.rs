//! Rendering. Every pane is a pure function of state — no pane owns a channel,
//! spawns a task, or talks to the wire.

pub mod chrome;
pub mod composer;
pub mod frame;
pub mod layout;
pub mod side;
pub mod tree;

pub use frame::{draw, layout};
