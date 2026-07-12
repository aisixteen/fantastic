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

/// The SHORT orientation readme auto-seeded into `.fantastic/readme.md` on boot.
/// The full manual is the keyed [`DOCS`] tree — `reflect readme=index` / `=<key>`.
pub const README: &str = include_str!("readme.md");

/// The manual, split into keyed topic docs (nested dirs = tiers via a `/` in the
/// key). Seeded into `.fantastic/docs/<key>.md`; `reflect readme=index` lists them
/// token-annotated and `reflect readme=<key>` drills one. Trimmed + reordered so a
/// self-authoring model reaches `self-extension` first, not a system-manual dump.
pub const DOCS: &[(&str, &str)] = &[
    ("self-extension", include_str!("docs/self-extension.md")),
    ("reflect", include_str!("docs/reflect.md")),
    ("agents", include_str!("docs/agents.md")),
    ("memory", include_str!("docs/memory.md")),
    ("persistence", include_str!("docs/persistence.md")),
    ("transports", include_str!("docs/transports.md")),
    ("streams", include_str!("docs/streams.md")),
    ("daemon", include_str!("docs/daemon.md")),
];

/// Seed `<workdir>/.fantastic/readme.md` from [`README`] AND the [`DOCS`] tree
/// into `<workdir>/.fantastic/docs/` if missing. Idempotent per file: preserves
/// any user-edited content on subsequent calls.
///
/// Substrate's `seed_readme` only fires for agents with a registered bundle; the
/// root has no handler_module, so the CLI binary calls this directly after
/// `bootstrap()`.
pub fn seed_root_readme(workdir: &Path) -> io::Result<()> {
    let fdir = workdir.join(".fantastic");
    let dest = fdir.join("readme.md");
    if !dest.exists() {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, README)?;
    }
    let docs_dir = fdir.join("docs");
    for (key, body) in DOCS {
        let path = docs_dir.join(format!("{key}.md"));
        if path.exists() {
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, body)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
