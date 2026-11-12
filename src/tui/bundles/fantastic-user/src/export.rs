//! EXPORT A SUBTREE AS A BLUEPRINT (#268). The engine lives in
//! `fantastic-core` (`fantastic_core::export`), because the kernel's ROOT
//! answers `export_subtree` too; this is the same engine under `user`.

pub use fantastic_core::export::*;

#[cfg(test)]
use fantastic_kernel::AgentRecord;
#[cfg(test)]
use serde_json::{Map, Value};

#[cfg(test)]
mod tests;
