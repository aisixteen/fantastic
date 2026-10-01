//! Bootstrap — the canonical "wake a kernel" entry.
//!
//! Every caller (CLI daemon, one-shot RPC, embedded library,
//! integration tests) goes through here so the boot sequence stays
//! identical:
//!
//! - **Disk mode** ([`StorageMode::Disk`]):
//!   1. Acquire `.fantastic/lock.yaml` (PID-keyed; stale → overwritten).
//!   2. Read the tree — `.fantastic/agent.yaml` (the root record) and every
//!      record under its children dir — into a flat record list; seed a root
//!      on a virgin workdir (or over a missing/corrupt root record).
//!   3. [`Kernel::load_records`] it, python's `kernel.load(read_tree(…))`:
//!      VALIDATED first (a bad id, a reserved key, a duplicate, an orphan
//!      refuses the boot), then built (weak-load: an unknown `handler_module`
//!      is logged + skipped with its subtree).
//! - **InMemory mode** ([`StorageMode::InMemory`]):
//!   1. No lock, no fs I/O.
//!   2. Create the root agent in process memory only.
//!
//! Both modes leave the kernel in the same logical state: root is
//! registered, [`Kernel::root`] points at it, [`Kernel::agents`]
//! contains every loaded agent.
//!
//! The caller supplies the [`BundleRegistry`] BEFORE calling
//! [`bootstrap`] — that's where the runtime decides which bundles
//! ship in this binary.
//!
//! **Dirty-binding contract**: on Disk mode the on-disk tree
//! (`.fantastic/agents/<id>/agent.yaml` + sidecars) is loosely
//! coupled to the in-RAM [`crate::state::KernelState`]. Persistence
//! merges kernel-managed fields into existing `agent.yaml` files
//! (see [`crate::persistence::persist`]) — never wholesale
//! overwrites, never touches sidecar files. The in-RAM state can
//! drift from disk; bundles reconcile their own slices on next
//! touch.

use crate::agent::{Agent, AgentId};
use crate::bundle::BundleRegistry;
use crate::errors::{KernelError, KernelResult};
use crate::kernel::Kernel;
use crate::storage::StorageMode;
use crate::{lock, persistence};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Default id for the root agent — python's `kernel_state`, which IS the tree
/// root and carries the loader's own `handler_module`.
pub const DEFAULT_ROOT_ID: &str = "kernel_state";

/// The root's `handler_module`: the durable-state loader bundle. A root record
/// is seeded with it on a virgin dir, exactly as python's `_bootstrap` does.
pub const ROOT_HANDLER_MODULE: &str = "kernel_state.tools";

/// Options for [`bootstrap`].
#[derive(Debug, Clone)]
pub struct BootstrapOptions {
    /// State medium. See [`StorageMode`].
    pub storage: StorageMode,
    /// Whether to acquire the PID lock. Ignored in
    /// [`StorageMode::InMemory`] (no lock without a workdir).
    pub acquire_lock: bool,
    /// Override the root agent's id. Default `"kernel_state"`.
    pub root_id: String,
}

impl BootstrapOptions {
    /// Disk-backed daemon bootstrap (acquires lock; root id `"kernel_state"`).
    pub fn daemon(workdir: impl Into<PathBuf>) -> Self {
        Self {
            storage: StorageMode::Disk(workdir.into()),
            acquire_lock: true,
            root_id: DEFAULT_ROOT_ID.to_string(),
        }
    }

    /// Disk-backed one-shot read (no lock).
    pub fn one_shot(workdir: impl Into<PathBuf>) -> Self {
        Self {
            storage: StorageMode::Disk(workdir.into()),
            acquire_lock: false,
            root_id: DEFAULT_ROOT_ID.to_string(),
        }
    }

    /// In-memory bootstrap — no filesystem I/O, no lock. Used by the
    /// embedding app's "brain" kernel and by tests that don't want to
    /// touch disk. The kernel still answers [`Kernel::save`] /
    /// [`Kernel::load`]; the consumer drives any persistence
    /// externally.
    pub fn in_memory() -> Self {
        Self {
            storage: StorageMode::InMemory,
            acquire_lock: false,
            root_id: DEFAULT_ROOT_ID.to_string(),
        }
    }
}

/// Outcome of a successful [`bootstrap`].
pub struct BootedKernel {
    /// The live kernel (root set, agents hydrated).
    pub kernel: Arc<Kernel>,
    /// Ids of every agent loaded during hydration (excluding root).
    /// Useful for boot-time `boot` verb dispatch.
    pub loaded: Vec<AgentId>,
}

/// Drives the boot sequence. Caller fills `bundles` first.
pub fn bootstrap(bundles: BundleRegistry, opts: BootstrapOptions) -> KernelResult<BootedKernel> {
    let mut kernel = Kernel::with_storage(opts.storage.clone());
    kernel.bundles = bundles;
    let kernel = Arc::new(kernel);

    match &opts.storage {
        StorageMode::Disk(workdir) => boot_disk(&kernel, workdir, &opts),
        StorageMode::InMemory => boot_in_memory(&kernel, &opts.root_id),
    }
}

