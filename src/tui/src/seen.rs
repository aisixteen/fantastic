//! What THIS client has looked at — the only state that is the human's rather
//! than the kernel's, and therefore the only thing stored per client.
//!
//! It decides one thing: whether a finished root reads `done` (new) or `idle`
//! (you have seen it). Two people watching the same kernel disagree about that,
//! correctly, and neither writes it into the kernel.

// `contains`/`mark` are the pair `App::mark_seen` drives. The AI panel marks rows
// as the human reads them, which starts mattering the moment an agent that writes
// conversation rows is in the roster.
#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::kern::model::RowId;

#[derive(Debug, Default)]
pub struct Seen {
    ids: HashSet<RowId>,
    path: Option<PathBuf>,
    dirty: bool,
}

impl Seen {
    /// `.fantastic/cli/seen.yaml`. A missing or corrupt file is an empty set,
    /// never an error: nothing here is worth failing a launch over.
    pub fn load(cwd: &Path) -> Self {
        let path = cwd.join(".fantastic").join("cli").join("seen.yaml");
        let ids = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| fantastic_kernel::yaml_io::load(&t).ok())
            .and_then(|v| serde_json::from_value::<Vec<RowId>>(v).ok())
            .map(|v| v.into_iter().collect())
            .unwrap_or_default();
        Self {
            ids,
            path: Some(path),
            dirty: false,
        }
    }

    pub fn contains(&self, id: RowId) -> bool {
        self.ids.contains(&id)
    }

    pub fn mark(&mut self, id: RowId) {
        if self.ids.insert(id) {
            self.dirty = true;
        }
    }

    pub fn ids(&self) -> &HashSet<RowId> {
        &self.ids
    }

    /// Best effort, and deliberately so: losing this file costs one stale
    /// `done` badge.
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut v: Vec<RowId> = self.ids.iter().copied().collect();
        v.sort_unstable();
        let text = fantastic_kernel::yaml_io::dump(&serde_json::json!(v));
        if std::fs::write(path, text).is_ok() {
            self.dirty = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_a_file() {
        let dir = std::env::temp_dir().join("ft-seen-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = Seen::load(&dir);
        assert!(!s.contains(7));
        s.mark(7);
        s.flush();
        let s2 = Seen::load(&dir);
        assert!(s2.contains(7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_an_empty_set_not_an_error() {
        let s = Seen::load(Path::new("/nonexistent-dir-for-a-test"));
        assert!(s.ids().is_empty());
    }
}
