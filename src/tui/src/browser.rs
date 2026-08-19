//! The **folder browser** — an arrow-navigable directory picker used when creating
//! a new folder kernel. Pure model (no terminal) + unit-tested, like `hub.rs`.
//!
//! Only directories are listed (you pick a folder to root a kernel in); `..` is
//! always first. Up/Down move, Enter descends (or `..` climbs), and a distinct
//! "choose" key takes the CURRENT directory as the kernel dir.

use std::path::PathBuf;

/// The picker: the current directory + its subdirs (with `..` first) + selection.
pub struct Browser {
    pub cwd: PathBuf,
    /// `..` at index 0, then the subdirectory names (sorted, dotdirs hidden).
    pub entries: Vec<String>,
    pub sel: usize,
}

impl Browser {
    /// Open the browser rooted at `start` (falls back to `/` if it can't be read).
    pub fn open(start: PathBuf) -> Browser {
        let mut b = Browser {
            cwd: start,
            entries: Vec::new(),
            sel: 0,
        };
        b.read();
        b
    }

    /// (Re)read `cwd`'s subdirectories into `entries` (dirs only, dotdirs hidden,
    /// sorted; `..` prepended). Resets the selection to the top.
    fn read(&mut self) {
        let mut dirs: Vec<String> = std::fs::read_dir(&self.cwd)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| !n.starts_with('.'))
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        self.entries = std::iter::once("..".to_string()).chain(dirs).collect();
        self.sel = 0;
    }

    /// ↑ : previous entry (clamped).
    pub fn up(&mut self) {
        self.sel = self.sel.saturating_sub(1);
    }

    /// ↓ : next entry (clamped).
    pub fn down(&mut self) {
        if self.sel + 1 < self.entries.len() {
            self.sel += 1;
        }
    }

    /// ⏎ : descend into the selected entry (`..` climbs to the parent).
    pub fn enter(&mut self) {
        let name = self.entries.get(self.sel).cloned().unwrap_or_default();
        if name == ".." {
            if let Some(parent) = self.cwd.parent() {
                self.cwd = parent.to_path_buf();
            }
        } else if !name.is_empty() {
            self.cwd = self.cwd.join(name);
        }
        self.read();
    }

    /// The directory to root the new kernel in (the current `cwd`).
    pub fn choose(&self) -> PathBuf {
        self.cwd.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp subtree this test owns (no RNG: the process id + the CALLER's tag). The
    /// tag is per-test on purpose — a shared path made these tests race: they run in
    /// parallel, and one's `remove_dir_all` would delete the tree the other was walking.
    fn scratch(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("ft-browser-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("alpha")).unwrap();
        std::fs::create_dir_all(base.join("beta")).unwrap();
        std::fs::create_dir_all(base.join(".hidden")).unwrap();
        std::fs::write(base.join("afile.txt"), "x").unwrap();
        base
    }

    #[test]
    fn lists_dirs_only_dotdirs_hidden_dotdot_first() {
        let base = scratch("lists");
        let b = Browser::open(base.clone());
        assert_eq!(b.entries[0], "..");
        assert_eq!(&b.entries[1..], &["alpha".to_string(), "beta".to_string()]);
        // no file, no dotdir
        assert!(!b.entries.iter().any(|e| e == "afile.txt" || e == ".hidden"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn nav_clamps_and_enter_descends_and_climbs() {
        let base = scratch("nav");
        let mut b = Browser::open(base.clone());
        b.up(); // clamped at 0
        assert_eq!(b.sel, 0);
        b.down(); // → "alpha"
        assert_eq!(b.entries[b.sel], "alpha");
        b.enter(); // descend into alpha
        assert_eq!(b.cwd, base.join("alpha"));
        assert_eq!(b.entries[0], ".."); // alpha is empty except ".."
        b.enter(); // ".." → back to base
        assert_eq!(b.cwd, base);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn choose_returns_current_dir() {
        let base = scratch("choose");
        let b = Browser::open(base.clone());
        assert_eq!(b.choose(), base);
        let _ = std::fs::remove_dir_all(&base);
    }
}
