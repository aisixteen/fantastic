//! `fantastic migrate-home [--dry-run]` — move the files an older layout kept
//! into the home, `~/.fantastic` (`FANTASTIC_HOME`, else `~/.fantastic`). Run by
//! hand: nothing migrates by itself.
//!
//! ```text
//!   ~/.config/fantastic/fantastics/                 → <home>/fantastics/
//!   ~/.config/fantastic/agents/ai_agent/saved/      → <home>/user/setups/ai_agent/
//!   ~/.config/fantastic/agents/ai_agent/keys/       → <home>/user/keys/
//!   ~/.fantastic/credentials/*.yaml                 → <home>/user/credentials/
//! ```
//!
//! Each move is a rename, so a file keeps its mode (0600) or its DACL; every dir
//! it creates is made private as the home store makes its own. It REFUSES, and
//! moves nothing, when a target exists, when a registered fantastic runs (its
//! folder's lock is held) or when the app holds `<home>/app`. It prints every
//! move; `--dry-run` prints them and moves nothing. Dirs the moves left empty
//! are removed, and said.

use std::path::{Path, PathBuf};

/// The old and the new places: the moves' inputs.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The old home: `~/.config/fantastic`.
    pub old_home: PathBuf,
    /// The old credential_store default dir: `~/.fantastic/credentials`.
    pub old_credentials: PathBuf,
    /// The home: `FANTASTIC_HOME`, else `~/.fantastic`.
    pub home: PathBuf,
}

impl Layout {
    /// The layout this process's environment names.
    pub fn from_env() -> Result<Self, String> {
        let user = fantastic_user_home_store::user_home_from_env()?;
        let home = std::path::absolute(fantastic_user_home_store::home_dir_from_env()?)
            .map_err(|e| format!("the home: {e}"))?;
        Ok(Self {
            old_home: user.join(".config").join("fantastic"),
            old_credentials: user.join(".fantastic").join("credentials"),
            home,
        })
    }
}

/// One rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    /// From.
    pub from: PathBuf,
    /// To.
    pub to: PathBuf,
}

/// The moves this layout calls for: only those whose source exists.
pub fn plan(l: &Layout) -> Result<Vec<Move>, String> {
    let mut out = Vec::new();
    let user = l.home.join("user");
    for (from, to) in [
        (l.old_home.join("fantastics"), l.home.join("fantastics")),
        (
            l.old_home.join("agents").join("ai_agent").join("saved"),
            user.join("setups").join("ai_agent"),
        ),
        (
            l.old_home.join("agents").join("ai_agent").join("keys"),
            user.join("keys"),
        ),
    ] {
        if from.exists() {
            out.push(Move { from, to });
        }
    }
    if l.old_credentials.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&l.old_credentials)
            .map_err(|e| format!("{}: {e}", l.old_credentials.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "yaml"))
            .collect();
        files.sort();
        for from in files {
            let to = user
                .join("credentials")
                .join(from.file_name().expect("a file has a name"));
            out.push(Move { from, to });
        }
    }
    Ok(out)
}

/// Why the moves may not run now, or `Ok`.
pub fn refusal(l: &Layout, moves: &[Move]) -> Result<(), String> {
    for m in moves {
        if m.to.exists() {
            return Err(format!(
                "{} already exists — move or remove it first; nothing was moved",
                m.to.display()
            ));
        }
    }
    if let Some(pid) = held(fantastic_kernel::lock::held_at(&l.home.join("app")))? {
        return Err(format!(
            "the app holds {} (pid {pid}) — quit it first; nothing was moved",
            l.home.join("app").display()
        ));
    }
    for project in registered(&l.old_home.join("fantastics"))? {
        if let Some(pid) = held(fantastic_kernel::lock::held(&project))? {
            return Err(format!(
                "a fantastic runs in {} (pid {pid}) — stop it first; nothing was moved",
                project.display()
            ));
        }
    }
    Ok(())
}

fn held(r: fantastic_kernel::KernelResult<Option<u32>>) -> Result<Option<u32>, String> {
    r.map_err(|e| e.to_string())
}

