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

/// ONE STORE IO, THROUGH THE BATCH (C47).
///
/// Every persistence byte — the record merge read, the record write, the readme
/// probe and seed, the blob verbs, the ledger flush, the message-log probe and
/// append — crosses this function, and it is a `kernel.send` of ONE binary item
/// under OPERATOR authority. So each is a ledger execution with a call row, a
/// task row in `ps`, a settle row and a timeout, exactly like python's
/// `kernel_state` which does all of this with `kernel.send([...])`.
/// `send_with_binary` minted none of it, so the substrate's own writes were the
/// one class of work the message log could not see.
///
/// Operator authority: the substrate reading and writing its OWN store is
/// persistence bookkeeping, not an agent reaching out — it never runs against a
/// grant.
///
/// `Box::pin` breaks the async TYPE-recursion cycle: a persist (reached from
/// `send`'s dispatch) sends to the provider, whose dispatch could re-enter
/// `send`. Boxing erases the future type at this hop.
pub(crate) async fn store_send_binary(
    kernel: &Arc<Kernel>,
    store_id: &AgentId,
    header: Value,
    blob: Vec<u8>,
) -> (Value, Option<Vec<u8>>) {
    let out =
        Box::pin(crate::send::with_operator(kernel.send(vec![
            Item::new(store_id.clone(), header).with_binary(blob),
        ])))
        .await
        .remove(0);
    (out.result, out.body)
}

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
pub fn store_reldir(store_root: &Path, agent_root: &Path) -> String {
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
pub async fn read_via_store(kernel: &Arc<Kernel>, store_id: &AgentId, path: &str) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut offset: u64 = 0;
    loop {
        let (meta, body) = store_send_binary(
            kernel,
            store_id,
            serde_json::json!({"type": "read_stream", "path": path, "offset": offset}),
            Vec::new(),
        )
        .await;
        if meta.get("error").is_some() {
            return Vec::new();
        }
        out.extend_from_slice(&body.unwrap_or_default());
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

/// ONE `read_stream` call — no offset, no pumping to EOF.
///
/// Mirrors python's `_persist_via_store`/`_seed_doc_via_store` merge and
/// readme-probe reads exactly (C44/D6, C45): the provider's default chunk is
/// 65536 bytes, so a record or readme bigger than that reads back as a
/// PREFIX — python's own quirk, mirrored rather than "fixed" out from under
/// it (D6: python is immutable). [`read_via_store`] (below) is the pump-to-EOF
/// sibling `load_blob`/the ledger sidecar actually need and keep.
///
/// Returns `None` on an error reply (refused, missing file, sealed…),
/// `Some(bytes)` on success — EMPTY bytes on success still counts as PRESENT
/// (C45: `isinstance(got.get("bytes"), (bytes, bytearray))` is true for `b""`).
async fn read_stream_once(kernel: &Arc<Kernel>, store_id: &AgentId, path: &str) -> Option<Vec<u8>> {
    let (meta, body) = store_send_binary(
        kernel,
        store_id,
        serde_json::json!({"type": "read_stream", "path": path}),
        Vec::new(),
    )
    .await;
    if meta.get("error").is_some() {
        return None;
    }
    Some(body.unwrap_or_default())
}

/// A fresh 8-hex-char tmp suffix beside the target — python
/// `kernel_state.tools`' `f"{af}.tmp.{secrets.token_hex(4)}"`, drawn from the
/// same OS CSPRNG `lifecycle::mint_id`/`mint_guid` use.
fn mint_tmp_suffix() -> String {
    crate::lifecycle::hex(&crate::lifecycle::os_random(4))
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
/// - **Provider wired** → `read_stream` the existing JSON (ONE chunk, C44),
///   MERGE the agent's `record()` fields over it (overwriting only those keys,
///   leaving unknown keys + sidecars untouched), `write_stream` the merged
///   JSON to a TMP sibling and `rename` it over `agent.json` (C43 — the same
///   tmp+rename commit `text_file::write_atomic` uses). If the provider is
///   sealed it refuses and the write doesn't land — NO fallback; gating the
///   store is the operator's choice. `json.dumps(record, indent=2)` bytes,
///   python-exact including `\uXXXX` escapes (C42).
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
    // Merge: the WHOLE document through the provider (python pages to eof since
    // 2026-09-20; one chunk lost every key of a record over 64 KiB), then overlay
    // the kernel-managed keys.
    let existing = read_via_store(kernel, &store_id, &af).await;
    let mut on_disk: Map<String, Value> = match serde_json::from_slice::<Value>(&existing) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let record_json = crate::jsonsafe::record_value(&agent.record());
    if let Value::Object(record_map) = record_json {
        for (k, v) in record_map {
            on_disk.insert(k, v);
        }
    }
    // Records persist AS-IS — a single write of the merged JSON. There is no
    // `code` field to lift out to a `main.py` sidecar any more: a durable program
    // is a FILE some process (or a `write_stream`) put on disk, not a record field
    // (mirrors python's `kernel_state`, which no longer splits code).
    let json = fantastic_bundle::py_json_dumps(&Value::Object(on_disk), true, Some(2));
    let tmp = format!("{af}.tmp.{}", mint_tmp_suffix());
    let (write_meta, _) = store_send_binary(
        kernel,
        &store_id,
        serde_json::json!({"type": "write_stream", "path": tmp, "truncate": true}),
        json.into_bytes(),
    )
    .await;
    if write_meta.get("error").is_some() {
        // Leave the previous record untouched; a debris tmp is visible in `list`.
        return Ok(());
    }
    // `rename` is a TEXT-channel verb (`fantastic-file`'s stream verbs live on
    // the text channel; no blob rides this call) — the leg's one atomic op.
    Box::pin(crate::send::with_operator(kernel.send(vec![Item::new(
        store_id,
        serde_json::json!({"type": "rename", "old_path": tmp, "new_path": af}),
    )])))
    .await;
    Ok(())
}

/// Seed a `readme.md` (the bundle ships it via `Bundle::readme()`) AND its
/// keyed `docs/` tree (`Bundle::docs()`) into the agent's dir THROUGH the
/// discovered provider — python `_seed_readme_via_store`. Copy-if-missing PER
/// FILE — never clobber operator edits, an operator-EMPTIED file included (C45:
/// "present" is any successful reply, zero bytes counts). No-op in InMemory /
/// ephemeral / no-provider-wired.
pub async fn seed_readme(
    kernel: &Arc<Kernel>,
    agent: &Agent,
    readme: Option<&str>,
    docs: &[(&str, &str)],
) -> KernelResult<()> {
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
    let prefix = if reldir.is_empty() {
        String::new()
    } else {
        format!("{reldir}/")
    };
    if let Some(readme) = readme {
        seed_doc(kernel, &store_id, &format!("{prefix}readme.md"), readme).await;
    }
    for (key, body) in docs {
        seed_doc(kernel, &store_id, &format!("{prefix}docs/{key}.md"), body).await;
    }
    Ok(())
}

/// One sidecar doc THROUGH the provider, copy-if-missing — python
/// `_seed_doc_via_store`. Already present is ANY successful reply, zero bytes
/// included (C45); only an ERROR reply (missing file, refused…) means "not
/// there, seed it." A refused WRITE simply doesn't land — no fallback.
async fn seed_doc(kernel: &Arc<Kernel>, store_id: &AgentId, path: &str, body: &str) {
    if read_stream_once(kernel, store_id, path).await.is_some() {
        return;
    }
    store_send_binary(
        kernel,
        store_id,
        serde_json::json!({"type": "write_stream", "path": path, "truncate": true}),
        body.as_bytes().to_vec(),
    )
    .await;
}

/// Remove an agent's dir THROUGH the discovered provider (the `delete` verb is
/// recursive). Never removes the root (`reldir == ""`). No-op without a provider
/// (the dir, if any, was never the substrate's to remove). Mirrors Python's
/// `_forget_via_store`.
pub async fn forget(kernel: &Arc<Kernel>, agent: &Agent) -> KernelResult<()> {
    if kernel.storage.is_in_memory() || agent.ephemeral {
        return Ok(());
    }
    forget_path(kernel, &agent.root_path).await;
    Ok(())
}

/// [`forget`] by PATH — what the record writer holds for an agent that has
/// already left the index (python caches `_paths` for exactly this).
pub async fn forget_path(kernel: &Arc<Kernel>, agent_root: &Path) {
    if kernel.storage.is_in_memory() {
        return;
    }
    let Some(store_id) = find_store(kernel) else {
        return;
    };
    let Some(root) = kernel.root() else {
        return;
    };
    let reldir = store_reldir(&root.root_path, agent_root);
    if reldir.is_empty() {
        return; // never remove the root
    }
    let _ = Box::pin(crate::send::with_operator(kernel.send(vec![Item::new(
        store_id,
        // `recursive` is REQUIRED: an agent's dir holds `agent.json` (and any
        // sidecars), and a bare `delete` on a populated dir refuses — py parity
        // (`kernel_state._forget_via_store` has always passed it).
        serde_json::json!({"type": "delete", "path": reldir, "recursive": true}),
    )])))
    .await;
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
/// `kernel.agents` + `parent.children`. NO INBOX: a queue exists only where a
/// reader called [`Kernel::ensure_inbox`] for one.
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
        // An unreadable or corrupt `agent.json` is skipped SILENTLY — python
        // swallows both in `kernel_state/tools.py:131-155` and prints nothing
        // (A15.3). A rust-only WARN here landed on a one-shot's stdout and
        // stopped the reply parsing as JSON.
        let raw = match fs::read_to_string(&agent_file) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rec: AgentRecord = match serde_json::from_str(&raw) {
            Ok(r) => r,
            Err(_) => continue,
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
            // DISK POSITION IS AUTHORITATIVE, not the field. A record whose
            // `parent_id` disagrees with the directory it sits in would resolve
            // `$parent` differently on the two halves of the gate, and a reboot
            // would silently "correct" it either way (python's `read_tree`
            // overwrites the field from the walk; rust trusted the field).
            Some(parent.id.clone()),
            rec.meta.clone(),
            entry.clone(),
            false,
        );
        let id = agent.id.clone();
        // Register in the kernel index. No inbox — nothing here reads one.
        kernel.register(Arc::clone(&agent));
        // Wire into parent's children map.
        parent.add_child(Arc::clone(&agent));
        loaded.push(id.clone());

        // Recurse into grandchildren.
        let mut sub = load_children(kernel, bundles, agent)?;
        loaded.append(&mut sub);
    }

    Ok(loaded)
}

/// Walk a `.fantastic` tree → a FLAT list of records (root + descendants).
///
/// `parent_id` comes from DISK POSITION (authoritative), not from the field.
/// Pure read, called BEFORE any agent exists — a missing or unparseable ROOT
/// record returns `[]` (the caller reseeds), and a corrupt SIBLING is skipped.
/// Port of python `kernel_state.tools.read_tree`.
pub fn read_tree(root_dir: &Path) -> Vec<Value> {
    let root_file = root_dir.join("agent.json");
    let Ok(raw) = fs::read_to_string(&root_file) else {
        return Vec::new();
    };
    let Ok(Value::Object(root_rec)) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    // The root record DECLARES the children-dir name (default "agents"); a
    // self-describing layout sets e.g. "web_agents".
    let children_dir = root_rec
        .get("children_dir")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("agents")
        .to_string();
    let root_id = root_rec.get("id").cloned().unwrap_or(Value::Null);
    let mut out = vec![Value::Object(root_rec)];
    walk_tree(root_dir, &children_dir, &root_id, &mut out, &children_dir);
    out
}

fn walk_tree(root_dir: &Path, rel_dir: &str, parent_id: &Value, out: &mut Vec<Value>, cdir: &str) {
    let abs = root_dir.join(rel_dir);
    if !abs.is_dir() {
        return;
    }
    let Ok(rd) = fs::read_dir(&abs) else { return };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for entry in entries {
        let Some(name) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let rel = format!("{rel_dir}/{name}");
        let Ok(raw) = fs::read_to_string(root_dir.join(&rel).join("agent.json")) else {
            continue;
        };
        let Ok(Value::Object(mut rec)) = serde_json::from_str::<Value>(&raw) else {
            continue; // corrupt sibling → skip
        };
        rec.insert("parent_id".to_string(), parent_id.clone());
        let id = rec.get("id").cloned().unwrap_or(Value::Null);
        out.push(Value::Object(rec));
        walk_tree(root_dir, &format!("{rel}/{cdir}"), &id, out, cdir);
    }
}

/// An absolute dir for a record that is NOT (or no longer) live: walk its parent
/// chain in the kernel index and join every link under the root's children dir.
/// Python `kernel_state.tools._resolve_path`.
pub fn resolve_record_path(kernel: &Arc<Kernel>, id: &str, parent_id: Option<&str>) -> PathBuf {
    let Some(root) = kernel.root() else {
        return PathBuf::new();
    };
    let mut parts: Vec<String> = vec![id.to_string()];
    let mut pid: Option<String> = parent_id.map(str::to_string).or_else(|| {
        kernel
            .agents
            .get(&AgentId::from(id))
            .and_then(|a| a.parent_id.as_ref().map(|p| p.0.clone()))
    });
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    while let Some(p) = pid {
        if p == root.id.0 || !seen.insert(p.clone()) {
            break;
        }
        parts.push(p.clone());
        pid = kernel
            .agents
            .get(&AgentId::from(p.as_str()))
            .and_then(|a| a.parent_id.as_ref().map(|q| q.0.clone()));
    }
    let children_dir = root
        .meta
        .read()
        .ok()
        .and_then(|m| {
            m.get("children_dir")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "agents".to_string());
    let mut path = root.root_path.clone();
    for part in parts.iter().rev() {
        path = path.join(&children_dir).join(part);
    }
    path
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

// ─── the RECORD persist_writer (python `_FlushLoop`) ─────────────────

/// The dirty queues the state-stream subscriber fills and the writer drains.
#[derive(Default)]
struct Dirty {
    /// Ordered set of ids to write (python's `dict[str, None]`).
    persist: Vec<AgentId>,
    /// id → dir to remove.
    forget: Vec<(AgentId, PathBuf)>,
    /// id → dir cache, so a REMOVED agent's path is still known.
    paths: std::collections::HashMap<AgentId, PathBuf>,
}

/// A running record writer: the state-stream subscription plus the keepalive
/// task that debounces and flushes it. Python `kernel_state._FlushLoop`.
///
/// KEEPALIVE, because the persistence writer must outlive the quiet phase of
/// `shutdown_agents` — it is what flushes what shutdown produced.
#[derive(Clone)]
pub struct PersistWriter {
    dirty: Arc<std::sync::Mutex<Dirty>>,
    wake: Arc<tokio::sync::Notify>,
    token: crate::kernel::SubscriberToken,
    task: crate::tasks::TaskId,
}

impl PersistWriter {
    /// Drain the dirty queues once — the test seam AND the loop's body
    /// (python `aflush`).
    pub async fn flush(&self, kernel: &Arc<Kernel>) {
        flush_records(kernel, &self.dirty).await;
    }

    /// Unsubscribe and stop the task. NO FINAL FLUSH HERE: `stop` runs
    /// mid-teardown, when the store may already be down — a `create_agent` +
    /// `update_agent` followed straight away by a shutdown reached this flush with
    /// the provider gone and the update never landed. The last write happens in
    /// the root's `flush_pending`, before the walk.
    pub async fn stop(self, kernel: &Arc<Kernel>) {
        kernel.clear_record_writer();
        kernel.remove_state_subscriber(self.token);
        self.wake.notify_waiters();
        kernel.tasks.forget(&self.task);
    }
}

/// Subscribe the loader to the kernel state stream and start the debounced
/// record writer. Replaces the inline persist that used to ride `create_agent` /
/// `update_agent` / `delete_agent`.
///
/// NO PROVIDER ⇒ THE DIRTY SET SURVIVES. Python returns BEFORE clearing the
/// queues when no store is wired, so everything created before the store exists
/// lands the moment one is (`tools.py:703-704`).
pub fn install_persist_writer(kernel: &Arc<Kernel>, loader_id: &AgentId) -> PersistWriter {
    let dirty = Arc::new(std::sync::Mutex::new(Dirty::default()));
    {
        let mut d = dirty.lock().expect("dirty poisoned");
        for e in kernel.agents.iter() {
            d.paths.insert(e.key().clone(), e.value().root_path.clone());
        }
    }
    let wake = Arc::new(tokio::sync::Notify::new());
    let k_sub = Arc::clone(kernel);
    let d_sub = Arc::clone(&dirty);
    let w_sub = Arc::clone(&wake);
    let token = kernel.add_state_subscriber(Arc::new(move |evt: &Value, _body| {
        let Some(aid) = evt.get("agent_id").and_then(Value::as_str) else {
            return;
        };
        if aid.is_empty() {
            return;
        }
        let aid = AgentId::from(aid);
        let mut d = d_sub.lock().expect("dirty poisoned");
        match evt.get("kind").and_then(Value::as_str) {
            Some("added") | Some("updated") => {
                let Some(agent) = k_sub.agents.get(&aid).map(|e| Arc::clone(&e)) else {
                    return;
                };
                if agent.ephemeral {
                    return;
                }
                d.paths.insert(aid.clone(), agent.root_path.clone());
                d.forget.retain(|(i, _)| i != &aid);
                if !d.persist.contains(&aid) {
                    d.persist.push(aid);
                }
            }
            Some("removed") => {
                let Some(path) = d.paths.remove(&aid) else {
                    return;
                };
                d.persist.retain(|i| i != &aid);
                d.forget.push((aid, path));
            }
            _ => return,
        }
        drop(d);
        w_sub.notify_one();
    }));
    let k_run = Arc::clone(kernel);
    let d_run = Arc::clone(&dirty);
    let w_run = Arc::clone(&wake);
    let task = kernel.spawn_keepalive_task(loader_id, "persist_writer", async move {
        loop {
            w_run.notified().await;
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            flush_records(&k_run, &d_run).await;
        }
    });
    let writer = PersistWriter {
        dirty,
        wake,
        token,
        task,
    };
    // A ONE-SHOT DRAINS SYNCHRONOUSLY. python's `call` mode boots the root and
    // then awaits `root._fs_flush_loop.aflush()` before the process exits — the
    // debounce never ticks in that window. The kernel keeps the handle so a
    // caller with no reference to the loader bundle can do the same.
    kernel.set_record_writer(writer.clone());
    writer
}

async fn flush_records(kernel: &Arc<Kernel>, dirty: &Arc<std::sync::Mutex<Dirty>>) {
    // NO PROVIDER ⇒ the queues stay queued (python returns before clearing).
    if find_store(kernel).is_none() {
        return;
    }
    let (persist_ids, forget) = {
        let mut d = dirty.lock().expect("dirty poisoned");
        (
            std::mem::take(&mut d.persist),
            std::mem::take(&mut d.forget),
        )
    };
    for aid in persist_ids {
        let Some(agent) = kernel.agents.get(&aid).map(|e| Arc::clone(&e)) else {
            continue;
        };
        if agent.ephemeral {
            continue;
        }
        let _ = persist(kernel, &agent).await;
        // EVERY DRAINED WRITE SEEDS THE SIDECARS (python `_persist_via_store` →
        // `_seed_readme_via_store`), not only the create that first made the
        // record — a record written before a store was wired would otherwise
        // never get its readme.
        if let Some(bundle) = agent
            .handler_module
            .as_deref()
            .and_then(|hm| kernel.bundles.get(hm))
        {
            let _ = seed_readme(kernel, &agent, bundle.readme(), bundle.docs()).await;
        }
    }
    for (_, path) in forget {
        forget_path(kernel, &path).await;
    }
}

/// What the ROOT LOADER's `boot` installed — the handles a shutdown needs back.
///
/// The MACHINERY lives here, in the kernel, because both the shipped
/// `kernel_state.tools` bundle and the kernel's own test double drive exactly
/// the same sequence; the bundle owns the VERB SURFACE, not a second copy of
/// this. Python keeps these on the loader agent (`_fs_flush_loop`,
/// `_message_pump`).
#[derive(Default)]
pub struct RootLoaderState {
    /// The debounced record writer, when this loader watches (`watch != false`).
    pub writer: Option<PersistWriter>,
    /// The message pump — ROOT only.
    pub pump: Option<crate::messages_log::MessagePump>,
}

/// The loader's `boot`, python `kernel_state._boot`:
///
/// 1. ROOT only — REBUILD the execution ledger from the tape (the `call` +
///    `settle` rows the cold hydration already put back), then start the message
///    pump at the highest id hydration saw. That order: rebuilding mints nothing,
///    and nothing rebuilt is re-appended.
/// 2. Unless `watch=false` (a loader serving a REMOTE kernel answers verbs
///    only) — subscribe the record writer to the kernel state stream.
/// 3. ROOT only — mint the instance `guid` ONCE, AFTER the writer subscribes,
///    so the `updated` event it publishes is what carries the new record to
///    disk. An explicit or already-persisted guid always wins.
pub async fn boot_root_loader(kernel: &Arc<Kernel>, agent: &Arc<Agent>) -> RootLoaderState {
    let mut state = RootLoaderState::default();
    let is_root = agent.parent_id.is_none();
    if is_root {
        // THE TAPE IS THE LEDGER. `hydrate_messages` put the rows back cold; here
        // the settled calls become `executions` again, so dedup survives the
        // reboot out of one file.
        let _ = crate::messages_log::hydrate_executions(kernel);
        state.pump = Some(crate::messages_log::install_message_pump(
            kernel,
            kernel.rows.next_id() - 1,
        ));
    }
    let watching = agent
        .meta
        .read()
        .ok()
        .and_then(|m| m.get("watch").map(|v| v != &Value::Bool(false)))
        .unwrap_or(true);
    if !watching {
        return state;
    }
    state.writer = Some(install_persist_writer(kernel, &agent.id));
    if is_root {
        let has_guid = agent.meta.read().ok().is_some_and(|m| {
            m.get("guid")
                .and_then(Value::as_str)
                .is_some_and(|g| !g.is_empty())
        });
        if !has_guid {
            let mut patch = Map::new();
            patch.insert(
                "guid".to_string(),
                Value::String(crate::lifecycle::mint_guid()),
            );
            let rec = agent.update_meta(patch);
            kernel.publish_state(&serde_json::json!({
                "agent_id": rec.id,
                "kind": "updated",
                "changed": ["guid"],
            }));
        }
    }
    state
}

/// Unsubscribe and stop both loops — python `_MessagePump.stop` +
/// `_FlushLoop.stop`. The WRITING happened in `flush_pending`, before the walk.
pub async fn shutdown_root_loader(state: RootLoaderState, kernel: &Arc<Kernel>) {
    if let Some(w) = state.writer {
        w.stop(kernel).await;
    }
    if let Some(p) = state.pump {
        p.stop(kernel).await;
    }
}

pub mod blob;
pub use blob::{load_blob, persist_blob};
