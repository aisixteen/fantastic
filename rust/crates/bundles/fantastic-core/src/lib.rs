//! Root orchestrator bundle — the userland id="core" agent at the
//! top of the tree.
//!
//! `core` has no `handler_module` — system verbs (create_agent,
//! delete_agent, update_agent, list_agents, reflect) are baked into
//! the substrate's `Agent` and answered natively. This crate is
//! intentionally small: it provides the readme seeded into
//! `.fantastic/readme.md` on boot and a helper the CLI binary calls
//! to drop it in place.

#![deny(missing_docs)]

use std::fs;
use std::io;
use std::path::Path;

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Seed `<workdir>/.fantastic/readme.md` from [`README`] if missing.
/// Idempotent: preserves any user-edited content on subsequent calls.
///
/// Substrate's `seed_readme` only fires for agents with a registered
/// bundle; the root has no handler_module, so the CLI binary calls
/// this directly after `bootstrap()`.
pub fn seed_root_readme(workdir: &Path) -> io::Result<()> {
    let dest = workdir.join(".fantastic/readme.md");
    if dest.exists() {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&dest, README)
}

#[cfg(test)]
mod tests;
