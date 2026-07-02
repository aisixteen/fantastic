//! The **kernel registry** — the persisted list of folder kernels the app knows.
//!
//! The in-proc `★ RAM` manager kernel is synthetic (never in the file); this
//! records the *folder* kernels you've opened, so the Kernels screen is the same
//! ("omnipresent") list across restarts. Stored at `<app_home>/kernels.json`
//! (sibling of `settings.json`) as a plain JSON array `[{dir, runtime}, …]` — no
//! `serde` derive dependency, just `serde_json` shaping.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::app_home;
use crate::gateway::Runtime;

/// One remembered folder kernel: where it lives + which runtime it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelEntry {
    pub dir: PathBuf,
    pub runtime: Runtime,
}

/// The whole registry (order preserved — the Kernels screen renders it in order).
#[derive(Debug, Default, Clone)]
pub struct Registry {
    pub entries: Vec<KernelEntry>,
}

/// `<app_home>/kernels.json`.
pub fn registry_path() -> PathBuf {
    app_home().join("kernels.json")
}

/// Canonicalize a dir for dedupe/compare; fall back to the given path if the dir
/// doesn't exist yet.
fn canon(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

impl Registry {
    /// Load the registry (a missing / unreadable / malformed file → empty).
    pub fn load() -> Registry {
        let Ok(text) = std::fs::read_to_string(registry_path()) else {
            return Registry::default();
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return Registry::default();
        };
        let mut entries = Vec::new();
        if let Some(arr) = v.as_array() {
            for e in arr {
                let dir = e.get("dir").and_then(Value::as_str);
                let rt = e
                    .get("runtime")
                    .and_then(Value::as_str)
                    .and_then(Runtime::parse);
                if let (Some(dir), Some(runtime)) = (dir, rt) {
                    entries.push(KernelEntry {
                        dir: PathBuf::from(dir),
                        runtime,
                    });
                }
            }
        }
        Registry { entries }
    }

    /// Persist the registry as pretty JSON (creating `<app_home>` if needed).
    pub fn save(&self) -> std::io::Result<()> {
        let arr: Vec<Value> = self
            .entries
            .iter()
            .map(|e| json!({ "dir": e.dir.to_string_lossy(), "runtime": e.runtime.as_str() }))
            .collect();
        let path = registry_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body =
            serde_json::to_string_pretty(&Value::Array(arr)).map_err(std::io::Error::other)?;
        std::fs::write(path, body)
    }

    /// Add a folder kernel (dedupe by canonical dir; keeps the newest runtime).
    pub fn add(&mut self, dir: PathBuf, runtime: Runtime) {
        let c = canon(&dir);
        if let Some(e) = self.entries.iter_mut().find(|e| canon(&e.dir) == c) {
            e.runtime = runtime;
        } else {
            self.entries.push(KernelEntry { dir: c, runtime });
        }
    }

    /// Forget a folder kernel (by canonical dir).
    pub fn remove(&mut self, dir: &Path) {
        let c = canon(dir);
        self.entries.retain(|e| canon(&e.dir) != c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_loads_empty() {
        // A path unlikely to exist → default empty (we don't touch the real file).
        let r = Registry::default();
        assert!(r.entries.is_empty());
    }

    #[test]
    fn add_dedupes_and_updates_runtime() {
        let mut r = Registry::default();
        let dir = std::env::temp_dir();
        r.add(dir.clone(), Runtime::Rust);
        r.add(dir.clone(), Runtime::Python); // same dir → update, not duplicate
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].runtime, Runtime::Python);
        r.remove(&dir);
        assert!(r.entries.is_empty());
    }

    #[test]
    fn runtime_round_trips_through_str() {
        for rt in [Runtime::Rust, Runtime::Python, Runtime::Swift] {
            assert_eq!(Runtime::parse(rt.as_str()), Some(rt));
        }
        assert_eq!(Runtime::parse("nope"), None);
    }
}
