//! Call security — the grant model (v9, #754).
//!
//! - **One field.** An agent's grants are one record field, `can`: lines, each a
//!   target (a literal local agent id, or `$parent`) and the one level held there
//!   — `call: [verbs]`, `update: [fields]` (which includes calling every verb) or
//!   `create: {who, what}` (creating agents of `what`'s bundles with ids matching
//!   `who` under the target, and updating, calling and deleting the agents
//!   strictly under it that it selects). `*` means everything in any slot; a
//!   target is never a wildcard. Nothing is inherited and nothing is stamped.
//! - **Position.** An agent calls its own tree, itself included, with no line.
//!   Anything outside it needs a line, the parent included. Nothing changes or
//!   deletes its own record or an ancestor's; `id`, `parent` and
//!   `handler_module` are not fields.
//! - **The caller's lines and the cap.** The kernel checks every call against
//!   the CALLER's lines alone; the target keeps no list of callers. A call passes
//!   only when the caller's lines allow it and every ancestor whose tree does not
//!   hold the target allows it too (the root holds every agent, so it never
//!   caps).
//! - **The writer holds what it hands out.** An agent writes a `can` line only
//!   when its own written lines (never its position) hold that line, cap
//!   included ([`glue`]); narrowing is always allowed.
//! - **Cedar.** Each agent is a Cedar entity and each principal's lines its own
//!   policy set ([`solver`]); the gate (`send::gate`) asks Cedar per agent in the
//!   chain. The operator (origin `None`) is never checked.
//!
//! Peer targets (`<guid>|…`) are parked: no line names one, and a remote target
//! answers as an unknown agent to every agent caller.

mod line;
pub use line::{parse_can, Level, Line, Target, What};
pub(crate) mod solver;
pub(crate) mod glue;

#[cfg(test)]
mod tests;
