//! `user.path` — the human's programs: every executable on PATH.
//!
//! PATH is this process's MERGED with the login shell's (`$SHELL -l -c`): a CLI
//! started from a GUI or a service has a narrower PATH than the human's
//! terminal, which is where their tools live (the way agent managers resolve
//! a coding agent — Vibe Kanban `utils/shell.rs`, Nimbalyst `shellPath.ts`).
//! No `$SHELL`, or a shell that does not answer in time: this process's PATH
//! alone — as on Windows, which has no login shell (`sys::login_path`). Nothing
//! is read but directory listings and file modes.

use std::collections::BTreeMap;

use serde_json::{json, Value};

/// `a` then the entries of `b` not in it, empties dropped — each split as the OS
/// splits PATH (`:`; `;` on Windows).
pub fn merge(a: &str, b: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for d in std::env::split_paths(a).chain(std::env::split_paths(b)) {
        let d = d.to_string_lossy();
        if !d.is_empty() && !out.iter().any(|x| *x == d) {
            out.push(d.into_owned());
        }
    }
    out
}

/// Every executable file in `dirs`, by the name a shell runs it by, the first
/// directory holding a name winning (within one, the file the shell tries first).
pub fn executables(dirs: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d) else {
            continue;
        };
        let mut here: Vec<(usize, String, String)> = Vec::new();
        for e in rd.flatten() {
            let Some(file) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some((name, rank)) = crate::sys::program_name(&file) else {
                continue;
            };
            if out.contains_key(&name) {
                continue;
            }
            // follows symlinks: a linked program is its target
            if crate::sys::is_program(&e.path()) {
                here.push((rank, name, e.path().to_string_lossy().into_owned()));
            }
        }
        here.sort_by_key(|h| h.0);
        for (_, name, path) in here {
            out.entry(name).or_insert(path);
        }
    }
    out
}

/// The running fantastic is always a known program (#216): the menu's fantastic tile
/// must not depend on PATH. Its own executable wins over a `fantastic` found on PATH.
pub fn with_self(
    mut exes: BTreeMap<String, String>,
    own: Option<String>,
) -> BTreeMap<String, String> {
    if let Some(own) = own {
        exes.insert("fantastic".to_string(), own);
    }
    exes
}

/// `{dirs, executables, self}`.
pub async fn scan() -> Value {
    let own_path = std::env::var("PATH").unwrap_or_default();
    let login = crate::sys::login_path().await.unwrap_or_default();
    let dirs = merge(&own_path, &login);
    let exes = tokio::task::spawn_blocking({
        let dirs = dirs.clone();
        move || executables(&dirs)
    })
    .await
    .unwrap_or_default();
    let own = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let exes = with_self(exes, own.clone());
    json!({"dirs": dirs, "executables": exes, "self": own})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_order_and_drops_repeats() {
        // PATH as this OS writes it: `/a:/b::/a` on unix
        let path = |v: &[&str]| std::env::join_paths(v).unwrap().into_string().unwrap();
        assert_eq!(
            merge(&path(&["/a", "/b", "", "/a"]), &path(&["/c", "/b"])),
            vec!["/a", "/b", "/c"]
        );
    }

    #[test]
    fn executables_first_dir_wins_and_skips_non_exec() {
        let t = std::env::temp_dir().join(format!("fpath_{}", std::process::id()));
        let (a, b) = (t.join("a"), t.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let tool = crate::sys::program_fixture(&a, "tool", true);
        crate::sys::program_fixture(&b, "tool", true);
        crate::sys::program_fixture(&b, "other", true);
        crate::sys::program_fixture(&b, "data", false);
        let got = executables(&[a.to_string_lossy().into(), b.to_string_lossy().into()]);
        assert_eq!(
            got.get("tool").map(String::as_str),
            Some(tool.to_str().unwrap())
        );
        assert!(got.contains_key("other"));
        assert!(!got.contains_key("data"));
        std::fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn the_running_fantastic_is_always_known() {
        let got = with_self(BTreeMap::new(), Some("/x/fantastic".to_string()));
        assert_eq!(
            got.get("fantastic").map(String::as_str),
            Some("/x/fantastic")
        );
    }

    #[test]
    fn the_running_fantastic_wins_over_path() {
        let mut exes = BTreeMap::new();
        exes.insert("fantastic".to_string(), "/usr/bin/fantastic".to_string());
        exes.insert("claude".to_string(), "/usr/bin/claude".to_string());
        let got = with_self(exes, Some("/x/fantastic".to_string()));
        assert_eq!(
            got.get("fantastic").map(String::as_str),
            Some("/x/fantastic")
        );
        assert_eq!(
            got.get("claude").map(String::as_str),
            Some("/usr/bin/claude")
        );
    }

    #[test]
    fn without_an_own_path_nothing_changes() {
        let mut exes = BTreeMap::new();
        exes.insert("fantastic".to_string(), "/usr/bin/fantastic".to_string());
        let got = with_self(exes.clone(), None);
        assert_eq!(got, exes);
    }
}
