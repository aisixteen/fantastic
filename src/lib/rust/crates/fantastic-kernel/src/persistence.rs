//! Disk layout for the agent tree.
//!
//! ```text
//! .fantastic/
//! ├── lock.yaml                 pid: <u32>
//! ├── agent.yaml                root record
//! ├── readme.md                 seeded from the root bundle (if any)
//! └── agents/
//!     └── <child_id>/
//!         ├── agent.yaml
//!         ├── readme.md
//!         └── agents/<grandchild>/...
//! ```
//!
//! **Weak loading**: if a persisted child's `handler_module` isn't in
//! the active runtime's [`crate::bundle::BundleRegistry`], the kernel logs one line
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
use crate::errors::{KernelError, KernelResult};
use crate::kernel::Kernel;
use crate::outcome::Item;
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The record file of every agent dir — one block-style YAML mapping, keys in
/// record order ([`crate::yaml_io::dump`]).
pub const RECORD_FILE: &str = "agent.yaml";

/// A record file's text → its mapping, or `None` when it is not one (corrupt,
/// empty, a scalar). Callers skip or reseed; nothing is repaired.
pub fn parse_record(raw: &str) -> Option<Map<String, Value>> {
    match crate::yaml_io::load(raw) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// An agent record as the mapping its `agent.yaml` holds (id, handler_module,
/// meta…, parent_id — the persisted key order).
#[must_use]
pub fn record_map(rec: &AgentRecord) -> Value {
    crate::jsonsafe::record_value(rec)
}

/// ONE STORE IO, THROUGH THE BATCH.
///
/// Every persistence byte — the record merge read, the record write, the readme
/// probe and seed, the blob verbs — crosses [`store_read`] or [`store_write`],
/// each ONE call of the provider's handler as KERNEL BOOKKEEPING
/// ([`Kernel::bookkeeping`]): no gate, no call row, the handler run as the
/// provider with no origin.
///
/// THE SHAPE IS PYTHON'S. A read sends no bytes — its row is the plain payload
/// and a watcher gets a text frame; only its REPLY carries a raw chunk. A write
/// names the `bytes` slot where python's payload dict holds it
/// (`{type, path, bytes, truncate}`), so the row reads `bytes: {len: N}` in that
/// position.
///
/// The substrate reading and writing its OWN store is persistence bookkeeping,
/// not an agent reaching out — it never runs against a grant.
///
/// `Box::pin` breaks the async TYPE-recursion cycle: a persist (reached from
/// `send`'s dispatch) sends to the provider, whose dispatch could re-enter
/// `send`. Boxing erases the future type at this hop.
pub(crate) async fn store_read(
    kernel: &Arc<Kernel>,
    store_id: &AgentId,
    payload: Value,
) -> (Value, Option<Vec<u8>>) {
    store_send(kernel, Item::new(store_id.clone(), payload)).await
}

/// The write half of [`store_read`]: `payload` names the `bytes` slot (null) in
/// python's position; `body` is what fills it.
pub(crate) async fn store_write(
    kernel: &Arc<Kernel>,
    store_id: &AgentId,
    payload: Value,
    body: Vec<u8>,
) -> Value {
    store_send(
        kernel,
        Item::new(store_id.clone(), payload).with_binary_key("bytes", body),
    )
    .await
    .0
}

/// ONE call to the persistence provider — KERNEL BOOKKEEPING: the provider's
/// handler runs directly ([`Kernel::bookkeeping`]), not through a gated send. A
/// provider no longer in the tree answers the unknown-agent refusal.
async fn store_send(kernel: &Arc<Kernel>, item: Item) -> (Value, Option<Vec<u8>>) {
    let Some(store) = kernel.agents.get(&item.target).map(|e| Arc::clone(&e)) else {
        return (
            crate::refusal::unknown_agent(
                item.target.as_str(),
                kernel.agents.iter().map(|e| e.key().0.clone()).collect(),
            ),
            None,
        );
    };
    let out = Box::pin(kernel.bookkeeping(&store, item.payload, item.binary)).await;
    // a store's chunk is the reply's `bytes` — one anywhere else is not a chunk
    (out.result, out.body.and_then(|b| b.into_at("bytes")))
}

/// DISCOVER the ROOT loader's persistence provider — [`find_store_for`] the
/// tree's root.
pub fn find_store(kernel: &Kernel) -> Option<AgentId> {
    find_store_for(kernel, &*kernel.root()?)
}

/// The disk dir a loader owns — python `_root(agent)`: a `root` meta overrides
/// the loader's own address, so a SESSION loader serves a sub-namespace.
#[must_use]
pub fn loader_dir(loader: &Agent) -> PathBuf {
    loader
        .meta
        .read()
        .ok()
        .and_then(|m| {
            m.get("root")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| loader.root_path.clone())
}

/// The children-container name a loader's records nest under — python
/// `_children_name(agent)`: its own `children_dir` meta, default `agents`.
#[must_use]
pub fn loader_children(loader: &Agent) -> String {
    loader
        .meta
        .read()
        .ok()
        .and_then(|m| {
            m.get("children_dir")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| crate::kernel::DEFAULT_CHILDREN_DIR.to_string())
}

/// DISCOVER a loader's persistence provider — python `_find_store(agent)`: a
/// `file_bridge.tools` in THIS LOADER'S SUBTREE whose `root` resolves to the
/// loader's own dir ([`loader_dir`]). Bound by MATCH, not a fixed id and **not
/// kernel-composed** — an operator/LLM creates it. Returns its id, or `None`
/// when none is wired — in which case that loader's records stay in RAM. **No
/// fallback** (the substrate never writes around a missing/sealed provider),
/// and never another loader's store: a session loader usually finds none.
pub fn find_store_for(kernel: &Kernel, loader: &Agent) -> Option<AgentId> {
    // The running dir (`Kernel::workdir`) — the ONE clamp base; a root with no
    // disk address has no store to discover.
    let base = kernel.workdir().ok()?;
    // Resolve LEXICALLY (absolutize + fold `.`/`..`) — no filesystem touch, so
    // discovery works before the dir exists (and stays a single deterministic
    // path, no fallback).
    let want = normalize_lexical(&base, &loader_dir(loader));

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
    let mut queue: std::collections::VecDeque<AgentId> = loader.child_ids().into_iter().collect();
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

/// An agent's dir RELATIVE to a loader's store root — python
/// `_store_reldir(agent, abs_path)`. Both sides are absolutized against the
/// running dir first, so a relative `root` meta and an absolute agent address
/// compare.
pub(crate) fn loader_reldir(kernel: &Kernel, loader: &Agent, abs: &Path) -> String {
    match kernel.workdir() {
        Ok(base) => store_reldir(
            &normalize_lexical(&base, &loader_dir(loader)),
            &normalize_lexical(&base, abs),
        ),
        Err(_) => store_reldir(&loader_dir(loader), abs),
    }
}

// ─── loud persistence ────────────────────────────────────────────
// A store that cannot hold the records, and every refused persist write, show
// up on stderr AND as `persistence.error` on the root reflect — and
// `persistence.provider` is reported only while writes land, never a silent
// RAM-only kernel. Python `kernel_state` `_persist_failed` /
// `_persist_landed` / `_check_store_allow`.

/// The last refused/failed persist write PER LOADER — absent while its writes
/// land. Kept ON the kernel ([`Kernel::extension`]); python keeps it on the
/// loader agent (`agent._persist_error`).
#[derive(Default)]
pub struct PersistHealth(std::sync::Mutex<std::collections::HashMap<AgentId, Value>>);

fn health(kernel: &Kernel) -> Arc<PersistHealth> {
    kernel.extension::<PersistHealth>("persistence.health")
}

/// The failing write the ROOT loader's reflect names under
/// `persistence.error`, if any.
#[must_use]
pub fn persist_error(kernel: &Kernel) -> Option<Value> {
    let root = kernel.root()?;
    health(kernel)
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&root.id)
        .cloned()
}

/// A refused/failed persist write is LOUD: stderr, and `persistence.error` on
/// that loader (the root's reflect then reports no provider) until a write
/// lands again.
fn persist_failed(kernel: &Kernel, loader: &AgentId, store: &AgentId, path: &str, why: &Value) {
    let msg = match why {
        Value::Object(o) => o.get("error").cloned().unwrap_or_else(|| why.clone()),
        other => other.clone(),
    };
    eprintln!(
        "  [{loader}] persist through {} failed: {path}: {}",
        fantastic_bundle::py_repr(&store.0),
        fantastic_bundle::py_str(&msg)
    );
    health(kernel)
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            loader.clone(),
            serde_json::json!({"store": store.0, "path": path, "error": msg}),
        );
}

fn persist_landed(kernel: &Kernel, loader: &AgentId) {
    health(kernel)
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(loader);
}

/// A reply that failed: not an object, or an object with a truthy `error`.
fn write_failed(r: &Value) -> bool {
    match r {
        Value::Object(o) => o.get("error").is_some_and(fantastic_bundle::py_truthy),
        _ => true,
    }
}

/// Does a store's `allow` list expose `path` (a listed path, or under a listed
/// directory)? The file_bridge's own rule, read off its record.
fn covers(allow: Option<&Value>, path: &str) -> bool {
    allow.and_then(Value::as_array).is_some_and(|items| {
        items.iter().filter_map(Value::as_str).any(|s| {
            if fantastic_bundle::is_absolute(s) {
                return false;
            }
            let e = normalize_lexical(
                Path::new("/"),
                Path::new(if s.is_empty() { "." } else { s }),
            );
            let e = wire_path(e.strip_prefix("/").unwrap_or(&e));
            e.is_empty() || e == path || path.starts_with(&format!("{e}/"))
        })
    })
}

/// At a watching loader's boot: a discovered store whose `allow` does not
/// expose the records (`agent.yaml` and the children dir) would refuse every
/// write — say so now. Python `_check_store_allow(agent)`, for EVERY watching
/// loader against its own store.
pub fn check_store_allow(kernel: &Kernel, loader: &Agent) {
    let Some(store_id) = find_store_for(kernel, loader) else {
        return;
    };
    let allow = kernel
        .agents
        .get(&store_id)
        .and_then(|a| a.meta.read().ok().and_then(|m| m.get("allow").cloned()));
    // python `(_RECORD_FILE, _children_name(agent))` — the loader's own
    // children-dir name, not a literal.
    let children = loader_children(loader);
    let missing: Vec<&str> = [RECORD_FILE, children.as_str()]
        .into_iter()
        .filter(|p| !covers(allow.as_ref(), p))
        .collect();
    if !missing.is_empty() {
        persist_failed(
            kernel,
            &loader.id,
            &store_id,
            &missing.join(", "),
            &Value::String(format!(
                "the store's `allow` does not expose the records (add them, or `.`): allow={}",
                fantastic_bundle::py_repr_value(allow.as_ref().unwrap_or(&Value::Null))
            )),
        );
    }
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
/// loader itself → `""` (its `agent.yaml` sits at the provider root); a child →
/// `agents/<id>` (recursively), `/`-separated on every OS — it is a wire path.
/// Both paths share the same base, so this is a plain prefix-strip (no
/// canonicalization).
pub fn store_reldir(store_root: &Path, agent_root: &Path) -> String {
    match agent_root.strip_prefix(store_root) {
        Ok(rel) => wire_path(rel),
        Err(_) => agent_root.to_string_lossy().into_owned(),
    }
}

/// A relative path as the wire spells it: its parts joined with `/` on every OS
/// (python `PurePath.as_posix`).
fn wire_path(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
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
        let (meta, body) = store_read(
            kernel,
            store_id,
            serde_json::json!({"type": "read_stream", "path": path, "offset": offset}),
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

/// A fresh 8-hex-char tmp suffix beside the target — python
/// `kernel_state.tools`' `f"{af}.tmp.{secrets.token_hex(4)}"`, drawn from the
/// same OS CSPRNG `lifecycle::mint_id`/`mint_guid` use.
fn mint_tmp_suffix() -> String {
    crate::lifecycle::hex(&crate::lifecycle::os_random(4))
}

/// Persist an agent's record onto its per-agent `agent.yaml` ("dirty binding"):
/// the in-RAM agent and the on-disk file aren't strictly coupled; this brings the
/// file up to date for the kernel-managed fields, leaving every other field
/// alone. The write goes **THROUGH the discovered `file_bridge` provider's
/// `write_stream`** — the substrate owns no `fs` surface of its own here.
///
/// Behaviour:
/// - **InMemory mode** → no-op (no filesystem at all)
/// - **Ephemeral agent** → no-op (per-process composition; never persists)
/// - **No provider wired** → no-op (RAM; lost on restart until a store is wired)
/// - **Provider wired** → `read_stream` the existing YAML (to eof),
///   MERGE the agent's `record()` fields over it (overwriting only those keys,
///   leaving unknown keys + sidecars untouched), `write_stream` the merged
///   YAML to a TMP sibling and `rename` it over `agent.yaml`. If the
///   provider refuses (sealed, unexposed) the write doesn't land — NO fallback
///   — and the refusal is LOUD (`persist_failed`). `yaml_io.dump(record)`
///   bytes, python-exact.
///
/// `Ok(true)` only when the record LANDED (written and renamed) — python
/// `_persist_via_store` returns after `_persist_failed`, so nothing that follows
/// a landed write (the readme/docs seed) runs for a failed one.
pub async fn persist(kernel: &Arc<Kernel>, loader: &Agent, agent: &Agent) -> KernelResult<bool> {
    if kernel.storage.is_in_memory() || agent.ephemeral {
        return Ok(false);
    }
    let Some(store_id) = find_store_for(kernel, loader) else {
        return Ok(false); // RAM — nothing wired.
    };
    let reldir = loader_reldir(kernel, loader, &agent.root_path);
    let af = if reldir.is_empty() {
        RECORD_FILE.to_string()
    } else {
        format!("{reldir}/{RECORD_FILE}")
    };
    // Merge: the WHOLE document through the provider (python pages to eof; one
    // chunk would lose every key of a record over 64 KiB), then overlay
    // the kernel-managed keys.
    let existing = read_via_store(kernel, &store_id, &af).await;
    let mut on_disk: Map<String, Value> = std::str::from_utf8(&existing)
        .ok()
        .and_then(parse_record)
        .unwrap_or_default();
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
    let body = crate::yaml_io::dump(&Value::Object(on_disk));
    let tmp = format!("{af}.tmp.{}", mint_tmp_suffix());
    let write_meta = store_write(
        kernel,
        &store_id,
        serde_json::json!({"type": "write_stream", "path": tmp, "bytes": null, "truncate": true}),
        body.into_bytes(),
    )
    .await;
    if write_failed(&write_meta) {
        // Leave the previous record untouched; a debris tmp is visible in `list`.
        persist_failed(kernel, &loader.id, &store_id, &af, &write_meta);
        return Ok(false);
    }
    // `rename` is a TEXT-channel verb (`fantastic-file`'s stream verbs live on
    // the text channel; no blob rides this call) — the leg's one atomic op.
    let moved = store_send(
        kernel,
        Item::new(
            store_id.clone(),
            serde_json::json!({"type": "rename", "old_path": tmp, "new_path": af}),
        ),
    )
    .await
    .0;
    if write_failed(&moved) {
        persist_failed(kernel, &loader.id, &store_id, &af, &moved);
        return Ok(false);
    }
    persist_landed(kernel, &loader.id);
    Ok(true)
}

/// Remove an agent's dir THROUGH the discovered provider (the `delete` verb is
/// recursive). Never removes the root (`reldir == ""`). No-op without a provider
/// (the dir, if any, was never the substrate's to remove). Mirrors Python's
/// `_forget_via_store`.
pub async fn forget(kernel: &Arc<Kernel>, loader: &Agent, agent: &Agent) -> KernelResult<()> {
    if kernel.storage.is_in_memory() || agent.ephemeral {
        return Ok(());
    }
    forget_path(kernel, loader, &agent.root_path).await;
    Ok(())
}

/// [`forget`] by PATH — what the record writer holds for an agent that has
/// already left the index (python caches `_paths` for exactly this).
pub async fn forget_path(kernel: &Arc<Kernel>, loader: &Agent, agent_root: &Path) {
    if kernel.storage.is_in_memory() {
        return;
    }
    let Some(store_id) = find_store_for(kernel, loader) else {
        return;
    };
    let reldir = loader_reldir(kernel, loader, agent_root);
    if reldir.is_empty() {
        return; // never remove the root
    }
    let out = store_send(
        kernel,
        Item::new(
            store_id.clone(),
            // `recursive` is REQUIRED: an agent's dir holds `agent.yaml` (and any
            // sidecars), and a bare `delete` on a populated dir refuses — py parity
            // (`kernel_state._forget_via_store` has always passed it).
            serde_json::json!({"type": "delete", "path": reldir, "recursive": true}),
        ),
    )
    .await
    .0;
    if out
        .as_object()
        .is_some_and(|o| o.get("error").is_some_and(fantastic_bundle::py_truthy))
    {
        persist_failed(kernel, &loader.id, &store_id, &reldir, &out);
    }
}

/// python `fs.resolve(root, rel)` for the tree walks: `rel` resolved under the
/// resolved `root` ([`fantastic_bundle::realpath`] — `..` and every symlink,
/// dangling or not, followed). Python's walk opens only through `file_bridge.fs`,
/// so a path that lands outside refuses — loud — with `fs`'s own text: an
/// escaping `children_dir`, or an entry (or its `agent.yaml`) linked outside.
///
/// # Errors
/// `fs: path '<rel>' escapes root` — python's `ValueError` text; the cwd's
/// `OSError` text when a relative root cannot absolutize
/// ([`fantastic_bundle::realpath`]).
pub(crate) fn clamp(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let escapes = || format!("fs: path {} escapes root", fantastic_bundle::py_repr(rel));
    let root_p = fantastic_bundle::realpath(root)?;
    if fantastic_bundle::is_absolute(rel) {
        return Err(escapes());
    }
    let target = if rel.is_empty() {
        root_p.clone()
    } else {
        fantastic_bundle::realpath(&root_p.join(rel))?
    };
    if !target.starts_with(&root_p) {
        // the two paths said, LOUD: python's text names neither
        eprintln!(
            "  [fs] {} resolved to {} — not under {}",
            fantastic_bundle::py_repr(rel),
            target.display(),
            root_p.display()
        );
        return Err(escapes());
    }
    Ok(target)
}

/// python `_read_record` under the clamp: one `agent.yaml` (`rel` under `root`)
/// → its mapping, `None` when absent, unreadable or not a mapping. The clamp is
/// OUTSIDE python's `try`, so a path that escapes refuses.
///
/// # Errors
/// [`clamp`]'s text.
pub(crate) fn read_record_clamped(
    root: &Path,
    rel: &str,
) -> Result<Option<Map<String, Value>>, String> {
    let path = clamp(root, rel)?;
    Ok(fs::read_to_string(path)
        .ok()
        .and_then(|raw| parse_record(&raw)))
}

/// python `_walk`: the records under `rel_dir`, depth-first in name order, each
/// `parent_id` set from its DISK POSITION; corrupt siblings skip. Every path is
/// [`clamp`]ed under `root_dir` exactly where python's `fs` clamps it.
///
/// # Errors
/// [`clamp`]'s text, for the first path that escapes `root_dir`.
pub(crate) fn walk_tree(
    root_dir: &Path,
    rel_dir: &str,
    parent_id: &Value,
    out: &mut Vec<Value>,
    cdir: &str,
) -> Result<(), String> {
    let abs = clamp(root_dir, rel_dir)?;
    if !abs.is_dir() {
        return Ok(());
    }
    let Ok(rd) = fs::read_dir(&abs) else {
        return Ok(());
    };
    let mut names: Vec<std::ffi::OsString> = rd.flatten().map(|e| e.file_name()).collect();
    names.sort();
    for name in names {
        let Some(name) = name.to_str() else {
            continue;
        };
        let rel = format!("{rel_dir}/{name}");
        let Some(mut rec) = read_record_clamped(root_dir, &format!("{rel}/{RECORD_FILE}"))? else {
            continue; // absent or corrupt sibling → skip
        };
        rec.insert("parent_id".to_string(), parent_id.clone());
        let id = rec.get("id").cloned().unwrap_or(Value::Null);
        out.push(Value::Object(rec));
        walk_tree(root_dir, &format!("{rel}/{cdir}"), &id, out, cdir)?;
    }
    Ok(())
}

/// A loader's PERSISTED records → `{id: parent_id}`, the parent taken from DISK
/// POSITION under `root_dir` (authoritative, as the loader's `read_tree` does): a record
/// directly under `<root_dir>/<children_dir>` is parented to the loader itself.
/// A record the host never runs — a federated kernel's, sent through a bridge —
/// exists only here, so a walk that stops at the live tree loses every record
/// nested under it. Python `kernel_state.tools._disk_parents`.
///
/// # Errors
/// [`clamp`]'s text — the walk refuses where python's does.
pub fn disk_parents(
    root_dir: &Path,
    children_dir: &str,
    loader_id: &str,
) -> Result<std::collections::HashMap<String, Option<String>>, String> {
    let mut out = Vec::new();
    walk_tree(
        root_dir,
        children_dir,
        &Value::from(loader_id),
        &mut out,
        children_dir,
    )?;
    Ok(out
        .into_iter()
        .filter_map(|r| {
            let id = r.get("id")?.as_str()?.to_string();
            let parent = r
                .get("parent_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some((id, parent))
        })
        .collect())
}

/// WHERE A RECORD REALLY IS, for one loader: the LIVE tree first, then the
/// loader's persisted records (read from disk once, on first need). Python
/// `kernel_state.tools._where` + its per-walk `disk` cache.
pub struct RecordPlaces<'a> {
    kernel: &'a Kernel,
    root_dir: PathBuf,
    children_dir: String,
    loader_id: String,
    disk: Option<std::collections::HashMap<String, Option<String>>>,
}

impl<'a> RecordPlaces<'a> {
    /// The places a loader (`loader_id`, owning `root_dir`, children under
    /// `children_dir`) knows.
    #[must_use]
    pub fn new(
        kernel: &'a Kernel,
        root_dir: PathBuf,
        children_dir: String,
        loader_id: String,
    ) -> Self {
        Self {
            kernel,
            root_dir,
            children_dir,
            loader_id,
            disk: None,
        }
    }

    /// `(known, parent)` for one id: the live tree, then the loader's persisted
    /// records. Unknown in both ⇒ `(false, None)` — the caller decides; nothing is
    /// guessed.
    ///
    /// # Errors
    /// [`disk_parents`]' clamp text.
    pub fn locate(&mut self, node: &str) -> Result<(bool, Option<String>), String> {
        if let Some(live) = self.kernel.agents.get(&AgentId::from(node)) {
            return Ok((true, live.parent_id.as_ref().map(|p| p.0.clone())));
        }
        if self.disk.is_none() {
            self.disk = Some(disk_parents(
                &self.root_dir,
                &self.children_dir,
                &self.loader_id,
            )?);
        }
        let disk = self.disk.as_ref().expect("filled above");
        Ok(match disk.get(node) {
            Some(parent) => (true, parent.clone()),
            None => (false, None),
        })
    }

    /// A record's on-disk dir under the loader's root, by walking its parent
    /// chain where it really is (live, then disk) — so a record nested 2+ levels
    /// under a federated record lands where it belongs. A record that already
    /// exists keeps ITS position; `payload_parent` places only a NEW one. Python
    /// `kernel_state.tools._resolve_path`.
    ///
    /// # Errors
    /// [`disk_parents`]' clamp text.
    pub fn resolve(&mut self, id: &str, payload_parent: Option<&str>) -> Result<PathBuf, String> {
        let mut parts: Vec<String> = vec![id.to_string()];
        let (known, mut pid) = self.locate(id)?;
        if !known {
            pid = payload_parent.map(str::to_string);
        }
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(p) = pid {
            if p.is_empty() || p == self.loader_id || !seen.insert(p.clone()) {
                break;
            }
            parts.push(p.clone());
            pid = self.locate(&p)?.1;
        }
        let mut path = self.root_dir.clone();
        for part in parts.iter().rev() {
            path = path.join(&self.children_dir).join(part);
        }
        Ok(path)
    }
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
    let body = crate::yaml_io::dump(&crate::jsonsafe::record_value(rec));
    fs::write(path, body).map_err(|e| KernelError::Persistence {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

/// Read an `agent.yaml` from `path` if it exists.
pub fn read_record_at(path: &Path) -> KernelResult<Option<AgentRecord>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|e| KernelError::Persistence {
        path: path.to_path_buf(),
        source: e,
    })?;
    let map = parse_record(&raw).ok_or_else(|| KernelError::CorruptRecord {
        path: path.to_path_buf(),
        reason: "not a YAML mapping".to_string(),
    })?;
    let rec: AgentRecord =
        serde_json::from_value(Value::Object(map)).map_err(|e| KernelError::CorruptRecord {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
    Ok(Some(rec))
}

/// The daemon lock's file name inside a kernel's state dir.
pub const LOCK_FILE: &str = "lock.yaml";

/// Standard relative path of the daemon lock from a workdir.
pub const LOCK_PATH: &str = ".fantastic/lock.yaml";

/// Standard relative path of the root agent's record from a workdir.
pub const ROOT_RECORD_PATH: &str = ".fantastic/agent.yaml";

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
    /// Ids `added` since the writer started whose record no drain has taken
    /// yet: nothing of theirs is on disk, so their `removed` has no dir to
    /// forget. An id leaves the set the moment a drain takes its write (before
    /// the write lands), so a `removed` racing that write still forgets the dir.
    unwritten: std::collections::HashSet<AgentId>,
}

/// A running record writer: the state-stream subscription plus the keepalive
/// task that debounces and flushes it. Python `kernel_state._FlushLoop`.
///
/// KEEPALIVE, because the persistence writer must outlive the quiet phase of
/// `shutdown_agents` — it is what flushes what shutdown produced.
#[derive(Clone)]
pub struct PersistWriter {
    loader: AgentId,
    dirty: Arc<std::sync::Mutex<Dirty>>,
    /// ONE DRAIN AT A TIME: the debounced loop and every [`Self::flush`] hold
    /// it for a whole drain, and a flush waits its turn (FIFO).
    drain: Arc<tokio::sync::Mutex<()>>,
    wake: Arc<tokio::sync::Notify>,
    token: crate::kernel::SubscriberToken,
    task: crate::tasks::TaskId,
}

impl PersistWriter {
    /// Drain the dirty queues once — the test seam AND the loop's body
    /// (python `aflush`).
    ///
    /// When it returns, every write that was dirty when it was called has
    /// LANDED — also the ones the debounced loop took first, so the caller finds
    /// the record and its readme on disk.
    pub async fn flush(&self, kernel: &Arc<Kernel>) {
        flush_records(kernel, &self.loader, &self.dirty, &self.drain).await;
    }

    /// Mark one record dirty with NO state event and wake the writer — the
    /// root's id-block reservation inside the silent ledger flush.
    pub(crate) fn mark_dirty(&self, kernel: &Kernel, id: &AgentId) {
        if mark_persist(kernel, &self.dirty, id, false) {
            self.wake.notify_one();
        }
    }

    /// The ids queued for the next write — a test seam.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> Vec<AgentId> {
        self.dirty.lock().expect("dirty poisoned").persist.clone()
    }

    /// Unsubscribe and stop the task. NO FINAL FLUSH HERE: `stop` runs
    /// mid-teardown, when the store may already be down — a `create_agent` +
    /// `update_agent` followed straight away by a shutdown reached this flush with
    /// the provider gone and the update never landed. The last write happens in
    /// the root's `flush_pending`, before the walk.
    pub async fn stop(self, kernel: &Arc<Kernel>) {
        if kernel.root().is_some_and(|r| r.id == self.loader) {
            kernel.clear_record_writer();
        }
        kernel.remove_state_subscriber(self.token);
        self.wake.notify_waiters();
        kernel.tasks.forget(&self.task);
    }
}

/// Queue one live, non-ephemeral record for the writer — what an `added` /
/// `updated` event does. `false` when there is nothing to write.
fn mark_persist(
    kernel: &Kernel,
    dirty: &std::sync::Mutex<Dirty>,
    aid: &AgentId,
    added: bool,
) -> bool {
    let Some(agent) = kernel.agents.get(aid).map(|e| Arc::clone(&e)) else {
        return false;
    };
    if agent.ephemeral {
        return false;
    }
    let mut d = dirty.lock().expect("dirty poisoned");
    d.paths.insert(aid.clone(), agent.root_path.clone());
    // A re-created id whose old dir is still queued for removal writes into
    // that dir (it exists); any other `added` has nothing on disk.
    let queued = d.forget.len();
    d.forget.retain(|(i, _)| i != aid);
    if d.forget.len() == queued && added {
        d.unwritten.insert(aid.clone());
    }
    if !d.persist.contains(aid) {
        d.persist.push(aid.clone());
    }
    true
}

/// Subscribe the loader to the kernel state stream and start the debounced
/// record writer.
///
/// NO PROVIDER ⇒ THE DIRTY SET SURVIVES. Python returns BEFORE clearing the
/// queues when no store is wired, so everything created before the store exists
/// lands the moment one is (`tools.py:703-704`).
pub fn install_persist_writer(kernel: &Arc<Kernel>, loader_id: &AgentId) -> PersistWriter {
    let dirty = Arc::new(std::sync::Mutex::new(Dirty::default()));
    {
        let mut d = dirty.lock().expect("dirty poisoned");
        for e in kernel.agents.iter() {
            if !e.value().ephemeral {
                d.paths.insert(e.key().clone(), e.value().root_path.clone());
            }
        }
    }
    let wake = Arc::new(tokio::sync::Notify::new());
    let drain = Arc::new(tokio::sync::Mutex::new(()));
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
        match evt.get("kind").and_then(Value::as_str) {
            Some(kind @ ("added" | "updated")) => {
                if !mark_persist(&k_sub, &d_sub, &aid, kind == "added") {
                    return;
                }
            }
            Some("removed") => {
                let mut d = d_sub.lock().expect("dirty poisoned");
                let Some(path) = d.paths.remove(&aid) else {
                    return;
                };
                d.persist.retain(|i| i != &aid);
                // Added and removed between two drains: no record was ever
                // written, so there is no dir to remove.
                if d.unwritten.remove(&aid) {
                    return;
                }
                d.forget.push((aid, path));
            }
            _ => return,
        }
        w_sub.notify_one();
    }));
    let k_run = Arc::clone(kernel);
    let d_run = Arc::clone(&dirty);
    let w_run = Arc::clone(&wake);
    let dr_run = Arc::clone(&drain);
    let l_run = loader_id.clone();
    let task = kernel.spawn_keepalive_task(loader_id, "persist_writer", async move {
        loop {
            w_run.notified().await;
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            flush_records(&k_run, &l_run, &d_run, &dr_run).await;
        }
    });
    let writer = PersistWriter {
        loader: loader_id.clone(),
        dirty,
        drain,
        wake,
        token,
        task,
    };
    // A ONE-SHOT DRAINS SYNCHRONOUSLY. python's `call` mode boots the root and
    // then awaits `root._fs_flush_loop.aflush()` before the process exits — the
    // debounce never ticks in that window. The kernel keeps the ROOT loader's
    // handle so a caller with no reference to the loader bundle can do the same;
    // another watching loader's writer is its own.
    if kernel.root().is_some_and(|r| r.id == *loader_id) {
        kernel.set_record_writer(writer.clone());
    }
    writer
}

async fn flush_records(
    kernel: &Arc<Kernel>,
    loader_id: &AgentId,
    dirty: &Arc<std::sync::Mutex<Dirty>>,
    drain: &tokio::sync::Mutex<()>,
) {
    // ONE DRAIN AT A TIME, held across the writes: a drain that took the queues
    // is finished before the next one looks at them.
    let _drain = drain.lock().await;
    // THE LOADER'S OWN STORE (python `_find_store(self.agent)`). NO PROVIDER ⇒
    // the queues stay queued (python returns before clearing).
    let Some(loader) = kernel.agents.get(loader_id).map(|e| Arc::clone(&e)) else {
        return;
    };
    if find_store_for(kernel, &loader).is_none() {
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
        dirty.lock().expect("dirty poisoned").unwritten.remove(&aid);
        // The record only — a bundle's readme and docs are served from the
        // bundle, never copied.
        let _ = persist(kernel, &loader, &agent).await;
    }
    for (_, path) in forget {
        forget_path(kernel, &loader, &path).await;
    }
}

/// What the ROOT LOADER's `boot` installed — the handles a shutdown needs back.
///
/// The MACHINERY lives here, in the kernel, because both the shipped
/// `kernel_state.tools` bundle and the kernel's own test double drive exactly
/// the same sequence; the bundle owns the VERB SURFACE, not a second copy of
/// this. Python keeps it on the loader agent (`_fs_flush_loop`).
#[derive(Default)]
pub struct RootLoaderState {
    /// The debounced record writer, when this loader watches (`watch != false`).
    pub writer: Option<PersistWriter>,
}

/// The loader's `boot`, python `kernel_state._boot`:
///
/// 1. Unless `watch=false` (a loader serving a REMOTE kernel answers verbs
///    only) — subscribe the record writer to the kernel state stream.
/// 2. ROOT only — mint the instance `guid` ONCE, AFTER the writer subscribes,
///    so the `updated` event it publishes is what carries the new record to
///    disk. An explicit or already-persisted guid always wins.
///
/// There is no message pump and no hydration: the kernel keeps no conversation,
/// and its execution rows are flushed by the kernel itself into the root's memory
/// store (`kernel_start`).
pub async fn boot_root_loader(kernel: &Arc<Kernel>, agent: &Arc<Agent>) -> RootLoaderState {
    let mut state = RootLoaderState::default();
    let is_root = agent.parent_id.is_none();
    let watching = agent
        .meta
        .read()
        .ok()
        .and_then(|m| m.get("watch").map(|v| v != &Value::Bool(false)))
        .unwrap_or(true);
    if !watching {
        return state;
    }
    // python runs `_check_store_allow(agent)` for EVERY watching loader.
    check_store_allow(kernel, agent);
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
            kernel.update(&agent.id, patch);
        }
    }
    state
}

/// Unsubscribe and stop the record loop — python `_FlushLoop.stop`. The WRITING
/// happened in `flush_pending`, before the walk.
pub async fn shutdown_root_loader(state: RootLoaderState, kernel: &Arc<Kernel>) {
    if let Some(w) = state.writer {
        w.stop(kernel).await;
    }
}

pub mod blob;
pub use blob::{load_blob, persist_blob};
