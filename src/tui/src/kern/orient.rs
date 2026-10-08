//! THE ORIENTATION for a coding CLI opened in the project (owner 2026-10-08):
//! `.fantastic/AGENTS.md` (the cross-tool name) and `.fantastic/CLAUDE.md`
//! (Claude Code's, importing it) — is a kernel running here, how to check it,
//! how to connect (`.mcp.json`, `/mcp`), how to call it from a shell, what is
//! safe to edit. SEEDED at boot when missing, never overwritten (the human's to
//! edit); static text only — what changes (ids, the page guid) is `reflect`'s.

use std::path::Path;

const FILES: &[(&str, &str)] = &[
    ("AGENTS.md", include_str!("../../starters/orientation/AGENTS.md")),
    ("CLAUDE.md", include_str!("../../starters/orientation/CLAUDE.md")),
];

/// Seed each file under `.fantastic/` that is missing.
pub fn seed(cwd: &Path) -> Result<(), String> {
    let dir = cwd.join(".fantastic");
    for (name, body) in FILES {
        let f = dir.join(name);
        if !f.exists() {
            std::fs::write(&f, body).map_err(|e| format!("{}: {e}", f.display()))?;
        }
    }
    Ok(())
}