/// The project dir each registry folder names (its `fantastic.yaml`'s `path`).
fn registered(fantastics: &Path) -> Result<Vec<PathBuf>, String> {
    let Ok(rd) = std::fs::read_dir(fantastics) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let info = e.path().join(fantastic_user_home_store::user_store::INFO);
        let Ok(raw) = std::fs::read_to_string(&info) else {
            continue;
        };
        let v = fantastic_kernel::yaml_io::load(&raw)
            .map_err(|e| format!("{}: {e}", info.display()))?;
        if let Some(p) = v.get("path").and_then(serde_json::Value::as_str) {
            out.push(PathBuf::from(p));
        }
    }
    out.sort();
    Ok(out)
}

/// Run the moves (`dry` prints them only). Each printed line is one move or one
/// empty dir removed.
pub fn run(l: &Layout, dry: bool, say: &mut dyn FnMut(String)) -> Result<usize, String> {
    let moves = plan(l)?;
    refusal(l, &moves)?;
    if !dry && !moves.is_empty() {
        // the home itself private (or made so) before the first move
        fantastic_user_home_store::ensure_private_dirs(&l.home, &l.home)?;
    }
    for m in &moves {
        say(format!(
            "{}move {} → {}",
            if dry { "would " } else { "" },
            m.from.display(),
            m.to.display()
        ));
        if dry {
            continue;
        }
        let parent = m.to.parent().expect("a target has a parent");
        fantastic_user_home_store::ensure_private_dirs(&l.home, parent)?;
        std::fs::rename(&m.from, &m.to)
            .map_err(|e| format!("move {} → {}: {e}", m.from.display(), m.to.display()))?;
    }
    if !dry {
        for d in [
            l.old_home.join("agents").join("ai_agent"),
            l.old_home.join("agents"),
            l.old_home.clone(),
            l.old_credentials.clone(),
        ] {
            if d.is_dir() && std::fs::read_dir(&d).is_ok_and(|mut r| r.next().is_none()) {
                std::fs::remove_dir(&d).map_err(|e| format!("remove {}: {e}", d.display()))?;
                say(format!("remove empty {}", d.display()));
            }
        }
    }
    Ok(moves.len())
}

