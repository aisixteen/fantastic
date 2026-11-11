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
    /// The work folder: where the kernel's file clamp is rooted and its commands
    /// run. The boundary of the session, not a preference. Not the state folder.
    pub work: PathBuf,
    /// The state folder (default `<work>/.fantastic`). Its ABSENCE is what makes
    /// a run seed it.
    pub store: PathBuf,
    /// `<store>/cli` — this client's own corner: the endpoint files, the seen
    /// set, the log.
    pub cli: PathBuf,
}

impl Paths {
    /// The two folders of a run. The state folder is `fantastic_kernel`'s one
    /// derivation (`state_dir_for`). The work folder is `workdir` when given; else
    /// the `workdir` the root record in the state folder keeps (a relative value
    /// is joined to the current dir); else the current dir.
    pub fn resolve(statedir: Option<&Path>, workdir: Option<&Path>) -> Result<Self, String> {
        let store = fantastic_kernel::bootstrap::state_dir_for(statedir, workdir)
            .map_err(|e| format!("no working directory: {e}"))?;
        let cwd = || std::env::current_dir().map_err(|e| format!("no working directory: {e}"));
        let work = match workdir {
            Some(w) => std::path::absolute(w).map_err(|e| format!("no working directory: {e}"))?,
            None => match recorded_workdir(&store) {
                Some(w) if w.is_absolute() => w,
                Some(w) => cwd()?.join(w),
                None => cwd()?,
            },
        };
        Ok(Self::of(store, work))
    }

    /// Both folders as they are, for a caller that has them already.
    pub fn of(store: PathBuf, work: PathBuf) -> Self {
        Self {
            cli: store.join("cli"),
            store,
            work,
        }
    }

    /// The kernel's guard, asked BEFORE this shell writes anything into the
    /// store (the seed, its log): a `.fantastic` that IS the fantastic home is
    /// never a kernel's (`fantastic_kernel::bootstrap::refuse_home_as_state_dir`).
    pub fn refuse_the_home(&self) -> Result<(), String> {
        use fantastic_kernel::bootstrap::{fantastic_home, refuse_home_as_state_dir};
        refuse_home_as_state_dir(&self.store, fantastic_home().as_deref())
            .map_err(|e| e.to_string())
    }
}

/// The `workdir` the root record in `store` keeps, if there is one.
fn recorded_workdir(store: &Path) -> Option<PathBuf> {
    let raw =
        std::fs::read_to_string(store.join(fantastic_kernel::persistence::RECORD_FILE)).ok()?;
    let rec = fantastic_kernel::persistence::parse_record(&raw)?;
    rec.get("workdir")
        .and_then(|v| v.as_str())
        .filter(|w| !w.is_empty())
        .map(PathBuf::from)
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

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fc-env-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn resolve_defaults_to_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let p = Paths::resolve(None, None).unwrap();
        assert_eq!(p.work, cwd);
        assert_eq!(p.store, cwd.join(".fantastic"));
        assert_eq!(p.cli, cwd.join(".fantastic").join("cli"));
        let far = Paths::resolve(Some(Path::new("/s")), Some(Path::new("/w"))).unwrap();
        assert_eq!(
            (far.store, far.work),
            (PathBuf::from("/s"), PathBuf::from("/w"))
        );
    }

    #[test]
    fn resolve_reads_the_recorded_workdir_back() {
        let s = scratch("rec");
        std::fs::write(s.join("agent.yaml"), "id: root\nworkdir: /w\n").unwrap();
        let p = Paths::resolve(Some(&s), None).unwrap();
        assert_eq!(p.work, PathBuf::from("/w"));
        assert_eq!(p.store, s);
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn a_relative_recorded_workdir_resolves_against_the_launch_cwd() {
        let s = scratch("rel");
        std::fs::write(s.join("agent.yaml"), "id: root\nworkdir: rel/w\n").unwrap();
        let p = Paths::resolve(Some(&s), None).unwrap();
        assert_eq!(p.work, std::env::current_dir().unwrap().join("rel/w"));
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn a_given_workdir_wins_over_the_record() {
        let s = scratch("win");
        std::fs::write(s.join("agent.yaml"), "id: root\nworkdir: /recorded\n").unwrap();
        let p = Paths::resolve(Some(&s), Some(Path::new("/given"))).unwrap();
        assert_eq!(p.work, PathBuf::from("/given"));
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn the_client_id_is_never_the_reserved_one() {
        let id = mint_client_id();
        assert!(id.starts_with("tui-"));
        assert_ne!(id, "cli", "`cli` is ai_core's own internal caller id");
        assert_eq!(id.len(), 10);
    }

    #[test]
    fn the_homes_parent_is_refused_before_anything_is_written() {
        let p = Paths::resolve(None, Some(Path::new("/u"))).unwrap();
        let home = Path::new("/u/.fantastic");
        let e = fantastic_kernel::bootstrap::refuse_home_as_state_dir(&p.store, Some(home))
            .unwrap_err();
        assert!(
            e.to_string().contains("is never a kernel's state folder"),
            "{e}"
        );
        let q = Paths::resolve(None, Some(Path::new("/u/proj"))).unwrap();
        assert!(
            fantastic_kernel::bootstrap::refuse_home_as_state_dir(&q.store, Some(home)).is_ok()
        );
    }
}
