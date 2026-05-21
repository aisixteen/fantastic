//! Disk layout for the agent tree.
//!
//! ```text
//! .fantastic/
//! ├── lock.json                 {pid: u32}
//! ├── agent.json                root record
//! ├── readme.md                 seeded from the root bundle (if any)
//! └── agents/
//!     └── <child_id>/
//!         ├── agent.json
//!         ├── readme.md
//!         └── agents/<grandchild>/...
//! ```
//!
//! **Weak loading**: if a persisted child's `handler_module` isn't in
//! the active runtime's [`BundleRegistry`], the kernel logs one line
//! to stderr and skips the agent + its subtree. The record stays on
//! disk untouched. Install the bundle and the agent rehydrates on
//! the next boot. The log line shape is part of the contract:
//!
//! ```text
//! [kernel] skipping agent <id>: bundle <module> not installed in this runtime
//! ```

use crate::agent::{Agent, AgentId, AgentRecord};
use crate::bundle::BundleRegistry;
use crate::errors::{KernelError, KernelResult};
use crate::kernel::Kernel;
use serde_json::Map;
use std::fs;
use std::path::Path;
use std::sync::Arc;

/// Write an agent's record to its `agent.json`. Idempotent.
///
/// Ephemeral agents skip persistence entirely.
pub fn persist(agent: &Agent) -> KernelResult<()> {
    if agent.ephemeral {
        return Ok(());
    }
    fs::create_dir_all(&agent.root_path).map_err(|e| KernelError::Persistence {
        path: agent.root_path.clone(),
        source: e,
    })?;
    let path = agent.agent_file();
    let json = serde_json::to_string_pretty(&agent.record())
        .expect("AgentRecord is always JSON-serializable");
    fs::write(&path, json).map_err(|e| KernelError::Persistence { path, source: e })?;
    Ok(())
}

/// Seed a `readme.md` file from a `&str` source (the bundle ships
/// it via `include_str!`). No-op if the file already exists — we
/// preserve any user-edited content across reboots.
pub fn seed_readme(agent: &Agent, readme: &str) -> KernelResult<()> {
    if agent.ephemeral {
        return Ok(());
    }
    let path = agent.readme_file();
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(&agent.root_path).map_err(|e| KernelError::Persistence {
        path: agent.root_path.clone(),
        source: e,
    })?;
    fs::write(&path, readme).map_err(|e| KernelError::Persistence { path, source: e })?;
    Ok(())
}

/// Hydrate a parent's children from `<parent_root>/agents/`.
///
/// Weak-loads — children whose `handler_module` isn't registered in
/// `bundles` are skipped with a stderr log line; their subtree is
/// also skipped (the orphan can't have any registered descendants
/// reachable via routing). The record stays on disk.
///
/// Returns the list of (id, root_path) pairs that registered
/// successfully so callers can drive on-boot hooks (`boot` verb,
/// state events, etc.). The Agents themselves are also inserted into
/// `kernel.agents` + `parent.children` and have their inboxes
/// auto-vivified by [`Kernel::register`].
pub fn load_children(
    kernel: &Kernel,
    bundles: &BundleRegistry,
    parent: Arc<Agent>,
) -> KernelResult<Vec<AgentId>> {
    let mut loaded: Vec<AgentId> = Vec::new();
    let children_dir = parent.children_dir();
    if !children_dir.exists() {
        return Ok(loaded);
    }
    let entries = match fs::read_dir(&children_dir) {
        Ok(e) => e,
        Err(err) => {
            return Err(KernelError::Persistence {
                path: children_dir,
                source: err,
            });
        }
    };

    // Sort by name for deterministic load order (Python uses
    // `sorted(cdir.iterdir())`).
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    paths.sort();

    for entry in paths {
        let agent_file = entry.join("agent.json");
        if !agent_file.exists() {
            continue;
        }
        let raw = match fs::read_to_string(&agent_file) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %agent_file.display(), error = %e, "agent.json unreadable; skipping");
                continue;
            }
        };
        let rec: AgentRecord = match serde_json::from_str(&raw) {
            Ok(r) => r,
            Err(e) => {
                // Mirrors Python's behaviour: corrupt agent.json is a
                // skip-with-warning, not a hard error.
                tracing::warn!(
                    path = %agent_file.display(),
                    error = %e,
                    "agent.json is not valid JSON; skipping"
                );
                continue;
            }
        };

        // Weak-load check: if handler_module is set AND isn't in this
        // runtime's bundle registry, skip + log.
        if let Some(ref hm) = rec.handler_module {
            if bundles.get(hm).is_none() {
                // Exact log line shape (grep-able from CI + selftest).
                eprintln!(
                    "[kernel] skipping agent {}: bundle {} not installed in this runtime",
                    rec.id, hm
                );
                continue;
            }
        }

        let agent = Agent::new(
            AgentId(rec.id.clone()),
            rec.handler_module.clone(),
            rec.parent_id.as_ref().map(|p| AgentId(p.clone())),
            rec.meta.clone(),
            entry.clone(),
            false,
        );
        let id = agent.id.clone();
        // Register in the kernel index + auto-vivify inbox.
        let _rx = kernel.register(Arc::clone(&agent));
        // Wire into parent's children map.
        parent.children.insert(id.clone(), Arc::clone(&agent));
        loaded.push(id.clone());

        // Recurse into grandchildren.
        let mut sub = load_children(kernel, bundles, agent)?;
        loaded.append(&mut sub);
    }

    Ok(loaded)
}

/// Convenience: write a record to a specific path. Used by tests that
/// stage `.fantastic/` layouts without going through `Agent::new`.
pub fn write_record_at(path: &Path, rec: &AgentRecord) -> KernelResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| KernelError::Persistence {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    let json = serde_json::to_string_pretty(rec).expect("AgentRecord always serializable");
    fs::write(path, json).map_err(|e| KernelError::Persistence {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

/// Read an `agent.json` from `path` if it exists.
pub fn read_record_at(path: &Path) -> KernelResult<Option<AgentRecord>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|e| KernelError::Persistence {
        path: path.to_path_buf(),
        source: e,
    })?;
    let rec: AgentRecord = serde_json::from_str(&raw).map_err(|e| KernelError::CorruptRecord {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(Some(rec))
}

/// Standard relative path of the daemon lock from a workdir.
pub const LOCK_PATH: &str = ".fantastic/lock.json";

/// Standard relative path of the root agent's record from a workdir.
pub const ROOT_RECORD_PATH: &str = ".fantastic/agent.json";

/// Standard relative directory containing child records from a workdir.
pub const CHILDREN_DIR: &str = ".fantastic/agents";

/// Make `_unused` lint quiet — referenced by integration tests.
fn _path_constants_used() {
    let _ = (LOCK_PATH, ROOT_RECORD_PATH, CHILDREN_DIR);
    let _ = Map::<String, serde_json::Value>::new();
}
