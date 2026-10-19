//! THE `home` AGENT: the Phase 1 ViewModel (`fantastic_home::Home`) bound to the
//! kernel protocol.
//!
//! ```text
//!   agent    HomeBundle (`home.tools`): the verbs → Events, validation, replies
//!   runner   owns the Home: Event → reduce → Effects → HomeOs → Events; the poll; the sinks
//!   wire     the state JSON (protocol "fantastic.home", version 1)
//!   os       trait HomeOs, and SysOs (delegates every call to sys)
//!   sys      every OS call of the crate
//! ```

pub mod agent;
pub mod os;
pub mod runner;
#[doc(hidden)]
pub mod sys;
pub mod wire;

#[cfg(test)]
pub(crate) mod fake;

pub use agent::HomeBundle;
pub use os::{HomeOs, SysOs};

/// The agent's id.
pub const ID: &str = "home";

/// Its `handler_module`.
pub const HANDLER_MODULE: &str = "home.tools";

/// Every verb of the agent except the lifecycle (`boot`, `shutdown`): what a
/// page reaches through `user`. Phase 6 adds `menu` here, and the grant follows.
pub const VERBS: &[&str] = &[
    "list",
    "open",
    "register",
    "start",
    "stop",
    "remove",
    "select",
    "select_page",
    "close_page",
    "dismiss",
    "log",
    "focus",
    "reflect",
];
