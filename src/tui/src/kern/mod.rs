//! THE KERNEL, HOSTED IN THIS PROCESS.
//!
//! There is no transport. This shell links the rust kernel crate, boots it over
//! the directory it was launched in, and dispatches by calling it. No port, no
//! pid, no socket, no request and no reply — the terminal is an agent on the same
//! spine as everything else, and `host` is how it reaches that spine.

pub mod backend;
pub mod bundle_watch;
pub mod host;
pub mod keys;
pub mod mcp_config;
pub mod model;
pub mod orient;
pub mod prompt;
pub mod reach;
pub mod root_ai;
pub mod runner;
pub mod saved;
pub mod setup;
pub mod setup_agent;
pub mod term_watch;
pub mod terminal;
pub mod user_store;
pub mod web;
pub mod wizard;
