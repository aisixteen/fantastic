//! WHERE THE KERNEL IS, and who we are when we talk to it.
//!
//! Nothing here guesses. A port, a leg id or a root id that is not on disk and
//! not in the environment is an ERROR that names the variable — the motto is
//! "by default, no defaults", and a default that reaches the network is the
//! exact shape this project refuses.
//!
//! The ONE thing minted rather than read is `client_id`, because it identifies
//! THIS terminal and no file can know it.

use std::path::{Path, PathBuf};

use crate::kern::model::AgentId;

/// WHO THIS TERMINAL IS when it acts in the kernel.
///
/// No host, no port and no leg id: this shell does not dial anything, and the
/// wire between it and the kernel is not its decision to make.
/// The human's own agent id. ONE copy: `ClientCfg::user` is set from it, and a
/// pane that has no `ClientCfg` names the human through it.
pub const HUMAN: &str = "user";

#[derive(Debug, Clone)]
pub struct ClientCfg {
    /// The human's own agent — the terminal's face in the kernel. Reads and acts
    /// both go out as this, because the terminal IS an agent on the spine, not a
    /// client of a server.
    pub user: AgentId,
    /// `tui-<hex6>`. NEVER `cli`: `ai_core` treats that id as its own internal
    /// caller and would route this terminal's tokens nowhere.
    pub client_id: String,
}

impl ClientCfg {
    pub fn here() -> Self {
        Self {
            user: HUMAN.to_string(),
            client_id: mint_client_id(),
        }
    }
}

/// Everything the shell knows before it has spoken to a kernel.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The directory the user ran us in. The kernel's own file clamp is rooted
    /// here, so this is the boundary of the session, not a preference.
    pub cwd: PathBuf,
    /// `<cwd>/.fantastic` — the store. Its ABSENCE is what makes a run seed it.
    pub store: PathBuf,
    /// `<cwd>/.fantastic/cli` — this client's own corner: the port file, the
    /// seen set, the log.
    pub cli: PathBuf,
}

impl Paths {
    pub fn here() -> std::io::Result<Self> {
        Ok(Self::at(&std::env::current_dir()?))
    }
    pub fn at(cwd: &Path) -> Self {
        let store = cwd.join(".fantastic");
        Self {
            cli: store.join("cli"),
            store,
            cwd: cwd.to_path_buf(),
        }
    }
    /// TRUE when this directory has never been used: a run here seeds it.
    pub fn is_fresh(&self) -> bool {
        !self.store.exists()
    }
}

/// `tui-<hex6>`, distinct per process. Derived from the pid and the clock, which
/// is enough: it separates two terminals in one directory, and that is all a
/// `client_id` is for.
pub fn mint_client_id() -> String {
    let pid = std::process::id() as u64;
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 + d.as_secs())
        .unwrap_or(0);
    // A cheap mix — this is an identifier, not a secret.
    let mut h = pid.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ ns.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 31;
    format!("tui-{:06x}", h & 0xFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_without_a_store_is_fresh() {
        let d = std::env::temp_dir().join(format!("fc-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = Paths::at(&d);
        assert!(
            p.is_fresh(),
            "no .fantastic yet, so a run seeds it"
        );
        std::fs::create_dir_all(&p.cli).unwrap();
        assert!(
            !Paths::at(&d).is_fresh(),
            "a seeded directory goes straight in"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_client_id_is_never_the_reserved_one() {
        let id = mint_client_id();
        assert!(id.starts_with("tui-"));
        assert_ne!(id, "cli", "`cli` is ai_core's own internal caller id");
        assert_eq!(id.len(), 10);
    }
}
