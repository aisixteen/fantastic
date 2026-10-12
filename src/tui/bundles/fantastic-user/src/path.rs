//! `user.path` — the human's programs: every executable on PATH.
//!
//! PATH is this process's MERGED with the login shell's (`$SHELL -l -c`): a CLI
//! started from a GUI or a service has a narrower PATH than the human's
//! terminal, which is where their tools live (the way agent managers resolve
//! a coding agent — Vibe Kanban `utils/shell.rs`, Nimbalyst `shellPath.ts`).
//! No `$SHELL`, or a shell that does not answer in time: this process's PATH
//! alone. Nothing is read but directory listings and file modes.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use serde_json::{json, Value};

/// How long the login shell may take to print its PATH.
const SHELL_WAIT: Duration = Duration::from_secs(5);

/// The login shell's PATH, if it answers in time.
async fn login_path() -> Option<String> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
    let child = tokio::process::Command::new(shell)
        .args(["-l", "-c", "printf %s \"$PATH\""])
        .env("TERM", "dumb")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(SHELL_WAIT, child).await.ok()?.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `a` then the entries of `b` not in it, empties dropped.
pub fn merge(a: &str, b: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for d in a.split(':').chain(b.split(':')) {
        if !d.is_empty() && !out.iter().any(|x| x == d) {
            out.push(d.to_string());
        }
    }
    out
}

/// Every executable file in `dirs`, the first directory holding a name winning.
pub fn executables(dirs: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
            if out.contains_key(&name) {
                continue;
            }
            // follows symlinks: a linked program is its target
            let Ok(m) = std::fs::metadata(e.path()) else { continue };
            if m.is_file() && m.permissions().mode() & 0o111 != 0 {
                out.insert(name, e.path().to_string_lossy().into_owned());
            }
        }
    }
    out
}

/// `{dirs, executables}`.
pub async fn scan() -> Value {
    let own = std::env::var("PATH").unwrap_or_default();
    let login = login_path().await.unwrap_or_default();
    let dirs = merge(&own, &login);
    let exes = tokio::task::spawn_blocking({
        let dirs = dirs.clone();
        move || executables(&dirs)
    })
    .await
    .unwrap_or_default();
    json!({"dirs": dirs, "executables": exes})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_order_and_drops_repeats() {
        assert_eq!(merge("/a:/b::/a", "/c:/b"), vec!["/a", "/b", "/c"]);
    }

    #[test]
    fn executables_first_dir_wins_and_skips_non_exec() {
        let t = std::env::temp_dir().join(format!("fpath_{}", std::process::id()));
        let (a, b) = (t.join("a"), t.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        for (dir, name, mode) in [(&a, "tool", 0o755), (&b, "tool", 0o755), (&b, "other", 0o755), (&b, "data", 0o644)] {
            let f = dir.join(name);
            std::fs::write(&f, "x").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let got = executables(&[a.to_string_lossy().into(), b.to_string_lossy().into()]);
        assert_eq!(got.get("tool").map(String::as_str), Some(a.join("tool").to_str().unwrap()));
        assert!(got.contains_key("other"));
        assert!(!got.contains_key("data"));
        std::fs::remove_dir_all(&t).unwrap();
    }
}
