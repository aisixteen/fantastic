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
//! disk untouched. Boot a build whose `register_default_bundles()`
//! covers that handler_module and the agent rehydrates on next boot.
//!
//! The log line shape is part of the wire contract (Python tests +
//! Rust selftest grep for it verbatim) so the word "installed" is
//! kept here from the Python lineage — for the Rust runtime it means
//! "linked into the binary at compile time", not any runtime install
//! mechanism:
//!
//! ```text
//! [kernel] skipping agent <id>: bundle <module> not installed in this runtime
//! ```

use crate::agent::{Agent, AgentId, AgentRecord};
use crate::bundle::BundleRegistry;
use crate::errors::{KernelError, KernelResult};
use crate::kernel::Kernel;
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// DISCOVER the persistence provider — mirrors Python `kernel_state._find_store`:
/// the first `file_bridge.tools` CHILD of the root whose `root` resolves to the
/// root loader's own store dir (its `.fantastic`). Bound by MATCH, not a fixed id
/// and **not kernel-composed** — an operator/LLM creates it (the one allowed
/// autoagent is the loader; the store is wired consciously). Returns its id, or
/// `None` when none is wired — in which case the live tree stays in RAM. **No
/// fallback** (the substrate never writes around a missing/sealed provider).
pub fn find_store(kernel: &Kernel) -> Option<AgentId> {
    let root = kernel.root()?;
    let base = std::env::current_dir().unwrap_or_default();
    // The loader's own dir is the store root (`.fantastic`). Resolve LEXICALLY
    // (absolutize + fold `.`/`..`) — no filesystem touch, so discovery works
    // before the dir exists (and stays a single deterministic path, no fallback).
    let want = normalize_lexical(&base, &root.root_path);
    for cid in root.child_ids() {
        let child = match kernel.agents.get(&cid) {
            Some(e) => Arc::clone(&e),
            None => continue,
        };
        if child.handler_module.as_deref() != Some("file_bridge.tools") {
            continue;
        }
        let r = child
            .meta
            .read()
            .ok()
            .and_then(|m| m.get("root").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        if normalize_lexical(&base, Path::new(&r)) == want {
            return Some(cid);
        }
    }
    None
}

/// Absolutize `p` against `base` and fold `.`/`..` lexically (no filesystem
/// access). Both sides of the store match go through this so a relative
/// `.fantastic` and an absolute workdir compare equal deterministically.
fn normalize_lexical(base: &Path, p: &Path) -> PathBuf {
    use std::path::Component;
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    let mut out: Vec<Component> = Vec::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(c);
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// An agent's dir RELATIVE to the loader's store root (`.fantastic`). The root
/// loader itself → `""` (its `agent.json` sits at the provider root); a child →
/// `agents/<id>` (recursively). Both paths share the same base, so this is a
/// plain prefix-strip (no canonicalization).
fn store_reldir(store_root: &Path, agent_root: &Path) -> String {
    match agent_root.strip_prefix(store_root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => agent_root.to_string_lossy().into_owned(),
    }
}

/// `read_stream` an agent.json-relative path through the provider, returning the
/// raw bytes (empty on any error / missing file). One chunk — records are small
/// (well under the default stream length), matching Python's single read.
async fn read_via_store(kernel: &Arc<Kernel>, store_id: &AgentId, path: &str) -> Vec<u8> {
    // Box::pin breaks the async TYPE-recursion cycle: a persist (reached from
    // `send`'s dispatch) sends to the provider, whose dispatch could re-enter
    // `send`. Boxing erases the future type at this hop.
    let (meta, body) = Box::pin(kernel.send_with_binary(
        store_id,
        serde_json::json!({"type": "read_stream", "path": path}),
        Vec::new(),
    ))
    .await;
    if meta.get("error").is_some() {
        return Vec::new();
    }
    body
}

/// Persist an agent's record onto its per-agent `agent.json` ("dirty binding"):
/// the in-RAM agent and the on-disk file aren't strictly coupled; this brings the
/// file up to date for the kernel-managed fields, leaving every other field
/// alone. The write goes **THROUGH the discovered `file_bridge` provider's
/// `write_stream`** — the substrate owns no `fs` surface of its own here.
///
/// Behaviour:
/// - **InMemory mode** → no-op (no filesystem at all)
/// - **Ephemeral agent** → no-op (per-process composition; never persists)
/// - **No provider wired** → no-op (RAM; lost on restart until a store is wired)
/// - **Provider wired** → `read_stream` the existing JSON, MERGE the agent's
///   `record()` fields over it (overwriting only those keys, leaving unknown
///   keys + sidecars untouched), and `write_stream` the merged JSON back
///   (`truncate`). If the provider is sealed it refuses and the write doesn't
///   land — NO fallback; gating the store is the operator's choice.
pub async fn persist(kernel: &Arc<Kernel>, agent: &Agent) -> KernelResult<()> {
    if kernel.storage.is_in_memory() || agent.ephemeral {
        return Ok(());
    }
    let Some(store_id) = find_store(kernel) else {
        return Ok(()); // RAM — nothing wired.
    };
    let Some(root) = kernel.root() else {
        return Ok(());
    };
    let reldir = store_reldir(&root.root_path, &agent.root_path);
    let af = if reldir.is_empty() {
        "agent.json".to_string()
    } else {
        format!("{reldir}/agent.json")
    };
    // Merge: read existing bytes through the provider, overlay kernel-managed keys.
    let mut on_disk: Map<String, Value> =
        match serde_json::from_slice::<Value>(&read_via_store(kernel, &store_id, &af).await) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        };
    let record_json =
        serde_json::to_value(agent.record()).expect("AgentRecord is always JSON-serializable");
    if let Value::Object(record_map) = record_json {
        for (k, v) in record_map {
            on_disk.insert(k, v);
        }
    }
    let json = serde_json::to_string_pretty(&Value::Object(on_disk))
        .expect("merged record is JSON-serializable");
    Box::pin(kernel.send_with_binary(
        &store_id,
        serde_json::json!({"type": "write_stream", "path": af, "truncate": true}),
        json.into_bytes(),
    ))
    .await;
    Ok(())
}

/// Seed a `readme.md` (the bundle ships it via `Bundle::readme()`) into the
/// agent's dir THROUGH the discovered provider. Copy-if-missing — never clobber
/// operator edits. No-op in InMemory / ephemeral / no-provider-wired.
pub async fn seed_readme(kernel: &Arc<Kernel>, agent: &Agent, readme: &str) -> KernelResult<()> {
    if kernel.storage.is_in_memory() || agent.ephemeral {
        return Ok(());
    }
    let Some(store_id) = find_store(kernel) else {
        return Ok(());
    };
    let Some(root) = kernel.root() else {
        return Ok(());
    };
    let reldir = store_reldir(&root.root_path, &agent.root_path);
    let path = if reldir.is_empty() {
        "readme.md".to_string()
    } else {
        format!("{reldir}/readme.md")
    };
    // Already present (any bytes) → leave it.
    if !read_via_store(kernel, &store_id, &path).await.is_empty() {
        return Ok(());
    }
    Box::pin(kernel.send_with_binary(
        &store_id,
        serde_json::json!({"type": "write_stream", "path": path, "truncate": true}),
        readme.as_bytes().to_vec(),
    ))
    .await;
    Ok(())
}

/// Remove an agent's dir THROUGH the discovered provider (the `delete` verb is
/// recursive). Never removes the root (`reldir == ""`). No-op without a provider
/// (the dir, if any, was never the substrate's to remove). Mirrors Python's
/// `_forget_via_store`.
pub async fn forget(kernel: &Arc<Kernel>, agent: &Agent) -> KernelResult<()> {
    if kernel.storage.is_in_memory() || agent.ephemeral {
        return Ok(());
    }
    let Some(store_id) = find_store(kernel) else {
        return Ok(());
    };
    let Some(root) = kernel.root() else {
        return Ok(());
    };
    let reldir = store_reldir(&root.root_path, &agent.root_path);
    if reldir.is_empty() {
        return Ok(()); // never remove the root
    }
    let _ = Box::pin(kernel.send(
        &store_id,
        serde_json::json!({"type": "delete", "path": reldir}),
    ))
    .await;
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
