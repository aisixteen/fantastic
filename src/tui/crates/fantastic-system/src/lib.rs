//! THE SYSTEM AGENTS' MODELS AND VIEWMODELS (Phase 10, #55). One module per
//! agent; each is `Vm::new(config)`, `reduce(event) -> Vec<Effect>` (the only
//! mutation) and `state()` (what the view draws). OS answers arrive on events
//! as plain data, time as `now_ms`; the effects are plain data the bundle's
//! driver runs through its port. Nothing here touches the OS.

pub mod audio;
pub mod network;
pub mod power;
