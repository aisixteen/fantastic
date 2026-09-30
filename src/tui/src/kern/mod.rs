//! THE KERNEL, HOSTED IN THIS PROCESS.
//!
//! There is no transport. This shell links the rust kernel crate, boots it over
//! the directory it was launched in, and dispatches by calling it. No port, no
//! pid, no socket, no request and no reply — the terminal is an agent on the same
//! spine as everything else, and `host` is how it reaches that spine.

pub mod ai_cells;
pub mod host;
pub mod keys;
pub mod model;
pub mod runner;
pub mod setup;
pub mod setup_agent;
pub mod templates;
pub mod terminal;
pub mod ui_agent;
pub mod web;
// CRITICAL TODO(dev-llm): dev-only hardwired LLM endpoint.
#[cfg(feature = "dev-llm")]
pub mod dev_llm;