/// Entry point, called from `main` before any runtime exists.
pub fn main(argv: &[String]) -> ! {
    let dry = match argv {
        [] => false,
        [f] if f == "--dry-run" => true,
        _ => {
            eprintln!("fantastic migrate-home: takes only --dry-run (got {argv:?})");
            std::process::exit(2);
        }
    };
    let l = match Layout::from_env() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fantastic migrate-home: {e}");
            std::process::exit(2);
        }
    };
    match run(&l, dry, &mut |line| println!("{line}")) {
        Ok(0) => {
            println!(
                "nothing to move: no older layout under {} or {}",
                l.old_home.display(),
                l.old_credentials.display()
            );
            std::process::exit(0);
        }
        Ok(n) => {
            println!(
                "{n} move(s){}",
                if dry {
                    " — a dry run, nothing was moved"
                } else {
                    ""
                }
            );
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("fantastic migrate-home: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh dir per test, removed when dropped.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("fc-migrate-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn layout(t: &Path) -> Layout {
        Layout {
            old_home: t.join(".config").join("fantastic"),
            old_credentials: t.join(".fantastic").join("credentials"),
            home: t.join(".fantastic"),
        }
    }

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    /// An old tree: one registered fantastic, a saved set-up, the keys, a credential file.
    fn old_tree(t: &Path, project: &Path) -> Layout {
        let l = layout(t);
        write(
            &l.old_home.join("fantastics/g1/fantastic.yaml"),
            &format!("guid: g1\npath: {}\n", project.display()),
        );
        write(
            &l.old_home.join("agents/ai_agent/saved/opus.yaml"),
            "handler_module: x.tools\n",
        );
        write(&l.old_home.join("agents/ai_agent/keys/mode"), "plain");
        write(&l.old_credentials.join("credentials.yaml"), "refs: {}\n");
        private(&l.home);
        l
    }

    /// `0700`, as the credential store and the home store make their dirs.
    fn private(_d: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(_d, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn an_old_tree_moves_to_the_new_one() {
        let t = Tmp::new("moves");
        let project = t.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let l = old_tree(t.path(), &project);
        let mut lines = Vec::new();
        assert_eq!(run(&l, false, &mut |s| lines.push(s)).unwrap(), 4);
        let h = &l.home;
        assert!(h.join("fantastics/g1/fantastic.yaml").is_file());
        assert!(h.join("user/setups/ai_agent/opus.yaml").is_file());
        assert!(h.join("user/keys/mode").is_file());
        assert!(h.join("user/credentials/credentials.yaml").is_file());
        assert!(!l.old_home.exists(), "the emptied old home is removed");
        assert!(
            !l.old_credentials.exists(),
            "the emptied credentials dir is removed"
        );
        assert_eq!(
            lines.iter().filter(|s| s.starts_with("move ")).count(),
            4,
            "{lines:?}"
        );
        #[cfg(unix)]
        for d in ["", "user", "user/setups", "user/credentials"] {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(h.join(d)).unwrap().permissions().mode() & 0o777;
            assert_eq!(m, 0o700, "{d}");
        }
        // a second run finds nothing to move
        assert_eq!(run(&l, false, &mut |_| {}).unwrap(), 0);
    }

    #[test]
    fn a_dry_run_prints_every_move_and_moves_nothing() {
        let t = Tmp::new("dry");
        let l = old_tree(t.path(), &t.path().join("proj"));
        let mut lines = Vec::new();
        assert_eq!(run(&l, true, &mut |s| lines.push(s)).unwrap(), 4);
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(
            lines.iter().all(|s| s.starts_with("would move ")),
            "{lines:?}"
        );
        assert!(lines[0].ends_with(&format!("→ {}", l.home.join("fantastics").display())));
        assert!(l.old_home.join("agents/ai_agent/saved/opus.yaml").is_file());
        assert!(!l.home.join("user").exists());
    }

    #[test]
    fn an_existing_target_refuses_and_nothing_moves() {
        let t = Tmp::new("exists");
        let l = old_tree(t.path(), &t.path().join("proj"));
        std::fs::create_dir_all(l.home.join("user/keys")).unwrap();
        let e = run(&l, false, &mut |_| {}).unwrap_err();
        assert!(e.contains("user") && e.contains("already exists"), "{e}");
        assert!(l.old_home.join("fantastics/g1").is_dir(), "nothing moved");
        assert!(!l.home.join("fantastics").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_home_others_can_read_refuses_before_any_move() {
        use std::os::unix::fs::PermissionsExt;
        let t = Tmp::new("open");
        let l = old_tree(t.path(), &t.path().join("proj"));
        std::fs::set_permissions(&l.home, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = run(&l, false, &mut |_| {}).unwrap_err();
        assert!(
            e.contains("readable by others") && e.contains("chmod 700"),
            "{e}"
        );
        assert!(l.old_home.join("fantastics/g1").is_dir(), "nothing moved");
    }

    #[test]
    fn a_running_fantastic_or_the_app_refuses() {
        let t = Tmp::new("running");
        let project = t.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let l = old_tree(t.path(), &project);
        fantastic_kernel::lock::acquire(&project).unwrap();
        let e = run(&l, false, &mut |_| {}).unwrap_err();
        assert!(
            e.contains("a fantastic runs in") && e.contains("proj"),
            "{e}"
        );
        fantastic_kernel::lock::release(&project).unwrap();

        let app = l.home.join("app");
        fantastic_kernel::lock::acquire_at(&app).unwrap();
        let e = run(&l, false, &mut |_| {}).unwrap_err();
        assert!(e.contains("the app holds"), "{e}");
        fantastic_kernel::lock::release_at(&app).unwrap();
        assert!(
            l.old_home.join("agents/ai_agent/keys/mode").is_file(),
            "nothing moved"
        );
    }
}
