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
use crate::outcome::Item;
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

    // SUBTREE, not direct children, BREADTH-FIRST.
    //
    // One level was scanned for a long time and the consequence was invisible: a
    // provider declared through a `static_loader` manifest lands as a GRANDCHILD
    // (a child of the roster loader, not of the root), was never discovered, and
    // the flush then no-opped with no error anywhere — the tree silently stayed
    // in RAM. Declaring it was never the problem; FINDING it was.
    //
    // Breadth-first so the shallowest match still wins, which keeps the previous
    // behaviour whenever a direct child matches.
    let mut queue: std::collections::VecDeque<AgentId> = root.child_ids().into_iter().collect();
    let mut seen: std::collections::HashSet<AgentId> = std::collections::HashSet::new();
    while let Some(cid) = queue.pop_front() {
        if !seen.insert(cid.clone()) {
            continue;
        }
        let Some(child) = kernel.agents.get(&cid).map(|e| Arc::clone(&e)) else {
            continue;
        };
        queue.extend(child.child_ids());
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
pub(crate) fn store_reldir(store_root: &Path, agent_root: &Path) -> String {
    match agent_root.strip_prefix(store_root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => agent_root.to_string_lossy().into_owned(),
    }
}

/// `read_stream` a store-relative path through the provider, PUMPED TO EOF,
/// returning the raw bytes (empty on any error / missing file).
///
/// To EOF, not one chunk. The provider's default stream length is 64 KiB, so a
/// single read silently TRUNCATED anything larger — and for a record that meant
/// `serde_json` failed on the fragment, the merge fell back to an empty map, and
/// every non-kernel key already on disk was quietly dropped on the next write.
/// A partial read that looks like a successful one is the worst shape available.
pub(crate) async fn read_via_store(
    kernel: &Arc<Kernel>,
    store_id: &AgentId,
    path: &str,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut offset: u64 = 0;
    loop {
        // Box::pin breaks the async TYPE-recursion cycle: a persist (reached from
        // `send`'s dispatch) sends to the provider, whose dispatch could re-enter
        // `send`. Boxing erases the future type at this hop.
        // Operator authority: the substrate reading its OWN store is persistence
        // bookkeeping, not an agent reaching out — it never runs against a grant.
        let (meta, body) = Box::pin(crate::send::with_operator(kernel.send_with_binary(
            store_id,
            serde_json::json!({"type": "read_stream", "path": path, "offset": offset}),
            Vec::new(),
        )))
        .await;
        if meta.get("error").is_some() {
            return Vec::new();
        }
        out.extend_from_slice(&body);
        // A provider that reports neither `eof` nor a moving cursor gets ONE
        // pass — the loop must not spin on a surface that cannot answer.
        let eof = meta.get("eof").and_then(Value::as_bool).unwrap_or(true);
        let next = meta.get("next_offset").and_then(Value::as_u64);
        match next {
            Some(n) if !eof && n > offset => offset = n,
            _ => return out,
        }
    }
}

/// Fold `main.py` back onto the record as `code`. Absent file ⇒ the field simply
/// isn't there, exactly as if it had never been set.
fn join_code(dir: &std::path::Path, mut rec: AgentRecord) -> AgentRecord {
    let cf = dir.join(CODE_FILE);
    if let Ok(code) = fs::read_to_string(&cf) {
        rec.meta.insert(CODE_FIELD.to_string(), Value::String(code));
    }
    rec
}

/// A record field that is PROGRAM TEXT, not metadata. It round-trips through its
/// own file beside `agent.json` instead of being embedded as an escaped JSON
/// string:
///
/// ```text
/// agents/<id>/agent.json      the record
/// agents/<id>/main.py         the program
/// ```
///
/// Nothing about the in-memory record changes — a bundle still sets `code` in meta
/// and still reads it back. This is purely how the loader lays it down, which is
/// the loader's own business, so a storeless kernel behaves exactly as before.
/// Mirrors python's `kernel_state` split.
pub const CODE_FIELD: &str = "code";
/// The program's filename inside the agent's own directory.
pub const CODE_FILE: &str = "main.py";

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
    // The program is a FILE, not a JSON string. Take it out of the record before
    // the record is written, and lay it down beside.
    let code = match on_disk.remove(CODE_FIELD) {
        Some(Value::String(c)) if !c.is_empty() => Some(c),
        _ => None,
    };
    let json = serde_json::to_string_pretty(&Value::Object(on_disk))
        .expect("merged record is JSON-serializable");
    // Operator authority: the loader persisting a record to its own store.
    Box::pin(crate::send::with_operator(kernel.send_with_binary(
        &store_id,
        serde_json::json!({"type": "write_stream", "path": af, "truncate": true}),
        json.into_bytes(),
    )))
    .await;
    if let Some(code) = code {
        let cf = if reldir.is_empty() {
            CODE_FILE.to_string()
        } else {
            format!("{reldir}/{CODE_FILE}")
        };
        Box::pin(crate::send::with_operator(kernel.send_with_binary(
            &store_id,
            serde_json::json!({"type": "write_stream", "path": cf, "truncate": true}),
            code.into_bytes(),
        )))
        .await;
    }
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
    Box::pin(crate::send::with_operator(kernel.send_with_binary(
        &store_id,
        serde_json::json!({"type": "write_stream", "path": path, "truncate": true}),
        readme.as_bytes().to_vec(),
    )))
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
    let _ = Box::pin(crate::send::with_operator(kernel.send(vec![Item::new(
        store_id,
        serde_json::json!({"type": "delete", "path": reldir}),
    )])))
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
            Ok(r) => join_code(&entry, r),
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

// ─── the DURABLE LEDGER (#37) ────────────────────────────────────────

/// The sidecar holding SETTLED executions, store-relative at the root.
pub const LEDGER_BLOB: &str = "ledger.json";

/// Write every settled row through the discovered store.
///
/// No store ⇒ RAM only. That is the AUTO-FLUSH LAW, not a fallback: with
/// nothing wired the tree does not persist either, and the ledger follows the
/// same rule rather than inventing a second disk surface.
pub async fn flush_ledger(kernel: &Arc<Kernel>) {
    if kernel.storage.is_in_memory() {
        return;
    }
    let Some(store_id) = find_store(kernel) else {
        return;
    };
    let doc = kernel.ledger.settled_snapshot().to_string();
    Box::pin(crate::send::with_operator(kernel.send_with_binary(
        &store_id,
        serde_json::json!({"type": "write_stream", "path": LEDGER_BLOB, "truncate": true}),
        doc.into_bytes(),
    )))
    .await;
}

/// Restore settled executions at boot, so dedup holds across a restart: a
/// completed step is never re-executed by a fresh process and a parked dead
/// letter stays dead.
///
/// Runs BEFORE the flusher is installed, so hydration never rewrites what it
/// just read. Every failure is LOUD and starts empty — a ledger that silently
/// half-loads would re-arm exactly the side effects it exists to prevent.
pub async fn rehydrate_ledger(kernel: &Arc<Kernel>) -> usize {
    let Some(store_id) = find_store(kernel) else {
        return 0;
    };
    let bytes = read_via_store(kernel, &store_id, LEDGER_BLOB).await;
    if bytes.is_empty() {
        return 0;
    }
    let doc: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            println!("  [ledger] {LEDGER_BLOB} unreadable, starting empty: {e}");
            return 0;
        }
    };
    // An unknown version is not a migration we can guess at.
    match doc.get("v").and_then(Value::as_u64) {
        Some(1) => {}
        other => {
            println!(
                "  [ledger] {LEDGER_BLOB} is version {other:?}, this kernel reads 1 \
                 — starting empty (no migration written)"
            );
            return 0;
        }
    }
    kernel.ledger.rehydrate(&doc)
}

/// Rehydrate, then install the debounced write-through.
///
/// Debounced because a batch settles many rows in a burst and each one would
/// otherwise rewrite the whole sidecar; settles that land mid-write coalesce
/// into ONE follow-up pass.
pub async fn install_ledger_persistence(kernel: &Arc<Kernel>) {
    let n = rehydrate_ledger(kernel).await;
    if n > 0 {
        println!("  [ledger] rehydrated {n} settled execution(s)");
    }
    let k = Arc::clone(kernel);
    let pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
    kernel.set_ledger_flusher(Box::new(move || {
        use std::sync::atomic::Ordering;
        // Already scheduled? Fold into that pass.
        if pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let k2 = Arc::clone(&k);
        let p2 = Arc::clone(&pending);
        tokio::spawn(async move {
            p2.store(false, Ordering::SeqCst);
            flush_ledger(&k2).await;
        });
    }));
}

mod blob;
pub(crate) use blob::{load_blob, persist_blob};