fn boot_disk(
    kernel: &Arc<Kernel>,
    workdir: &Path,
    opts: &BootstrapOptions,
) -> KernelResult<BootedKernel> {
    let fantastic_dir = workdir.join(".fantastic");
    std::fs::create_dir_all(&fantastic_dir).map_err(|e| KernelError::Persistence {
        path: fantastic_dir.clone(),
        source: e,
    })?;
    if opts.acquire_lock {
        lock::acquire(workdir)?;
    }

    // THE TREE, python `_bootstrap`: `records = read_tree(.fantastic)`. A
    // missing or unparseable root `agent.yaml` does not hide the children under
    // `agents/` (python `read_tree`, 2026-09-20) — they are parented to the root
    // id the seed below mints.
    let mut records = read_tree(&fantastic_dir, &opts.root_id);
    let seeded = !records
        .iter()
        .any(|r| r.get("parent_id").is_none_or(Value::is_null));
    if seeded {
        let mut seed = Map::new();
        seed.insert("id".to_string(), Value::from(opts.root_id.clone()));
        seed.insert(
            "handler_module".to_string(),
            Value::from(ROOT_HANDLER_MODULE),
        );
        records.insert(0, seed);
    }
    // `kernel.load(records)`: VALIDATED before anything is built — a record with
    // a bad id or a reserved key, a duplicate, an orphan refuses the boot. The
    // kernel keeps no conversation and hydrates no rows: the load restores the
    // tree and the id counter (the root record's reserved bound, `next_id`).
    let root_hm = records
        .iter()
        .find(|r| r.get("parent_id").is_none_or(Value::is_null))
        .and_then(|r| r.get("handler_module"))
        .map(fantastic_bundle::py_str)
        .unwrap_or_default();
    kernel.load_records(records)?;
    // The root weak-loads like any record: a runtime without its loader bundle
    // has no tree to run. Said, not papered over with a root it cannot serve.
    let Some(root) = kernel.root() else {
        return Err(KernelError::InvalidSnapshot(format!(
            "the root's bundle {root_hm} is not installed in this runtime"
        )));
    };

    // THE VIRGIN SEED IS THE CONSTRUCTED ROOT, not the bare literal: the record
    // on disk then carries the structural grant baseline `Agent::new` stamps, in
    // its order (`can_call, callable_by`), and the loader's `boot` appends
    // `guid` after them. COLD primitive — this runs before any agent (and thus
    // any `file_bridge` store provider) exists, so it cannot route through one.
    if seeded && opts.storage.is_disk() {
        persistence::write_record_at(
            &fantastic_dir.join(persistence::RECORD_FILE),
            &root.record(),
        )?;
    }
    let loaded: Vec<AgentId> = kernel
        .agent_ids()
        .into_iter()
        .filter(|id| *id != root.id)
        .collect();
    Ok(BootedKernel {
        kernel: Arc::clone(kernel),
        loaded,
    })
}

/// python `kernel_state.read_tree` for the bootstrap's own `.fantastic`: the root
/// record (absent / unreadable / not a mapping ⇒ none) and every record under its
/// children dir, flat, `parent_id` from DISK POSITION. Corrupt siblings skip.
fn read_tree(fantastic_dir: &Path, root_id: &str) -> Vec<Map<String, Value>> {
    let root_rec = std::fs::read_to_string(fantastic_dir.join(persistence::RECORD_FILE))
        .ok()
        .and_then(|raw| persistence::parse_record(&raw));
    let mut out: Vec<Value> = Vec::new();
    match root_rec {
        None => persistence::walk_tree(
            fantastic_dir,
            crate::kernel::DEFAULT_CHILDREN_DIR,
            &Value::from(root_id),
            &mut out,
            crate::kernel::DEFAULT_CHILDREN_DIR,
        ),
        Some(rec) => {
            let cd = rec
                .get("children_dir")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(crate::kernel::DEFAULT_CHILDREN_DIR)
                .to_string();
            let id = rec.get("id").cloned().unwrap_or(Value::Null);
            out.push(Value::Object(rec));
            persistence::walk_tree(fantastic_dir, &cd, &id, &mut out, &cd);
        }
    }
    out.into_iter()
        .filter_map(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .collect()
}

fn boot_in_memory(kernel: &Arc<Kernel>, root_id: &str) -> KernelResult<BootedKernel> {
    // InMemory mode never touches the filesystem. The root agent's
    // `root_path` is a sentinel empty PathBuf — bundles that try to
    // write sidecars in InMemory mode will get an fs error from
    // their own code, which is correct (they shouldn't be writing
    // sidecars in a brain-kernel context).
    // NO GUID HERE. The instance guid is minted ONCE by the root loader's `boot`
    // (python `kernel_state._boot`), so an in-memory kernel gets one the same
    // way a disk one does — and never two different ones.
    let root = Agent::new(
        AgentId(root_id.to_string()),
        Some(ROOT_HANDLER_MODULE.to_string()),
        None,
        Map::new(),
        PathBuf::new(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    Ok(BootedKernel {
        kernel: Arc::clone(kernel),
        loaded: Vec::new(),
    })
}

/// Release the lock the bootstrap call acquired. Idempotent.
/// No-op on a workdir that was never used in Disk mode.
pub fn shutdown(workdir: &Path) -> KernelResult<()> {
    lock::release(workdir)
}

#[cfg(test)]
mod tests;
