//! Bootstrap — the canonical "wake a kernel" entry.
//!
//! Every caller (CLI daemon, one-shot RPC, embedded library,
//! integration tests) goes through here so the boot sequence stays
//! identical:
//!
//! - **Disk mode** ([`StorageMode::Disk`]):
//!   1. Acquire `.fantastic/lock.json` (PID-keyed; stale → overwritten).
//!   2. Read `.fantastic/agent.json` (the root record); auto-create
//!      on a virgin workdir.
//!   3. Hydrate `<workdir>/.fantastic/agents/**` via
//!      [`crate::persistence::load_children`] (weak-load: unknown
//!      `handler_module` is logged + skipped).
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
//! (`.fantastic/agents/<id>/agent.json` + sidecars) is loosely
//! coupled to the in-RAM [`crate::state::KernelState`]. Persistence
//! merges kernel-managed fields into existing `agent.json` files
//! (see [`crate::persistence::persist`]) — never wholesale
//! overwrites, never touches sidecar files. The in-RAM state can
//! drift from disk; bundles reconcile their own slices on next
//! touch.

use crate::agent::{Agent, AgentId, AgentRecord};
use crate::bundle::BundleRegistry;
use crate::errors::{KernelError, KernelResult};
use crate::kernel::Kernel;
use crate::storage::StorageMode;
use crate::{lock, persistence};
use serde_json::Map;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Default id for the root agent.
pub const DEFAULT_ROOT_ID: &str = "core";

/// Options for [`bootstrap`].
#[derive(Debug, Clone)]
pub struct BootstrapOptions {
    /// State medium. See [`StorageMode`].
    pub storage: StorageMode,
    /// Whether to acquire the PID lock. Ignored in
    /// [`StorageMode::InMemory`] (no lock without a workdir).
    pub acquire_lock: bool,
    /// Override the root agent's id. Default `"core"`.
    pub root_id: String,
}

impl BootstrapOptions {
    /// Disk-backed daemon bootstrap (acquires lock; root id `"core"`).
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

    // Root record. Auto-create if absent — matches Python's behaviour
    // (fantastic on a virgin dir writes the bootstrap record).
    let root_record_path = fantastic_dir.join("agent.json");
    let root_rec: AgentRecord = match read_record_at(&root_record_path)? {
        Some(rec) => rec,
        None => {
            let rec = AgentRecord {
                id: opts.root_id.clone(),
                handler_module: None,
                parent_id: None,
                meta: Map::new(),
            };
            // Use the merge-persist path so a pre-existing `.fantastic/agent.json`
            // with extra fields (some other tool's keys) is preserved.
            let temp_root = Agent::new(
                AgentId(rec.id.clone()),
                None,
                None,
                rec.meta.clone(),
                fantastic_dir.clone(),
                false,
            );
            persistence::persist(&temp_root, &opts.storage)?;
            rec
        }
    };

    // Construct the root agent — its on-disk dir is `.fantastic/`
    // (children live under `.fantastic/agents/<id>/`).
    let root = Agent::new(
        AgentId(root_rec.id.clone()),
        root_rec.handler_module.clone(),
        None,
        root_rec.meta.clone(),
        fantastic_dir.clone(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    let loaded = persistence::load_children(kernel, &kernel.bundles, Arc::clone(&root))?;
    Ok(BootedKernel {
        kernel: Arc::clone(kernel),
        loaded,
    })
}

fn boot_in_memory(kernel: &Arc<Kernel>, root_id: &str) -> KernelResult<BootedKernel> {
    // InMemory mode never touches the filesystem. The root agent's
    // `root_path` is a sentinel empty PathBuf — bundles that try to
    // write sidecars in InMemory mode will get an fs error from
    // their own code, which is correct (they shouldn't be writing
    // sidecars in a brain-kernel context).
    let root = Agent::new(
        AgentId(root_id.to_string()),
        None,
        None,
        Map::new(),
        PathBuf::new(),
        false,
    );
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    Ok(BootedKernel {
        kernel: Arc::clone(kernel),
        loaded: Vec::new(),
    })
}

/// Read an [`AgentRecord`] from `path`. Returns `None` if the file
/// doesn't exist; an error if it exists but is unreadable or
/// malformed.
fn read_record_at(path: &Path) -> KernelResult<Option<AgentRecord>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path).map_err(|e| KernelError::Persistence {
        path: path.to_path_buf(),
        source: e,
    })?;
    let rec: AgentRecord = serde_json::from_str(&raw).map_err(|e| KernelError::CorruptRecord {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(Some(rec))
}

/// Release the lock the bootstrap call acquired. Idempotent.
/// No-op on a workdir that was never used in Disk mode.
pub fn shutdown(workdir: &Path) -> KernelResult<()> {
    lock::release(workdir)
}

#[cfg(test)]
mod tests;
